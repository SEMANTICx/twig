use serde_json::json;
use twig::{Daemon,Record,config::HarnessConfig,stub::StubLlm,Llm};
#[test]
fn interrupted_approval_is_unknown_and_never_replayed_by_new_input(){
 let root=std::env::temp_dir().join(format!("twig-recovery-{}",std::process::id()));
 std::fs::create_dir_all(&root).unwrap();let runs=root.join("runs");let script="printf executed > result";
 let call=json!({"id":"pending","name":"bash","args":{"command":script}});
 {
  let mut r=Record::open(runs.join("recover.jsonl"),"recover").unwrap();
  let assistant=json!({"role":"assistant","content":"","tool_calls":[{"id":"pending","type":"function","function":{"name":"bash","arguments":json!({"command":script}).to_string()}}]});
  r.append("message",json!({"message":assistant})).unwrap();
  r.append("approval.requested",json!({"id":"pending","name":"bash","args":call["args"]})).unwrap();
  r.append("messages.checkpoint",json!({"messages":[assistant],"pending_call":call})).unwrap();
 }
 let model=StubLlm::new(vec![StubLlm::text("must not execute")]);
 let mut daemon=Daemon::new(&model,&root,&runs,"recover",&HarnessConfig::default()).unwrap();
 let list=daemon.dispatch("approval.list",&json!({}),&mut |_|{}).unwrap();
 assert_eq!(list["pending"][0]["id"],"pending");
 assert!(daemon.dispatch("turn",&json!({"input":"continue"}),&mut |_|{}).is_err());
 assert!(!root.join("result").exists());assert_eq!(model.calls(),0);
 let resolved=daemon.dispatch("approval.resolve",&json!({"id":"pending","allow":true,"replayed":false}),&mut |_|{}).unwrap();
 assert_eq!(resolved["replayed"],false);assert!(!root.join("result").exists());assert_eq!(model.calls(),0);
 assert_eq!(daemon.dispatch("approval.list",&json!({}),&mut |_|{}).unwrap()["pending"],json!([]));
 drop(daemon);
 let mut resumed=Daemon::new(&model,&root,&runs,"recover",&HarnessConfig::default()).unwrap();
 assert_eq!(resumed.dispatch("record.verify",&json!({}),&mut |_|{}).unwrap()["ok"],true);
}
#[test]
fn post_checkpoint_messages_restore_once() {
 let root=std::env::temp_dir().join(format!("twig-recovery-checkpoint-{}",std::process::id()));
 std::fs::create_dir_all(&root).unwrap();let runs=root.join("runs");
 {
  let mut r=Record::open(runs.join("checkpoint.jsonl"),"checkpoint").unwrap();
  let first=json!({"role":"user","content":"first"});
  r.append("message",json!({"message":first.clone()})).unwrap();
  r.append("messages.checkpoint",json!({"messages":[first]})).unwrap();
  r.append("message",json!({"message":{"role":"user","content":"post-checkpoint"}})).unwrap();
 }
 let model=StubLlm::new(vec![StubLlm::text("done")]);
 let mut daemon=Daemon::new(&model,&root,&runs,"checkpoint",&HarnessConfig::default()).unwrap();
 daemon.dispatch("turn",&json!({"input":"next"}),&mut |_|{}).unwrap();
 let rows=daemon.dispatch("record.tail",&json!({"n":100}),&mut |_|{}).unwrap();
 let checkpoint=rows["rows"].as_array().unwrap().iter().rev()
  .find(|row|row["kind"]=="messages.checkpoint").unwrap();
 assert_eq!(checkpoint["messages"].as_array().unwrap().iter()
  .filter(|message|message["content"]=="post-checkpoint").count(),1);
}
#[test]
fn started_tool_with_unknown_outcome_is_never_replayed() {
 let root=std::env::temp_dir().join(format!("twig-recovery-started-{}",std::process::id()));
 std::fs::create_dir_all(&root).unwrap();let runs=root.join("runs");let script="printf executed > result";
 {
  let mut r=Record::open(runs.join("started.jsonl"),"started").unwrap();
  let assistant=json!({"role":"assistant","content":"","tool_calls":[{"id":"call","type":"function","function":{"name":"bash","arguments":json!({"command":script}).to_string()}}]});
  r.append("message",json!({"message":assistant})).unwrap();
  r.append("tool.started",json!({"id":"call","tool":"bash","args":{"command":script}})).unwrap();
  r.append("messages.checkpoint",json!({"messages":[assistant]})).unwrap();
 }
 let model=StubLlm::new(vec![]);
 let mut daemon=Daemon::new(&model,&root,&runs,"started",&HarnessConfig::default()).unwrap();
 daemon.dispatch("turn",&json!({"input":"continue"}),&mut |_|{}).unwrap();
 assert_eq!(model.calls(),1);
 assert!(!root.join("result").exists());
}
