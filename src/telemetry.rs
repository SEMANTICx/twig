//! Provider-reported tokens, never inferred from text or called dollars.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
}

impl Usage {
    pub fn parse(v: &Value) -> Self {
        Self {
            input_tokens: v.get("prompt_tokens").or_else(|| v.get("input_tokens")).and_then(Value::as_u64),
            output_tokens: v.get("completion_tokens").or_else(|| v.get("output_tokens")).and_then(Value::as_u64),
            cached_input_tokens: v.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64)
                .or_else(|| v.get("cache_read_input_tokens").and_then(Value::as_u64)),
        }
    }
}

pub fn summarize(rows: &[Value]) -> Value {
    let replies: Vec<_> = rows.iter().filter(|r| r["kind"] == "model.reply").collect();
    let known = replies.iter().filter(|r| r["usage"]["input_tokens"].is_u64()
        && r["usage"]["output_tokens"].is_u64()).count();
    let sum = |field: &str| replies.iter().filter_map(|r| r["usage"][field].as_u64()).sum::<u64>();
    let total = |field: &str| (!replies.is_empty() && replies.iter().all(|r| r["usage"][field].is_u64()))
        .then(|| sum(field));
    json!({
        "input_tokens": total("input_tokens"), "output_tokens": total("output_tokens"),
        "cached_input_tokens": total("cached_input_tokens"),
        "cache_reported_calls": replies.iter().filter(|r| r["usage"]["cached_input_tokens"].is_u64()).count(),
        "reported_calls": known, "calls": replies.len(),
        "complete": known == replies.len() && !replies.is_empty(),
        "models": replies.iter().filter_map(|r| r["model"].as_str()).collect::<std::collections::BTreeSet<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_cache_and_missing_usage_without_inventing_zero_cost() {
        let u = Usage::parse(&json!({"prompt_tokens":100,"completion_tokens":20,
            "prompt_tokens_details":{"cached_tokens":80}}));
        assert_eq!(u.input_tokens, Some(100));
        assert_eq!(u.cached_input_tokens, Some(80));
        assert_eq!(Usage::parse(&Value::Null).input_tokens, None);
        let s = summarize(&[json!({"kind":"model.reply","usage":u,"model":"a"}),
            json!({"kind":"model.reply","model":"a"})]);
        assert_eq!(s["reported_calls"], 1);
        assert_eq!(s["complete"], false);
        assert_eq!(s["cached_input_tokens"], Value::Null);
        let no_cache = summarize(&[json!({"kind":"model.reply","usage":{"input_tokens":10,"output_tokens":1}})]);
        assert_eq!(no_cache["complete"], true);
        assert_eq!(no_cache["cached_input_tokens"], Value::Null);
        assert_eq!(no_cache["cache_reported_calls"], 0);
    }
}
