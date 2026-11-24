use serde_json::json;
use twig::{Daemon,Record,config::HarnessConfig,stub::StubLlm};
#[test]
fn pending_approval_survives_restart_and_owns_the_session(){
 let root=std::env::temp_dir().join(format!("twig-recovery-{}",std::process::id()));
 std::fs::create_dir_all(&root).unwrap();let runs=root.join("runs");
 let call=json!({"id":"pending","name":"bash","args":{"command":"printf done > result"}});
 {
  let mut r=Record::open(runs.join("recover.jsonl"),"recover").unwrap();
  r.append("messages.checkpoint",json!({"messages":[{"role":"assistant","tool_calls":[{"id":"pending","function":{"name":"bash","arguments":"{}"}}]}],"pending_call":call})).unwrap();
 }
 let model=StubLlm::new(vec![]);
 let mut daemon=Daemon::new(&model,&root,&runs,"recover",&HarnessConfig::default()).unwrap();
 assert!(!root.join("result").exists());
 assert!(Daemon::new(&model,&root,&runs,"recover",&HarnessConfig::default()).is_err());
 let pending=daemon.dispatch("approval.list",&json!({}),&mut |_|{}).unwrap();assert_eq!(pending["pending"].as_array().unwrap().len(),1);
 daemon.dispatch("approval.resolve",&json!({"id":"pending","allow":true}),&mut |_|{}).unwrap();
 assert_eq!(std::fs::read_to_string(root.join("result")).unwrap(),"done");
 assert!(daemon.dispatch("approval.resolve",&json!({"id":"pending","allow":true}),&mut |_|{}).is_err());
 drop(daemon);
 let mut resumed=Daemon::new(&model,&root,&runs,"recover",&HarnessConfig::default()).unwrap();
 assert_eq!(resumed.dispatch("approval.list",&json!({}),&mut |_|{}).unwrap()["pending"],json!([]));
}
