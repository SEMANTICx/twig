use std::io::{BufRead,BufReader,Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration,Instant};
use serde_json::{json,Value};
use twig::{Daemon,config::HarnessConfig,stub::{StubLlm,call}};

fn session(command:&str, action:&str)->Vec<Value>{
    let dir=std::env::temp_dir().join(format!("twig-interaction-{}-{}",std::process::id(),action));
    std::fs::create_dir_all(&dir).unwrap();
    let llm=StubLlm::new(vec![StubLlm::with_tools("",vec![call("one","bash",json!({"command":command}))]),StubLlm::text("finished")]);
    let (mut input, server_input)=UnixStream::pair().unwrap();
    let (server_output, output)=UnixStream::pair().unwrap();
    output.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut reader=BufReader::new(output);
    std::thread::scope(|s|{
        s.spawn(||{
            let mut daemon=Daemon::new(&llm,&dir,dir.join("runs"),action,&HarnessConfig::default()).unwrap();
            daemon.serve(BufReader::new(server_input),server_output).unwrap();
        });
        writeln!(input,"{}",json!({"id":1,"method":"turn","params":{"input":"do it"}})).unwrap();
        let mut events=Vec::new();let mut acted=false;
        loop{
            let mut line=String::new();reader.read_line(&mut line).unwrap();
            let value:Value=serde_json::from_str(&line).unwrap();
            if !acted && ((action=="approval" && value["ev"]=="approval.requested")
                || (action=="cancel" && value["ev"]=="tool.output")){
                let request=if action=="approval"{json!({"id":2,"method":"approval.resolve","params":{"id":"one","allow":true}})}
                    else{json!({"id":2,"method":"cancel"})};
                writeln!(input,"{request}").unwrap();acted=true;
            }
            let done=value["id"]==1 && (value.get("result").is_some()||value.get("error").is_some());
            events.push(value);if done{break;}
        }
        drop(input);
        events
    })
}
#[test]
fn output_is_live_and_cancel_terminates_running_shell(){
    let start=Instant::now();
    let events=session("printf started; sleep 30; printf SHOULD_NOT_RUN","cancel");
    assert!(start.elapsed()<Duration::from_secs(3));
    assert!(events.iter().any(|e|e["ev"]=="tool.output" && e["text"].as_str().unwrap_or("").contains("started")));
    assert!(!events.iter().any(|e|e["text"].as_str().unwrap_or("").contains("SHOULD_NOT_RUN")));
}
#[test]
fn approval_waits_then_runs_the_exact_command_once(){
    let events=session("printf 'git reset --hard'","approval");
    assert_eq!(events.iter().filter(|e|e["ev"]=="approval.requested").count(),1);
    assert_eq!(events.iter().filter(|e|e["ev"]=="tool.started").count(),1);
    assert!(events.iter().any(|e|e["ev"]=="tool.output" && e["text"]=="git reset --hard"));
}
