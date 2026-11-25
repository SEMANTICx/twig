//! Model protocol types, OpenAI reply parsing, and provider-reported token telemetry.
//!
//! Pure leaf layer for `llm`: depends only on `crate::control::Control`.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::control::Control;

/// Provider-reported tokens, never inferred from text or called dollars.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
}

impl Usage {
    pub fn parse(v: &Value) -> Self {
        Self {
            input_tokens: v
                .get("prompt_tokens")
                .or_else(|| v.get("input_tokens"))
                .and_then(Value::as_u64),
            output_tokens: v
                .get("completion_tokens")
                .or_else(|| v.get("output_tokens"))
                .and_then(Value::as_u64),
            cached_input_tokens: v
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .or_else(|| v.get("cache_read_input_tokens").and_then(Value::as_u64)),
        }
    }
}

pub fn summarize_usage(rows: &[Value]) -> Value {
    let replies: Vec<_> = rows.iter().filter(|r| r["kind"] == "model.reply").collect();
    let known = replies
        .iter()
        .filter(|r| {
            r["usage"]["input_tokens"].is_u64() && r["usage"]["output_tokens"].is_u64()
        })
        .count();
    let sum = |field: &str| {
        replies
            .iter()
            .filter_map(|r| r["usage"][field].as_u64())
            .sum::<u64>()
    };
    let total = |field: &str| {
        (!replies.is_empty() && replies.iter().all(|r| r["usage"][field].is_u64()))
            .then(|| sum(field))
    };
    json!({
        "input_tokens": total("input_tokens"),
        "output_tokens": total("output_tokens"),
        "cached_input_tokens": total("cached_input_tokens"),
        "cache_reported_calls": replies
            .iter()
            .filter(|r| r["usage"]["cached_input_tokens"].is_u64())
            .count(),
        "reported_calls": known,
        "calls": replies.len(),
        "complete": known == replies.len() && !replies.is_empty(),
        "models": replies
            .iter()
            .filter_map(|r| r["model"].as_str())
            .collect::<BTreeSet<_>>(),
    })
}

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
    pub usage: Usage,
}

#[derive(Debug, Clone)]
pub struct Roles(pub HashMap<String, String>);

impl Default for Roles {
    fn default() -> Self {
        let mut m = HashMap::new();
        let model = std::env::var("TWIG_MODEL").unwrap_or_else(|_| "aihub/gpt-6.1-sol".into());
        for role in ["filter", "plan", "code", "judge"] {
            m.insert(role.into(), model.clone());
        }
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

/// Extract a reply from an OpenAI-shaped response body.
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
            Some(Value::String(raw)) => {
                serde_json::from_str(raw).unwrap_or(json!({ "_raw": raw }))
            }
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
    Ok(Reply {
        text,
        calls,
        usage: Usage::parse(&body["usage"]),
    })
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
    fn stream(
        &self,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: Option<&Value>,
        control: &Control,
        emit: &mut dyn FnMut(Value),
    ) -> Result<Reply, String> {
        if control.cancelled() {
            return Err("cancelled".into());
        }
        let reply = self.complete(model, system, messages, tools)?;
        if !reply.text.is_empty() {
            emit(json!({"ev": "model.delta", "text": reply.text}));
        }
        Ok(reply)
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

    #[test]
    fn preserves_cache_and_missing_usage_without_inventing_zero_cost() {
        let u = Usage::parse(&json!({"prompt_tokens":100,"completion_tokens":20,
            "prompt_tokens_details":{"cached_tokens":80}}));
        assert_eq!(u.input_tokens, Some(100));
        assert_eq!(u.cached_input_tokens, Some(80));
        assert_eq!(Usage::parse(&Value::Null).input_tokens, None);
        let s = summarize_usage(&[
            json!({"kind":"model.reply","usage":u,"model":"a"}),
            json!({"kind":"model.reply","model":"a"}),
        ]);
        assert_eq!(s["reported_calls"], 1);
        assert_eq!(s["complete"], false);
        assert_eq!(s["cached_input_tokens"], Value::Null);
        let no_cache = summarize_usage(&[
            json!({"kind":"model.reply","usage":{"input_tokens":10,"output_tokens":1}}),
        ]);
        assert_eq!(no_cache["complete"], true);
        assert_eq!(no_cache["cached_input_tokens"], Value::Null);
        assert_eq!(no_cache["cache_reported_calls"], 0);
    }
}
