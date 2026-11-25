use std::path::{Path, PathBuf};
use std::process::Command;
use twig::automation::{Automation, AutomationConfig, Review, ShippingPermissions, SourceKind, TaskExecutor, WorkItem};

fn temp() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path=std::env::temp_dir().join(format!("twig-automation-{}-{}",std::process::id(),NEXT.fetch_add(1,std::sync::atomic::Ordering::Relaxed)));
    std::fs::create_dir_all(&path).unwrap(); path
}
fn git(cwd:&Path,args:&[&str]) { let out=Command::new("git").arg("-C").arg(cwd).args(args).output().unwrap(); assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr)); }
fn repo(path:&Path) {
    std::fs::create_dir_all(path).unwrap(); git(path,&["init","-q"]); git(path,&["config","user.email","twig@example.invalid"]); git(path,&["config","user.name","Twig Tests"]);
    std::fs::write(path.join("README"),"start\n").unwrap(); git(path,&["add","README"]); git(path,&["commit","-qm","initial"]);
}
fn config(root:&Path)->AutomationConfig {
    AutomationConfig { repository:"owner/repo".into(),repository_path:root.join("repo"),state_dir:root.join("state"),label:"twig".into(),poll_interval_secs:1,include_pull_requests:true,check_command:"test -f answer.txt".into(),check_timeout_secs:3,lease_secs:30,permissions:ShippingPermissions::default(),reply:None,create_pr:false,merge:false,release_tag:None,git_path:"git".into(),gh_path:"gh".into() }
}
fn item(kind:SourceKind)->WorkItem { WorkItem::new("owner/repo",kind,42,"title","body","https://example.invalid/42") }
struct Executor { reviewed:usize }
impl TaskExecutor for Executor {
    fn execute(&mut self,_:&WorkItem,worktree:&Path,_:Option<&str>)->Result<String,String> { std::fs::write(worktree.join("answer.txt"),"ok\n").map_err(|e|e.to_string())?; Ok("done".into()) }
    fn verify(&mut self,_:&WorkItem,_:&Path)->Result<String,String> { Ok("verified".into()) }
    fn review(&mut self,_:&WorkItem,diff:&str,evidence:&str)->Result<Review,String> { self.reviewed+=1; Ok(Review{approved:diff.contains("answer.txt")&&evidence.contains("exit: Some(0)"),summary:"reviewed actual diff".into()}) }
}

#[test]
fn source_ids_ignore_mutable_content_and_separate_kind() {
    assert_eq!(item(SourceKind::Issue).id,"github:owner/repo:issue:42");
    assert_ne!(item(SourceKind::Issue).id,item(SourceKind::PullRequest).id);
    let renamed=WorkItem::new("owner/repo",SourceKind::Issue,42,"changed","other","url");
    assert_eq!(item(SourceKind::Issue).id,renamed.id);
}

#[test]
fn permission_defaults_and_config_validation_prevent_accidental_shipping() {
    let p=ShippingPermissions::default(); assert!(!p.replies&&!p.push&&!p.create_pr&&!p.merge&&!p.release);
    let c=AutomationConfig{repository:"owner/repo".into(),check_command:"true".into(),create_pr:true,
        permissions:ShippingPermissions{create_pr:true,..Default::default()},..Default::default()};
    assert!(c.validate().unwrap_err().contains("push"));
}

#[test]
fn work_is_verified_reviewed_and_committed_in_a_real_worktree_then_recovers_done() {
    let root=temp(); repo(&root.join("repo")); let mut automation=Automation::open(config(&root)).unwrap();
    let task=item(SourceKind::Issue); assert_eq!(automation.ingest(vec![task.clone(),task.clone()]).unwrap(),1);
    let mut executor=Executor{reviewed:0};
    assert_eq!(automation.run_once(&mut executor).unwrap(),vec![task.id.clone()]); assert_eq!(executor.reviewed,1);
    let wt=root.join("state/worktrees").join(blake3::hash(task.id.as_bytes()).to_hex().to_string());
    assert!(Command::new("git").arg("-C").arg(&wt).args(["rev-parse","--is-inside-work-tree"]).output().unwrap().status.success());
    let log=Command::new("git").arg("-C").arg(&wt).args(["log","-1","--format=%s"]).output().unwrap();
    assert!(log.status.success(),"{}",String::from_utf8_lossy(&log.stderr));
    let status=Command::new("git").arg("-C").arg(&wt).args(["status","--short"]).output().unwrap();
    assert!(log.stdout.starts_with(b"twig: issue #42"),"subject={:?}; status={:?}",String::from_utf8_lossy(&log.stdout),String::from_utf8_lossy(&status.stdout));
    assert!(automation.run_once(&mut executor).unwrap().is_empty()); assert_eq!(executor.reviewed,1);
    assert!(root.join("state/reports").read_dir().unwrap().next().is_some());
}

#[test]
fn pull_request_ingestion_can_be_disabled_explicitly() {
    let root=temp(); repo(&root.join("repo")); let mut c=config(&root); c.include_pull_requests=false;
    let mut automation=Automation::open(c).unwrap(); assert_eq!(automation.ingest(vec![item(SourceKind::PullRequest)]).unwrap(),0);
}

#[test]
fn failed_check_never_commits_or_reviews() {
    let root=temp(); repo(&root.join("repo")); let mut c=config(&root); c.check_command="false".into();
    let mut automation=Automation::open(c).unwrap(); let task=item(SourceKind::Issue); automation.ingest(vec![task.clone()]).unwrap();
    let mut executor=Executor{reviewed:0}; assert!(automation.run_once(&mut executor).unwrap().is_empty()); assert_eq!(executor.reviewed,0);
    let wt=root.join("state/worktrees").join(blake3::hash(task.id.as_bytes()).to_hex().to_string());
    assert_eq!(String::from_utf8(Command::new("git").arg("-C").arg(wt).args(["rev-list","--count","HEAD"]).output().unwrap().stdout).unwrap().trim(),"1");
}

#[test]
fn pull_request_ids_are_canonical_across_restart() {
    let root=temp(); repo(&root.join("repo")); let task=item(SourceKind::PullRequest);
    { let mut a=Automation::open(config(&root)).unwrap(); assert_eq!(a.ingest(vec![task.clone()]).unwrap(),1); }
    let mut a=Automation::open(config(&root)).unwrap(); assert_eq!(a.ingest(vec![task]).unwrap(),0);
}
#[test]
fn modified_queue_event_is_rejected_before_resume() {
    let root=temp(); repo(&root.join("repo"));
    let mut automation=Automation::open(config(&root)).unwrap();
    automation.ingest(vec![item(SourceKind::Issue)]).unwrap();
    drop(automation);
    let queue=root.join("state/queue.jsonl");
    let text=std::fs::read_to_string(&queue).unwrap();
    let altered=text.replace("\"title\":\"title\"","\"title\":\"tampered\"");
    assert_ne!(text,altered);
    std::fs::write(queue,altered).unwrap();
    assert!(Automation::open(config(&root)).is_err());
}
#[test]
fn unattended_check_runs_in_isolation() {
    let root=temp(); repo(&root.join("repo"));
    let secret=root.join("host-secret"); std::fs::write(&secret,"unchanged").unwrap();
    let mut c=config(&root);
    c.check_command=format!("test ! -e '{}' && test -f answer.txt",secret.display());
    let mut automation=Automation::open(c).unwrap();
    let task=item(SourceKind::Issue); automation.ingest(vec![task.clone()]).unwrap();
    let mut executor=Executor{reviewed:0};
    assert_eq!(automation.run_once(&mut executor).unwrap(),vec![task.id]);
    assert_eq!(std::fs::read_to_string(secret).unwrap(),"unchanged");
    assert_eq!(executor.reviewed,1);
}
#[test]
fn legacy_queue_is_backed_up_before_hash_chain_migration() {
    let root=temp(); repo(&root.join("repo"));
    let state=root.join("state"); std::fs::create_dir_all(&state).unwrap();
    let queue=state.join("queue.jsonl");
    let legacy=r#"{"event":"queued","at":1,"item":{"id":"legacy"}}"#;
    std::fs::write(&queue,format!("{legacy}\n")).unwrap();
    let backup=queue.with_extension("legacy.jsonl");
    let _automation=Automation::open(config(&root)).unwrap();
    assert_eq!(std::fs::read_to_string(backup).unwrap(),format!("{legacy}\n"));
    assert!(std::fs::read_to_string(queue).unwrap().contains("twig.automation.event.v1"));
}
