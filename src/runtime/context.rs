//! Lossless recovery artifacts plus model-authored context summaries.
//!
//! Constraints and artifact references are encapsulated behind `Context` so
//! callers cannot mutate them inconsistently or drop constraints on compaction.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::llm::Llm;

#[derive(Debug, Clone)]
pub struct Context {
    max_bytes: usize,
    artifact_dir: PathBuf,
    /// Explicit user constraints are never paraphrased or discarded.
    constraints: Vec<String>,
    artifact_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    pub before_bytes: usize,
    pub after_bytes: usize,
    pub removed_messages: usize,
    pub artifact: PathBuf,
}

fn bytes(messages: &[Value]) -> usize {
    serde_json::to_vec(messages).map_or(usize::MAX, |v| v.len())
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes.saturating_sub(3);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Reject missing/duplicate results and orphaned calls rather than silently repairing history.
pub fn validate_tool_pairs(messages: &[Value]) -> Result<(), String> {
    let mut pending = HashSet::new();
    let mut seen = HashSet::new();
    for message in messages {
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                let id = call["id"].as_str().ok_or("tool call missing id")?;
                if !seen.insert(id) {
                    return Err(format!("duplicate tool call {id}"));
                }
                pending.insert(id);
            }
        }
        if message["role"] == "tool" {
            let id = message["tool_call_id"]
                .as_str()
                .ok_or("tool result missing id")?;
            if !pending.remove(id) {
                return Err(format!("orphan tool result {id}"));
            }
        } else if !pending.is_empty() && message["tool_calls"].as_array().is_none() {
            return Err("tool results interrupted by another message".into());
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err("tool calls still awaiting results; compact only at a safe boundary".into())
    }
}

const MICROCOMPACT_RECENT_TOOLS: usize = 6;
const MICROCOMPACT_MIN_BYTES: usize = 1_200;

impl Context {
    pub fn new(max_bytes: usize, artifact_dir: PathBuf) -> Self {
        Self {
            max_bytes,
            artifact_dir,
            constraints: Vec::new(),
            artifact_refs: Vec::new(),
        }
    }

    /// Stage 1 deterministic zero-LLM compaction: archive older large `tool` outputs
    /// to `artifact:tool-<id>.txt` while keeping recent tool outputs verbatim.
    fn microcompact(&self, messages: &mut [Value]) -> usize {
        let tool_positions: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| (m["role"] == "tool").then_some(i))
            .collect();
        if tool_positions.len() <= MICROCOMPACT_RECENT_TOOLS {
            return 0;
        }
        let fold_count = tool_positions.len() - MICROCOMPACT_RECENT_TOOLS;
        let mut folded = 0usize;
        for &idx in &tool_positions[..fold_count] {
            let Some(content) = messages[idx]["content"].as_str().map(str::to_owned) else {
                continue;
            };
            if content.len() <= MICROCOMPACT_MIN_BYTES || content.starts_with("[microcompacted ") {
                continue;
            }
            let raw_id = messages[idx]["tool_call_id"].as_str().unwrap_or("old");
            let safe_id: String = raw_id
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .take(48)
                .collect();
            let fname = format!("tool-{safe_id}.txt");
            let saved = std::fs::create_dir_all(&self.artifact_dir).is_ok()
                && std::fs::write(self.artifact_dir.join(&fname), &content).is_ok();
            let lines = content.lines().count();
            let replacement = if saved {
                format!(
                    "[microcompacted tool output ({} bytes, {lines} lines) -> inspect via read(path=\"artifact:{fname}\", start_line=1)]",
                    content.len()
                )
            } else {
                let preview: String = content.chars().take(320).collect();
                format!(
                    "[microcompacted tool output ({} bytes, {lines} lines)]\n{preview}...",
                    content.len()
                )
            };
            messages[idx]["content"] = json!(replacement);
            folded += 1;
        }
        folded
    }

    /// Emergency zero-LLM pass that folds ALL oversized tool outputs including recent ones.
    fn microcompact_all(&self, messages: &mut [Value]) -> usize {
        let mut folded = 0usize;
        for (i, msg) in messages.iter_mut().enumerate() {
            if msg["role"] != "tool" {
                continue;
            }
            let Some(content) = msg["content"].as_str().map(str::to_owned) else {
                continue;
            };
            if content.len() <= MICROCOMPACT_MIN_BYTES || content.starts_with("[microcompacted ") {
                continue;
            }
            let raw_id = msg["tool_call_id"].as_str().unwrap_or("tail");
            let safe_id: String = raw_id
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                .take(40)
                .collect();
            let fname = format!("tool-{safe_id}-{i}.txt");
            let saved = std::fs::create_dir_all(&self.artifact_dir).is_ok()
                && std::fs::write(self.artifact_dir.join(&fname), &content).is_ok();
            let lines = content.lines().count();
            msg["content"] = json!(if saved {
                format!(
                    "[microcompacted tool output ({} bytes, {lines} lines) -> inspect via read(path=\"artifact:{fname}\", start_line=1)]",
                    content.len()
                )
            } else {
                format!("[microcompacted tool output ({} bytes, {lines} lines)]", content.len())
            });
            folded += 1;
        }
        folded
    }

    pub fn add_constraint(&mut self, constraint: impl Into<String>) {
        let constraint = constraint.into();
        if !constraint.trim().is_empty() && !self.constraints.contains(&constraint) {
            self.constraints.push(constraint);
        }
    }

    pub fn add_artifact_ref(&mut self, reference: impl Into<String>) {
        let reference = reference.into();
        if !reference.trim().is_empty() && !self.artifact_refs.contains(&reference) {
            self.artifact_refs.push(reference);
        }
    }

    /// Ensure all recorded user constraints appear verbatim in the live messages
    /// without mutating message index 0 (preserving KV prompt cache prefixes).
    pub fn sync_constraints(&self, messages: &mut Vec<Value>) {
        for constraint in &self.constraints {
            let present = messages.iter().any(|m| {
                m["content"].as_str() == Some(constraint.as_str())
                    || m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains(constraint.as_str()))
            });
            if !present {
                messages.push(json!({"role": "system", "content": constraint}));
            }
        }
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn used_bytes(&self, messages: &[Value]) -> usize {
        bytes(messages)
    }

    fn all_artifact_refs(&self) -> Vec<String> {
        let mut refs = self.artifact_refs.clone();
        if let Ok(entries) = std::fs::read_dir(&self.artifact_dir) {
            let mut timed_files: Vec<(std::time::SystemTime, String)> = entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if !(name.starts_with("tool-") && name.ends_with(".txt")) {
                        return None;
                    }
                    let mtime = e
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    Some((mtime, format!("artifact:{name}")))
                })
                .collect();
            timed_files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            for (_, r) in timed_files.into_iter().take(24) {
                if !refs.contains(&r) {
                    refs.push(r);
                }
            }
        }
        refs
    }

    /// Uses the existing chat representation. On failure the live context is unchanged.
    /// The first user request and every system message remain verbatim, alongside
    /// explicit constraints, a recent complete tool-group suffix and recovery refs.
    pub fn compact(
        &self,
        messages: &mut Vec<Value>,
        llm: &dyn Llm,
        model: &str,
    ) -> Result<Option<Compaction>, String> {
        self.compact_with_force(messages, llm, model, false)
    }

    /// Manual compaction triggered by `/compact` regardless of byte threshold.
    pub fn compact_now(
        &self,
        messages: &mut Vec<Value>,
        llm: &dyn Llm,
        model: &str,
    ) -> Result<Option<Compaction>, String> {
        self.compact_with_force(messages, llm, model, true)
    }

    fn compact_with_force(
        &self,
        messages: &mut Vec<Value>,
        llm: &dyn Llm,
        model: &str,
        force: bool,
    ) -> Result<Option<Compaction>, String> {
        let before_bytes = bytes(messages);
        if !force && before_bytes <= self.max_bytes {
            return Ok(None);
        }
        validate_tool_pairs(messages)?;
        let folded = self.microcompact(messages);
        let after_micro = bytes(messages);
        if !force && folded > 0 && after_micro <= self.max_bytes {
            return Ok(Some(Compaction {
                before_bytes,
                after_bytes: after_micro,
                removed_messages: folded,
                artifact: self.artifact_dir.clone(),
            }));
        }
        let first_user = messages.iter().position(|m| m["role"] == "user");
        let is_unpinned_system = |m: &Value| {
            m["role"] == "system"
                && m["content"].as_str().is_some_and(|s| {
                    let t = s.trim_start();
                    t.starts_with("<system-reminder>")
                        || t.starts_with("Compacted history (summary is evidence")
                })
        };
        let pinned = |i: usize, m: &Value| {
            (m["role"] == "system" && !is_unpinned_system(m)) || Some(i) == first_user
        };
        let pinned_prefix: Vec<Value> = messages
            .iter()
            .enumerate()
            .filter(|(i, m)| pinned(*i, m))
            .map(|(_, m)| m.clone())
            .collect();
        let pinned_bytes = bytes(&pinned_prefix);
        let max_tail = self
            .max_bytes
            .saturating_sub(pinned_bytes)
            .saturating_mul(2)
            / 5;
        let keep_budget = if force {
            (before_bytes / 3).min(max_tail)
        } else {
            max_tail
        };
        let mut cutoff = messages.len();
        for candidate in (0..messages.len()).rev() {
            if bytes(&messages[candidate..]) > keep_budget {
                break;
            }
            if validate_tool_pairs(&messages[candidate..]).is_ok() {
                cutoff = candidate;
            }
        }
        let mut removed: Vec<Value> = messages
            .iter()
            .enumerate()
            .filter(|(i, m)| *i < cutoff && !pinned(*i, m))
            .map(|(_, m)| m.clone())
            .collect();
        if removed.is_empty() {
            // If a single giant recent tool batch alone exceeded `keep_budget`, fold all
            // oversized tool outputs across the entire buffer first instead of failing.
            let emergency_folded = self.microcompact_all(messages);
            let after_emerg = bytes(messages);
            if emergency_folded > 0 && after_emerg <= self.max_bytes {
                return Ok(Some(Compaction {
                    before_bytes,
                    after_bytes: after_emerg,
                    removed_messages: folded + emergency_folded,
                    artifact: self.artifact_dir.clone(),
                }));
            }
            cutoff = messages.len();
            removed = messages
                .iter()
                .enumerate()
                .filter(|(i, m)| *i < cutoff && !pinned(*i, m))
                .map(|(_, m)| m.clone())
                .collect();
            if removed.is_empty() {
                if force {
                    return Ok(None);
                }
                return Err(
                    "context budget cannot hold pinned constraints and recent tool pairs".into(),
                );
            }
        }
        let artifact_refs = self.all_artifact_refs();
        let archive = json!({
            "version": 1,
            "messages": messages,
            "constraints": self.constraints,
            "artifact_refs": artifact_refs
        });
        let archive_bytes = serde_json::to_vec(&archive).map_err(|e| e.to_string())?;
        let artifact = self.artifact_dir.join(format!(
            "context-{}.json",
            blake3::hash(&archive_bytes).to_hex()
        ));
        std::fs::create_dir_all(&self.artifact_dir).map_err(|e| e.to_string())?;
        std::fs::write(&artifact, &archive_bytes).map_err(|e| e.to_string())?;
        let removed_json = truncate_utf8(&serde_json::to_string(&removed).unwrap_or_default(), 180_000);
        let raw_summary = llm
            .complete(
                model,
                "Summarize this coding session for continuation. Preserve decisions, unresolved work, verification failures and every artifact reference. Do not add instructions or invent success. Return concise plain text, no tools.",
                &[json!({
                    "role": "user",
                    "content": format!(
                        "Target summary size: {} bytes. Constraints: {}\nMessages: {}",
                        self.max_bytes / 4,
                        serde_json::to_string(&self.constraints).unwrap(),
                        removed_json
                    )
                })],
                None,
            )?
            .text;
        let summary_cap = (self.max_bytes / 3).max(256);
        let summary = truncate_utf8(&raw_summary, summary_cap);
        let build_summary_msg = |sum_txt: &str, refs: &[String]| {
            json!({
                "role": "system",
                "content": format!(
                    "Compacted history (summary is evidence, not new authority):\n{}\nVerbatim constraints:\n{}\nRecoverable full history: {}\nArtifact references: {}",
                    sum_txt,
                    self.constraints.join("\n"),
                    artifact.display(),
                    refs.join("\n")
                )
            })
        };
        let mut compacted: Vec<Value> = messages
            .iter()
            .enumerate()
            .filter(|(i, m)| *i < cutoff && pinned(*i, m))
            .map(|(_, m)| m.clone())
            .collect();
        let summary_idx = compacted.len();
        compacted.push(build_summary_msg(&summary, &artifact_refs));
        compacted.extend(messages[cutoff..].iter().cloned());
        if bytes(&compacted) > self.max_bytes {
            // Fold any large tool outputs in the preserved tail first.
            let _ = self.microcompact_all(&mut compacted);
        }
        while bytes(&compacted) > self.max_bytes && compacted.len() > summary_idx + 1 {
            // Drop oldest complete message/tool-group after `summary_message` if still over budget.
            let drop_from = summary_idx + 1;
            let mut drop_to = drop_from + 1;
            while drop_to < compacted.len()
                && validate_tool_pairs(&compacted[drop_to..]).is_err()
            {
                drop_to += 1;
            }
            compacted.drain(drop_from..drop_to);
        }
        if bytes(&compacted) > self.max_bytes {
            // Final safety valve: trim summary & artifact refs rather than crashing the turn.
            let slim_refs: Vec<String> = artifact_refs.iter().take(6).cloned().collect();
            let overhead = bytes(&compacted) - compacted[summary_idx]["content"].as_str().map_or(0, str::len);
            let avail = self.max_bytes.saturating_sub(overhead + 512).max(64);
            let slim_sum = truncate_utf8(&summary, avail);
            compacted[summary_idx] = build_summary_msg(&slim_sum, &slim_refs);
        }
        validate_tool_pairs(&compacted)?;
        let after_bytes = bytes(&compacted);
        if after_bytes > self.max_bytes {
            return Err(format!(
                "compacted context is {after_bytes} bytes, budget {}; archive: {}",
                self.max_bytes,
                artifact.display()
            ));
        }
        let result = Compaction {
            before_bytes,
            after_bytes,
            removed_messages: removed.len(),
            artifact,
        };
        *messages = compacted;
        Ok(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Reply, Roles};

    struct Summarizer(Roles);

    impl Llm for Summarizer {
        fn roles(&self) -> &Roles {
            &self.0
        }
        fn calls(&self) -> u64 {
            0
        }
        fn complete(
            &self,
            _: &str,
            _: &str,
            _: &[Value],
            _: Option<&Value>,
        ) -> Result<Reply, String> {
            Ok(Reply {
                text: "Earlier investigation found the parser bug; fix pending.".into(),
                calls: vec![],
                usage: Default::default(),
            })
        }
    }

    #[test]
    fn compaction_keeps_constraints_pairs_and_recovery() {
        let dir = std::env::temp_dir().join(format!("twig-context-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tool-c99.txt"), "full clipped output\n").unwrap();
        let mut context = Context::new(2000, dir.clone());
        context.add_constraint("Never push");
        context.add_artifact_ref("evidence/check.json");
        let mut messages = vec![
            json!({"role":"user","content":"Fix the parser. Never push."}),
            json!({"role":"assistant","content":"x".repeat(5000)}),
            json!({"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"a","content":"old evidence"}),
            json!({"role":"user","content":"Continue"}),
        ];
        let event = context
            .compact(&mut messages, &Summarizer(Roles::default()), "summary")
            .unwrap()
            .unwrap();
        assert!(event.after_bytes <= 2000);
        validate_tool_pairs(&messages).unwrap();
        let serialized = serde_json::to_string(&messages).unwrap();
        assert!(serialized.contains("Never push"));
        assert!(serialized.contains("evidence/check.json"));
        assert!(serialized.contains("artifact:tool-c99.txt"));
        let archive: Value =
            serde_json::from_slice(&std::fs::read(event.artifact).unwrap()).unwrap();
        assert_eq!(archive["messages"].as_array().unwrap().len(), 5);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn microcompact_archives_old_tool_outputs_without_calling_llm() {
        struct PanicLlm(Roles);
        impl Llm for PanicLlm {
            fn roles(&self) -> &Roles {
                &self.0
            }
            fn calls(&self) -> u64 {
                0
            }
            fn complete(
                &self,
                _: &str,
                _: &str,
                _: &[Value],
                _: Option<&Value>,
            ) -> Result<Reply, String> {
                panic!("LLM summarizer must not be called when microcompact satisfies budget");
            }
        }

        let dir = std::env::temp_dir().join(format!("twig-microcompact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ctx = Context::new(4000, dir.clone());
        let mut messages = vec![json!({"role": "user", "content": "Inspect all files"})];
        for i in 0..8 {
            let id = format!("c{i}");
            let body = if i < 2 {
                "L".repeat(2500)
            } else {
                "short".to_owned()
            };
            messages.push(json!({"role": "assistant", "tool_calls": [{"id": id, "type": "function", "function": {"name": "read", "arguments": "{}"}}]}));
            messages.push(json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": body}));
        }
        let res = ctx
            .compact(&mut messages, &PanicLlm(Roles::default()), "m")
            .unwrap()
            .expect("microcompacted");
        assert!(res.after_bytes <= 4000);
        assert_eq!(res.removed_messages, 2);
        assert!(dir.join("tool-c0.txt").is_file());
        assert!(messages[2]["content"]
            .as_str()
            .unwrap()
            .contains("artifact:tool-c0.txt"));
        validate_tool_pairs(&messages).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn transient_system_reminders_are_not_pinned_during_compaction() {
        use crate::llm::stub::StubLlm;
        let dir = std::env::temp_dir().join(format!("twig-rem-compact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut ctx = Context::new(800, dir.clone());
        ctx.add_constraint("never touch legacy/");
        let mut messages = vec![
            json!({"role": "user", "content": "initial task"}),
            json!({"role": "system", "content": format!("<system-reminder>\n{}\n</system-reminder>", "S".repeat(1200))}),
            json!({"role": "assistant", "content": "done 1"}),
            json!({"role": "user", "content": "next"}),
            json!({"role": "assistant", "content": "done 2"}),
        ];
        ctx.sync_constraints(&mut messages);
        let llm = StubLlm::new(vec![StubLlm::text("concise summary")]);
        let res = ctx.compact_now(&mut messages, &llm, "m").unwrap();
        assert!(res.is_some());
        assert!(!messages
            .iter()
            .any(|m| m["content"].as_str().is_some_and(|s| s.starts_with("<system-reminder>"))));
        assert!(messages
            .iter()
            .any(|m| m["content"].as_str().is_some_and(|s| s.contains("never touch legacy/"))));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn repeated_compactions_fold_prior_summaries_and_never_exceed_budget() {
        use crate::llm::stub::StubLlm;
        let dir = std::env::temp_dir().join(format!("twig-multi-compact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for k in 0..80 {
            std::fs::write(dir.join(format!("tool-old_{k}.txt")), "x").unwrap();
        }
        let ctx = Context::new(2400, dir.clone());
        let mut messages = vec![json!({"role": "user", "content": "Audit entire repository"})];
        for round in 0..5 {
            for step in 0..4 {
                let id = format!("r{round}_{step}");
                messages.push(json!({"role": "assistant", "tool_calls": [{"id": id, "type": "function", "function": {"name": "read", "arguments": "{}"}}]}));
                messages.push(json!({"role": "tool", "tool_call_id": format!("r{round}_{step}"), "content": "D".repeat(900)}));
            }
            let llm = StubLlm::new(vec![StubLlm::text(&"S".repeat(1600))]);
            let res = ctx.compact(&mut messages, &llm, "m").unwrap();
            assert!(res.is_some());
            assert!(ctx.used_bytes(&messages) <= 2400);
            let sum_count = messages
                .iter()
                .filter(|m| {
                    m["content"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("Compacted history"))
                })
                .count();
            assert_eq!(sum_count, 1, "prior summary must fold into new summary");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
