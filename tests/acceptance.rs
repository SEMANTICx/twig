use serde_json::json;
use twig::{Daemon,config::HarnessConfig,stub::StubLlm};
#[test]
fn agent_claim_cannot_bypass_the_project_acceptance_check(){
 let root=std::env::temp_dir().join(format!("twig-acceptance-{}",std::process::id()));
 let _=std::fs::remove_dir_all(&root);
 std::fs::create_dir_all(&root).unwrap();
 let model=StubLlm::new(vec![StubLlm::text("done"),StubLlm::text("done again"),StubLlm::text("still done")]);
 let mut daemon=Daemon::new(&model,&root,root.join("runs"),"acceptance",&HarnessConfig::default()).unwrap();
 let result=daemon.dispatch("turn",&json!({"input":"fix the project","check":{"command":"test -f implemented","timeout_ms":100,"max_output_bytes":1024}}),&mut |_|{}).unwrap();
 assert_eq!(result["reason"],"verify_failed");assert_eq!(result["done"],false);
 drop(daemon);
 std::fs::write(root.join("implemented"),"yes").unwrap();
 let model=StubLlm::new(vec![StubLlm::text("done")]);
 let mut restored=Daemon::new(&model,&root,root.join("runs"),"acceptance",&HarnessConfig::default()).unwrap();
 let result=restored.dispatch("turn",&json!({"input":"verify the implementation"}),&mut |_|{}).unwrap();
 assert_eq!(result["verified"],true);
}
