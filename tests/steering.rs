use std::sync::Mutex;
use serde_json::{json,Value};
use twig::{control::Control,provider::{Llm,Roles,Reply},Daemon,config::HarnessConfig};
struct Model{roles:Roles,seen:Mutex<Vec<Vec<Value>>>}
impl Llm for Model{
 fn roles(&self)->&Roles{&self.roles} fn calls(&self)->u64{0}
 fn complete(&self,_:&str,_:&str,m:&[Value],_:Option<&Value>)->Result<Reply,String>{
  self.seen.lock().unwrap().push(m.to_vec());Ok(Reply{text:"done".into(),calls:vec![],usage:Default::default()})}
 fn stream(&self,model:&str,system:&str,m:&[Value],t:Option<&Value>,c:&Control,_:&mut dyn FnMut(Value))->Result<Reply,String>{
  if self.seen.lock().unwrap().is_empty(){c.steer("Preserve the public API".into());}
  self.complete(model,system,m,t)
 }
}
#[test]
fn steering_is_applied_at_next_safe_turn_and_history_is_retained(){
 let d=std::env::temp_dir().join(format!("twig-steering-{}",std::process::id()));std::fs::create_dir_all(&d).unwrap();
 let model=Model{roles:Roles::default(),seen:Mutex::new(vec![])};
 let mut daemon=Daemon::new(&model,&d,d.join("runs"),"s",&HarnessConfig::default()).unwrap();
 daemon.dispatch("turn",&json!({"input":"inspect"}),&mut |_|{}).unwrap();
 daemon.dispatch("turn",&json!({"input":"continue"}),&mut |_|{}).unwrap();
 let seen=model.seen.lock().unwrap();
 assert!(seen[1].iter().any(|m|m["content"]=="Preserve the public API"));
 assert!(seen[1].iter().any(|m|m["role"]=="assistant"&&m["content"]=="done"));
}
