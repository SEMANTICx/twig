//! OpenAI-compatible client pointed at magpie.
//!
//! magpie owns routing, keys and subscription logins; this owns the loop. One
//! endpoint serving many roles is what makes heterogeneous allocation almost
//! free: triage on a near-free model, code on a strong one, judge on a third.
//!
//! ONE GUARD ONLY: magpie ships a cross-turn loop guard and this harness has its
//! own budget guard. Two guards multiply false positives — a long `write_stdin`
//! wait has already been read as a loop and ended a turn early (magpie #3655).
//!
//! The switch is Settings → Models → Replies → "Stop looping replies"
//! (config key `noLoopGuard`). It is GLOBAL in magpie: turning it off also
//! removes loop protection for every other agent routed through the gateway.
//! Turn it off only if twig is the main client, or run twig against a separate
//! magpie instance.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

pub const DEFAULT_BASE: &str = "http://127.0.0.1:3425/v1";

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub usage: crate::telemetry::Usage,
}

#[derive(Debug, Clone)]
pub struct Roles(pub HashMap<String, String>);

impl Default for Roles {
    fn default() -> Self {
        let mut m = HashMap::new();
        let model = std::env::var("TWIG_MODEL").unwrap_or_else(|_| "aihub/gpt-6.1-sol".into());
        for role in ["filter", "plan", "code", "judge"] { m.insert(role.into(), model.clone()); }
        Roles(m)
    }
}

impl Roles {
    pub fn get(&self, role: &str) -> &str {
        self.0
            .get(role)
            .or_else(|| self.0.get("code"))
            .map(String::as_str)
            .unwrap_or("aihub/gpt-6.1-sol")
    }

    /// `TWIG_ROLES='{"code":"x"}'` overrides individual roles.
    pub fn from_env() -> Self {
        let mut roles = Self::default();
        if let Ok(raw) = std::env::var("TWIG_ROLES") {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) {
                for (k, v) in map {
                    if let Some(s) = v.as_str() {
                        roles.0.insert(k, s.to_string());
                    }
                }
            }
        }
        roles
    }
}

/// Extract a reply from an OpenAI-shaped response body. Kept separate from
/// transport so it can be tested without a network.
pub fn parse_reply(body: &Value) -> Result<Reply, String> {
    let choice = body
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or("response has no choices")?;
    let message = choice.get("message").ok_or("choice has no message")?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut calls = Vec::new();
    for (i, tc) in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .enumerate()
    {
        let function = tc.get("function").ok_or("tool_call has no function")?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or("tool_call has no name")?
            .to_string();
        let args = match function.get("arguments") {
            Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or(json!({ "_raw": raw })),
            Some(v) => v.clone(),
            None => json!({}),
        };
        let id = tc
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{i}"));
        calls.push(ToolCall { id, name, args });
    }
    Ok(Reply { text, calls, usage: crate::telemetry::Usage::parse(&body["usage"]) })
}

pub trait Llm: Sync {
    fn roles(&self) -> &Roles;
    fn calls(&self) -> u64;
    fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: Option<&Value>,
    ) -> Result<Reply, String>;
    fn stream(&self, model: &str, system: &str, messages: &[Value], tools: Option<&Value>,
        control: &crate::control::Control, emit: &mut dyn FnMut(Value)) -> Result<Reply, String> {
        if control.cancelled() { return Err("cancelled".into()); }
        let reply = self.complete(model, system, messages, tools)?;
        if !reply.text.is_empty() { emit(json!({"ev":"model.delta","text":reply.text})); }
        Ok(reply)
    }
}

pub struct Provider {
    base: String,
    key: String,
    roles: Roles,
    calls: AtomicU64,
    timeout: Duration,
}

impl Provider {
    pub fn new() -> Self {
        let base = std::env::var("TWIG_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE.to_string());
        let key = std::env::var("TWIG_API_KEY").unwrap_or_else(|_| "magpie".to_string());
        Self {
            base: base.trim_end_matches('/').to_string(),
            key,
            roles: Roles::from_env(),
            calls: AtomicU64::new(0),
            timeout: Duration::from_secs(600),
        }
    }

    pub fn with_base(base: impl Into<String>) -> Self {
        let mut p = Self::new();
        p.base = base.into().trim_end_matches('/').to_string();
        p
    }

    pub fn base(&self) -> &str {
        &self.base
    }
}

impl Default for Provider {
    fn default() -> Self {
        Self::new()
    }
}

impl Llm for Provider {
    fn roles(&self) -> &Roles {
        &self.roles
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: Option<&Value>,
    ) -> Result<Reply, String> {
        let mut msgs = vec![json!({"role": "system", "content": system})];
        msgs.extend_from_slice(messages);

        let mut payload = json!({
            "model": model,
            "messages": msgs,
            "temperature": 0.0,
        });
        if let Some(tools) = tools {
            payload["tools"] = tools.clone();
            payload["tool_choice"] = json!("auto");
        }

        self.calls.fetch_add(1, Ordering::Relaxed);
        let response = ureq::post(&format!("{}/chat/completions", self.base))
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {}", self.key))
            .timeout(self.timeout)
            .send_string(&payload.to_string());

        let text = match response {
            Ok(r) => r.into_string().map_err(|e| format!("read body: {e}"))?,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                let snippet: String = body.chars().take(400).collect();
                return Err(format!("magpie HTTP {code}: {snippet}"));
            }
            Err(e) => return Err(format!("magpie unreachable at {}: {e}", self.base)),
        };
        let body: Value =
            serde_json::from_str(&text).map_err(|e| format!("magpie returned non-JSON: {e}"))?;
        parse_reply(&body)
    }

    fn stream(&self, model: &str, system: &str, messages: &[Value], tools: Option<&Value>,
        control: &crate::control::Control, emit: &mut dyn FnMut(Value)) -> Result<Reply,String> {
        let mut msgs = vec![json!({"role":"system","content":system})];
        msgs.extend_from_slice(messages);
        let mut payload = json!({"model":model,"messages":msgs,"stream":true,
            "stream_options":{"include_usage":true}});
        if let Some(t) = tools { payload["tools"] = t.clone(); }
        self.calls.fetch_add(1, Ordering::Relaxed);
        crate::stream::complete(self.base.clone(), self.key.clone(), payload, self.timeout, control, emit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_and_tool_calls() {
        let body = json!({
            "choices": [{"message": {
                "content": "working",
                "tool_calls": [
                    {"id":"a","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}},
                    {"id":"b","type":"function","function":{"name":"read","arguments":"{\"path\":\"x\"}"}}
                ]
            }}]
        });
        let reply = parse_reply(&body).unwrap();
        assert_eq!(reply.text, "working");
        assert_eq!(reply.calls.len(), 2);
        assert_eq!(reply.calls[0].name, "bash");
        assert_eq!(reply.calls[0].args["command"], "ls");
    }

    #[test]
    fn tolerates_missing_tool_calls() {
        let body = json!({"choices":[{"message":{"content":"done"}}]});
        let reply = parse_reply(&body).unwrap();
        assert!(reply.calls.is_empty());
        assert_eq!(reply.text, "done");
    }

    #[test]
    fn malformed_arguments_are_kept_not_dropped() {
        let body = json!({"choices":[{"message":{"tool_calls":[
            {"id":"a","function":{"name":"bash","arguments":"not json"}}
        ]}}]});
        let reply = parse_reply(&body).unwrap();
        assert_eq!(reply.calls[0].args["_raw"], "not json");
    }

}
