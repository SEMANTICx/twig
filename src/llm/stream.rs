//! OpenAI SSE decoding with bounded per-line reads.
//!
//! Cancellation detaches the HTTP reader immediately; receiver disconnect then
//! closes it on the next transport event or read timeout.

use std::io::{BufRead, Read};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

use super::is_retryable_status;
use super::types::{parse_reply, Reply, Usage};
use crate::control::Control;

const MAX_LINE_BYTES: usize = 1_048_576;
const MAX_STREAM_RETRIES: u32 = 3;

fn read_bounded_line(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> Result<usize, String> {
    buf.clear();
    let mut limited = reader.by_ref().take((MAX_LINE_BYTES + 1) as u64);
    let n = limited.read_until(b'\n', buf).map_err(|e| e.to_string())?;
    if buf.len() > MAX_LINE_BYTES {
        return Err("stream event too large".into());
    }
    Ok(n)
}

fn sleep_cancellable(duration: Duration, control: &Control) -> bool {
    let step = Duration::from_millis(50);
    let mut elapsed = Duration::ZERO;
    while elapsed < duration {
        if control.cancelled() {
            return false;
        }
        let chunk = step.min(duration - elapsed);
        std::thread::sleep(chunk);
        elapsed += chunk;
    }
    !control.cancelled()
}

pub fn complete(
    base: String,
    key: String,
    payload: Value,
    timeout: Duration,
    control: &Control,
    emit: &mut dyn FnMut(Value),
) -> Result<Reply, String> {
    let body_str = payload.to_string();
    let mut attempt = 0u32;
    loop {
        if control.cancelled() {
            return Err("cancelled".into());
        }
        let mut emitted_any = false;
        match stream_once(&base, &key, &body_str, timeout, control, &mut |ev| {
            emitted_any = true;
            emit(ev);
        }) {
            Ok(reply) => return Ok(reply),
            Err(StreamFailure::Retryable(msg)) if !emitted_any && attempt < MAX_STREAM_RETRIES => {
                attempt += 1;
                let backoff = Duration::from_secs(1u64 << (attempt - 1));
                emit(json!({
                    "ev": "model.retry",
                    "attempt": attempt,
                    "backoff_secs": backoff.as_secs(),
                    "reason": msg
                }));
                if !sleep_cancellable(backoff, control) {
                    return Err("cancelled".into());
                }
            }
            Err(StreamFailure::Retryable(msg) | StreamFailure::Fatal(msg)) => return Err(msg),
        }
    }
}

enum StreamFailure {
    Retryable(String),
    Fatal(String),
}

fn stream_once(
    base: &str,
    key: &str,
    body_str: &str,
    timeout: Duration,
    control: &Control,
    emit: &mut dyn FnMut(Value),
) -> Result<Reply, StreamFailure> {
    let (tx, rx) = mpsc::sync_channel::<Result<Value, StreamFailure>>(16);
    let url = format!("{base}/chat/completions");
    let auth = format!("Bearer {key}");
    let body_owned = body_str.to_owned();
    std::thread::spawn(move || {
        let r = ureq::post(&url)
            .set("Content-Type", "application/json")
            .set("Authorization", &auth)
            .timeout(timeout)
            .send_string(&body_owned);
        match r {
            Ok(r)
                if !r
                    .header("content-type")
                    .unwrap_or("")
                    .contains("text/event-stream") =>
            {
                let body = r
                    .into_string()
                    .map_err(|e| StreamFailure::Fatal(e.to_string()))
                    .and_then(|s| {
                        serde_json::from_str(&s).map_err(|e| StreamFailure::Fatal(e.to_string()))
                    });
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
                                        let _ = tx.send(Err(StreamFailure::Fatal(format!(
                                            "stream JSON: {e}"
                                        ))));
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(StreamFailure::Fatal(e)));
                            break;
                        }
                    }
                }
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                let snippet: String = body.chars().take(300).collect();
                let msg = format!("magpie HTTP {code}: {snippet}");
                let err = if is_retryable_status(code) {
                    StreamFailure::Retryable(msg)
                } else {
                    StreamFailure::Fatal(msg)
                };
                let _ = tx.send(Err(err));
            }
            Err(e) => {
                let _ = tx.send(Err(StreamFailure::Retryable(format!("magpie: {e}"))));
            }
        }
    });
    let mut text = String::new();
    let mut calls = std::collections::BTreeMap::<u64, Value>::new();
    let mut usage = Usage::default();
    let mut finished = false;
    loop {
        if control.cancelled() {
            return Err(StreamFailure::Fatal("cancelled".into()));
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(Err(e)) => return Err(e),
            Ok(Ok(v)) => {
                if v.pointer("/choices/0/message").is_some() {
                    let r = parse_reply(&v).map_err(StreamFailure::Fatal)?;
                    if !r.text.is_empty() {
                        emit(json!({"ev": "model.delta", "text": r.text}));
                    }
                    return Ok(r);
                }
                if !v["error"].is_null() {
                    return Err(StreamFailure::Fatal(v["error"].to_string()));
                }
                if !v["usage"].is_null() {
                    usage = Usage::parse(&v["usage"]);
                }
                if !v["choices"][0]["finish_reason"].is_null() {
                    finished = true;
                }
                let delta = &v["choices"][0]["delta"];
                if let Some(s) = delta["reasoning_content"]
                    .as_str()
                    .or_else(|| delta["reasoning"].as_str())
                {
                    if !s.is_empty() {
                        emit(json!({"ev": "model.reasoning", "text": s}));
                    }
                }
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
        return Err(StreamFailure::Retryable(
            "model stream ended before finish_reason".into(),
        ));
    }
    let mut reply = parse_reply(&json!({"choices":[{"message":{"content":text,
        "tool_calls":calls.into_values().collect::<Vec<_>>()}}]}))
    .map_err(StreamFailure::Fatal)?;
    reply.usage = usage;
    Ok(reply)
}
