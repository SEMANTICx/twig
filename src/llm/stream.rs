//! OpenAI SSE decoding with bounded per-line reads.
//!
//! Cancellation detaches the HTTP reader immediately; receiver disconnect then
//! closes it on the next transport event or read timeout.

use std::io::{BufRead, Read};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

use super::types::{parse_reply, Reply, Usage};
use crate::control::Control;

const MAX_LINE_BYTES: usize = 1_048_576;

fn read_bounded_line(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> Result<usize, String> {
    buf.clear();
    let mut limited = reader.by_ref().take((MAX_LINE_BYTES + 1) as u64);
    let n = limited.read_until(b'\n', buf).map_err(|e| e.to_string())?;
    if buf.len() > MAX_LINE_BYTES {
        return Err("stream event too large".into());
    }
    Ok(n)
}

pub fn complete(
    base: String,
    key: String,
    payload: Value,
    timeout: Duration,
    control: &Control,
    emit: &mut dyn FnMut(Value),
) -> Result<Reply, String> {
    let (tx, rx) = mpsc::sync_channel::<Result<Value, String>>(16);
    std::thread::spawn(move || {
        let r = ureq::post(&format!("{base}/chat/completions"))
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {key}"))
            .timeout(timeout)
            .send_string(&payload.to_string());
        match r {
            Ok(r)
                if !r
                    .header("content-type")
                    .unwrap_or("")
                    .contains("text/event-stream") =>
            {
                let body = r
                    .into_string()
                    .map_err(|e| e.to_string())
                    .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()));
                let _ = tx.send(body);
            }
            Ok(r) => {
                let mut reader = std::io::BufReader::new(r.into_reader());
                let mut raw = Vec::new();
                loop {
                    match read_bounded_line(&mut reader, &mut raw) {
                        Ok(0) => break,
                        Ok(_) => {
                            let line = String::from_utf8_lossy(&raw);
                            if let Some(s) = line.trim().strip_prefix("data:") {
                                if s.trim() == "[DONE]" {
                                    break;
                                }
                                match serde_json::from_str(s.trim()) {
                                    Ok(v) => {
                                        if tx.send(Ok(v)).is_err() {
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        let _ = tx.send(Err(format!("stream JSON: {e}")));
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(Err(format!("magpie: {e}")));
            }
        }
    });
    let mut text = String::new();
    let mut calls = std::collections::BTreeMap::<u64, Value>::new();
    let mut usage = Usage::default();
    let mut finished = false;
    loop {
        if control.cancelled() {
            return Err("cancelled".into());
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(Err(e)) => return Err(e),
            Ok(Ok(v)) => {
                if v.pointer("/choices/0/message").is_some() {
                    let r = parse_reply(&v)?;
                    if !r.text.is_empty() {
                        emit(json!({"ev": "model.delta", "text": r.text}));
                    }
                    return Ok(r);
                }
                if !v["error"].is_null() {
                    return Err(v["error"].to_string());
                }
                if !v["usage"].is_null() {
                    usage = Usage::parse(&v["usage"]);
                }
                if !v["choices"][0]["finish_reason"].is_null() {
                    finished = true;
                }
                let delta = &v["choices"][0]["delta"];
                if let Some(s) = delta["content"].as_str() {
                    text.push_str(s);
                    emit(json!({"ev": "model.delta", "text": s}));
                }
                for c in delta["tool_calls"].as_array().into_iter().flatten() {
                    let slot = calls
                        .entry(c["index"].as_u64().unwrap_or(0))
                        .or_insert_with(|| json!({"id":"","function":{"name":"","arguments":""}}));
                    for field in ["id", "function/name", "function/arguments"] {
                        let pointer = format!("/{field}");
                        if let Some(s) = c.pointer(&pointer).and_then(Value::as_str) {
                            let old = slot
                                .pointer(&pointer)
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            *slot.pointer_mut(&pointer).expect("stream slot") = json!(old + s);
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if !finished {
        return Err("model stream ended before finish_reason".into());
    }
    let mut reply = parse_reply(&json!({"choices":[{"message":{"content":text,
        "tool_calls":calls.into_values().collect::<Vec<_>>()}}]}))?;
    reply.usage = usage;
    Ok(reply)
}
