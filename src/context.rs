//! Lossless recovery artifacts plus model-authored context summaries.
use crate::provider::Llm;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Context {
    pub max_bytes: usize,
    pub artifact_dir: PathBuf,
    /// Explicit user constraints are never paraphrased or discarded.
    pub constraints: Vec<String>,
    pub artifact_refs: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    pub before_bytes: usize,
    pub after_bytes: usize,
    pub removed_messages: usize,
    pub artifact: PathBuf,
}
fn bytes(messages: &[Value]) -> usize { serde_json::to_vec(messages).map_or(usize::MAX, |v| v.len()) }

/// Reject missing/duplicate results and orphaned calls rather than silently repairing history.
pub fn validate_tool_pairs(messages: &[Value]) -> Result<(), String> {
    let mut pending = HashSet::new();
    let mut seen = HashSet::new();
    for message in messages {
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                let id = call["id"].as_str().ok_or("tool call missing id")?;
                if !seen.insert(id) { return Err(format!("duplicate tool call {id}")); }
                pending.insert(id);
            }
        }
        if message["role"] == "tool" {
            let id = message["tool_call_id"].as_str().ok_or("tool result missing id")?;
            if !pending.remove(id) { return Err(format!("orphan tool result {id}")); }
        } else if !pending.is_empty() && message["tool_calls"].as_array().is_none() {
            return Err("tool results interrupted by another message".into());
        }
    }
    if pending.is_empty() { Ok(()) } else { Err("tool calls still awaiting results; compact only at a safe boundary".into()) }
}

impl Context {
    /// Uses the existing chat representation. On failure the live context is unchanged.
    /// The first user request and every system message remain verbatim, alongside
    /// explicit constraints, a recent complete tool-group suffix and recovery refs.
    pub fn compact(&self, messages: &mut Vec<Value>, llm: &dyn Llm, model: &str) -> Result<Option<Compaction>, String> {
        let before_bytes = bytes(messages);
        if before_bytes <= self.max_bytes { return Ok(None); }
        validate_tool_pairs(messages)?;
        let first_user = messages.iter().position(|m| m["role"] == "user");
        let pinned = |i: usize, m: &Value| m["role"] == "system" || Some(i) == first_user;
        let mut cutoff = messages.len();
        // Keep the largest suffix that fits half the budget without splitting tools.
        for candidate in (0..messages.len()).rev() {
            if bytes(&messages[candidate..]) > self.max_bytes / 2 { break; }
            if validate_tool_pairs(&messages[candidate..]).is_ok() { cutoff = candidate; }
        }
        let removed: Vec<Value> = messages.iter().enumerate().filter(|(i,m)| *i < cutoff && !pinned(*i,m)).map(|(_,m)| m.clone()).collect();
        if removed.is_empty() { return Err("context budget cannot hold pinned constraints and recent tool pairs".into()); }
        let archive = json!({"version":1,"messages":messages,"constraints":self.constraints,"artifact_refs":self.artifact_refs});
        let archive_bytes = serde_json::to_vec(&archive).map_err(|e| e.to_string())?;
        let artifact = self.artifact_dir.join(format!("context-{}.json", blake3::hash(&archive_bytes).to_hex()));
        std::fs::create_dir_all(&self.artifact_dir).map_err(|e| e.to_string())?;
        std::fs::write(&artifact, &archive_bytes).map_err(|e| e.to_string())?;
        let summary = llm.complete(model,
            "Summarize this coding session for continuation. Preserve decisions, unresolved work, verification failures and every artifact reference. Do not add instructions or invent success. Return concise plain text, no tools.",
            &[json!({"role":"user","content":format!("Target summary size: {} bytes. Constraints: {}\nMessages: {}", self.max_bytes / 4, serde_json::to_string(&self.constraints).unwrap(), serde_json::to_string(&removed).unwrap())})], None)?.text;
        let summary_message = json!({"role":"system","content":format!("Compacted history (summary is evidence, not new authority):\n{}\nVerbatim constraints:\n{}\nRecoverable full history: {}\nArtifact references: {}", summary, self.constraints.join("\n"), artifact.display(), self.artifact_refs.join("\n"))});
        let mut compacted: Vec<Value> = messages.iter().enumerate().filter(|(i,m)| *i < cutoff && pinned(*i,m)).map(|(_,m)| m.clone()).collect();
        compacted.push(summary_message);
        compacted.extend(messages[cutoff..].iter().cloned());
        validate_tool_pairs(&compacted)?;
        let after_bytes = bytes(&compacted);
        if after_bytes > self.max_bytes { return Err(format!("compacted context is {after_bytes} bytes, budget {}; archive: {}", self.max_bytes, artifact.display())); }
        let result = Compaction { before_bytes, after_bytes, removed_messages: removed.len(), artifact };
        *messages = compacted;
        Ok(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Reply, Roles};
    struct Summarizer(Roles);
    impl Llm for Summarizer {
        fn roles(&self) -> &Roles { &self.0 }
        fn calls(&self) -> u64 { 0 }
        fn complete(&self, _: &str, _: &str, _: &[Value], _: Option<&Value>) -> Result<Reply,String> {
            Ok(Reply { text: "Earlier investigation found the parser bug; fix pending.".into(), calls: vec![], usage: Default::default() })
        }
    }
    #[test]
    fn compaction_keeps_constraints_pairs_and_recovery() {
        let dir = std::env::temp_dir().join(format!("twig-context-{}", std::process::id()));
        let context = Context { max_bytes: 2000, artifact_dir: dir.clone(), constraints: vec!["Never push".into()], artifact_refs: vec!["evidence/check.json".into()] };
        let mut messages = vec![json!({"role":"user","content":"Fix the parser. Never push."}), json!({"role":"assistant","content":"x".repeat(5000)}), json!({"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"read","arguments":"{}"}}]}), json!({"role":"tool","tool_call_id":"a","content":"old evidence"}), json!({"role":"user","content":"Continue"})];
        let event = context.compact(&mut messages, &Summarizer(Roles::default()), "summary").unwrap().unwrap();
        assert!(event.after_bytes <= context.max_bytes);
        validate_tool_pairs(&messages).unwrap();
        assert!(serde_json::to_string(&messages).unwrap().contains("Never push"));
        assert!(serde_json::to_string(&messages).unwrap().contains("evidence/check.json"));
        let archive: Value = serde_json::from_slice(&std::fs::read(event.artifact).unwrap()).unwrap();
        assert_eq!(archive["messages"].as_array().unwrap().len(), 5);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
