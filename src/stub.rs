//! Scripted model. Lets every claim about the loop be tested without a network.
//!
//! This is not a mock framework: it is the minimum needed to prove that the
//! round-trip count, the tiering and the verifier gate actually behave.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::Value;

use crate::provider::{Llm, Reply, Roles, ToolCall};

pub struct StubLlm {
    roles: Roles,
    calls: AtomicU64,
    scripted: Mutex<Vec<Reply>>,
    seen: Mutex<Vec<String>>,
}

impl StubLlm {
    pub fn new(scripted: Vec<Reply>) -> Self {
        let mut map = HashMap::new();
        for role in ["filter", "plan", "code", "judge"] {
            map.insert(role.to_string(), "stub".to_string());
        }
        Self {
            roles: Roles(map),
            calls: AtomicU64::new(0),
            scripted: Mutex::new(scripted),
            seen: Mutex::new(Vec::new()),
        }
    }

    pub fn text(message: &str) -> Reply {
        Reply { text: message.to_string(), calls: Vec::new(), usage: Default::default() }
    }

    pub fn with_tools(message: &str, calls: Vec<ToolCall>) -> Reply {
        Reply { text: message.to_string(), calls, usage: Default::default() }
    }

    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("stub").clone()
    }
}

impl Llm for StubLlm {
    fn roles(&self) -> &Roles {
        &self.roles
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    fn complete(
        &self,
        _model: &str,
        _system: &str,
        messages: &[Value],
        _tools: Option<&Value>,
    ) -> Result<Reply, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(last) = messages
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
        {
            self.seen.lock().expect("stub").push(last.to_string());
        }
        let mut scripted = self.scripted.lock().expect("stub");
        if scripted.is_empty() {
            return Ok(Reply { text: "script exhausted".into(), calls: Vec::new(), usage: Default::default() });
        }
        Ok(scripted.remove(0))
    }
}

pub fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), args }
}
