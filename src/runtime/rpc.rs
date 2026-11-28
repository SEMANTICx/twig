//! Bidirectional newline-delimited JSON-RPC transport over stdio (`twig serve`).
//!
//! Decouples immediate control-plane signals (`cancel`, `background`, `steer`,
//! live `approval.resolve`, `ping`) from sequential worker turn execution so
//! user steering and cancellation remain responsive while tools or streams run.

use std::io::{self, BufRead, Write};
use std::sync::{mpsc, Arc, Mutex};

use serde_json::{json, Value};

use crate::control::Control;

fn write_json_line(out: &Mutex<impl Write>, payload: &Value) -> io::Result<()> {
    let mut guard = out.lock().map_err(|_| io::Error::other("output lock"))?;
    writeln!(guard, "{payload}")?;
    guard.flush()
}

fn try_immediate_control(
    control: &Control,
    method: &str,
    params: &Value,
) -> Option<Result<Value, String>> {
    match method {
        "cancel" => {
            control.cancel();
            Some(Ok(json!({"cancelled": true})))
        }
        "background" => {
            control.request_background();
            Some(Ok(json!({"backgrounded": true})))
        }
        "steer" => {
            let text = params["input"].as_str().unwrap_or("");
            if text.trim().is_empty() {
                Some(Err("steer requires nonempty input".into()))
            } else {
                control.steer(text.to_owned());
                Some(Ok(json!({"queued": true})))
            }
        }
        "approval.resolve" if control.waiting().as_deref() == params["id"].as_str() => {
            control.resolve(
                params["id"].as_str().unwrap_or("").into(),
                params["allow"].as_bool().unwrap_or(false),
                params["remember"].as_bool().unwrap_or(false),
                params.get("note").and_then(Value::as_str).map(str::to_owned),
            );
            Some(Ok(json!({"queued": true})))
        }
        "ping" => Some(Ok(json!({"pong": true}))),
        _ => None,
    }
}

pub(super) fn serve_stdio_loop(
    control: Arc<Control>,
    input: impl BufRead + Send,
    out: impl Write + Send,
    mut dispatch: impl FnMut(&str, &Value, &mut dyn FnMut(Value)) -> Result<Value, String> + Send,
) -> io::Result<()> {
    let output = Arc::new(Mutex::new(out));
    let (tx, rx) = mpsc::channel::<Value>();
    let worker_control = control.clone();
    std::thread::scope(|scope| {
        let worker_output = output.clone();
        let worker = scope.spawn(move || -> io::Result<()> {
            for req in rx {
                let id = req["id"].clone();
                let method = req["method"].as_str().unwrap_or("");
                if method == "turn" {
                    worker_control.reset();
                }
                let mut emit = |v: Value| {
                    let _ = write_json_line(&worker_output, &v);
                };
                let result = dispatch(method, &req["params"], &mut emit);
                let reply = match result {
                    Ok(v) => json!({"id": id, "result": v}),
                    Err(e) => json!({"id": id, "error": e}),
                };
                write_json_line(&worker_output, &reply)?;
            }
            Ok(())
        });

        for line in input.lines() {
            let line = line?;
            let req: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    write_json_line(&output, &json!({"error": e.to_string()}))?;
                    continue;
                }
            };
            let method = req["method"].as_str().unwrap_or("");
            let params = &req["params"];
            if let Some(reply) = try_immediate_control(&control, method, params) {
                let line = match reply {
                    Ok(result) => json!({"id": req["id"], "result": result}),
                    Err(error) => json!({"id": req["id"], "error": error}),
                };
                write_json_line(&output, &line)?;
            } else if tx.send(req).is_err() {
                break;
            }
        }
        control.close_input();
        drop(tx);
        worker
            .join()
            .map_err(|_| io::Error::other("session worker panicked"))?
    })
}
