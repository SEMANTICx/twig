use std::io::{BufRead,BufReader,Read,Write};
use std::net::TcpListener;
use serde_json::json;
use twig::{provider::{Provider,Llm},control::Control};
#[test]
fn sse_text_and_fragmented_tool_arguments_reach_the_caller(){
    let listener=TcpListener::bind("127.0.0.1:0").unwrap();
    let addr=listener.local_addr().unwrap();
    let server=std::thread::spawn(move||{
        let (mut socket,_)=listener.accept().unwrap();
        let mut reader=BufReader::new(socket.try_clone().unwrap());
        let mut len=0;
        loop{let mut line=String::new();reader.read_line(&mut line).unwrap();if line=="\r\n"{break;}
            if let Some(v)=line.to_ascii_lowercase().strip_prefix("content-length:"){len=v.trim().parse::<usize>().unwrap();}}
        let mut body=vec![0;len];reader.read_exact(&mut body).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).unwrap()["stream"],true);
        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
        for v in [json!({"choices":[{"delta":{"content":"hello"}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x","function":{"name":"bash","arguments":"{\"command\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"pwd\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":3}})] {
            writeln!(socket,"data: {v}\n").unwrap();socket.flush().unwrap();
        }
        writeln!(socket,"data: [DONE]\n").unwrap();
    });
    let provider=Provider::with_base(format!("http://{addr}/v1"));let mut events=Vec::new();
    let reply=provider.stream("model","system",&[],None,&Control::default(),&mut |e|events.push(e)).unwrap();
    assert_eq!(reply.text,"hello");assert_eq!(reply.calls[0].args["command"],"pwd");
    assert_eq!(reply.usage.input_tokens,Some(10));
    assert!(events.iter().any(|e|e["ev"]=="model.delta" && e["text"]=="hello"));
    server.join().unwrap();
}
