//! OpenAI-compatible client pointed at magpie.
//!
//! Strict DAG: `mod` (Provider) -> `stream` -> `types`; `stub` -> `types`.

pub mod stream;
pub mod stub;
pub mod types;

pub use types::{parse_reply, summarize_usage, Llm, Reply, Roles, ToolCall, Usage};

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use crate::control::Control;

pub const DEFAULT_BASE: &str = "http://127.0.0.1:3425/v1";

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

    fn stream(
        &self,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: Option<&Value>,
        control: &Control,
        emit: &mut dyn FnMut(Value),
    ) -> Result<Reply, String> {
        let mut msgs = vec![json!({"role": "system", "content": system})];
        msgs.extend_from_slice(messages);
        let mut payload = json!({
            "model": model,
            "messages": msgs,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if let Some(t) = tools {
            payload["tools"] = t.clone();
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        stream::complete(
            self.base.clone(),
            self.key.clone(),
            payload,
            self.timeout,
            control,
            emit,
        )
    }
}
