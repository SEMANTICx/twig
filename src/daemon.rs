//! Session daemon: the loop knows nothing about who drives it.
//!
//! stdin  one JSON request per line  {"id":1,"method":"turn","params":{...}}
//! stdout {"id":1,"result":{...}} for replies, {"ev":"..."} for events
//!
//! This is the app-server seam. A TUI, an IDE, a scheduler and the outer loop can
//! all drive the same session without the session caring which one it is.
//!
//! The verifier is wired in here, not bolted on beside it: when the model claims
//! it is finished, the recorded journal is replayed against the candidate before
//! the turn is accepted. A mismatch is fed back as the next user message.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::config::HarnessConfig;
use crate::guard::Guard;
use crate::playbook::{route, Playbook};
use crate::policy::Policy;
use crate::provider::Llm;
use crate::record::Record;
use crate::tools::Tools;
use crate::turn::{run_turn, TurnCtx};
use crate::verify::{gate, Candidate};

pub struct Daemon<'a> {
    llm: &'a dyn Llm,
    worktree: PathBuf,
    record: Record,
    guard: Guard,
    system: String,
    tools: Tools,
    policy: Policy,
    messages: Vec<Value>,
    audit: Vec<Value>,
    /// T2 calls waiting on a human. The only place the harness ever waits.
    pending: Vec<Value>,
    journal: Option<PathBuf>,
    candidate: Option<Candidate>,
    min_branch: f64,
    max_verify_rounds: u32,
    playbooks: Vec<Playbook>,
}

impl<'a> Daemon<'a> {
    pub fn new(
        llm: &'a dyn Llm,
        worktree: impl Into<PathBuf>,
        runs_dir: impl Into<PathBuf>,
        sid: impl Into<String>,
        cfg: &HarnessConfig,
    ) -> Result<Self, String> {
        let worktree: PathBuf = worktree.into();
        let runs_dir: PathBuf = runs_dir.into();
        let sid = sid.into();
        let record =
            Record::open(runs_dir.join(format!("{sid}.jsonl")), sid).map_err(|e| e.to_string())?;
        // the record must never read as agent progress
        let guard = Guard::new(&worktree, cfg.max_turns, cfg.stall_turns, 3).ignoring([runs_dir.clone()]);
        let playbooks = Playbook::load_dir(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("playbooks"));
        Ok(Self {
            llm,
            tools: Tools::with_config(&worktree, cfg),
            system: cfg.system.clone(),
            worktree,
            record,
            guard,
            policy: Policy::new(),
            messages: Vec::new(),
            audit: Vec::new(),
            pending: Vec::new(),
            journal: None,
            candidate: None,
            min_branch: 0.0,
            max_verify_rounds: 2,
            playbooks,
        })
    }

    pub fn record_path(&self) -> &std::path::Path {
        self.record.path()
    }

    pub fn sid(&self) -> String {
        self.record
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    pub fn dispatch(
        &mut self,
        method: &str,
        params: &Value,
        emit: &mut dyn FnMut(Value),
    ) -> Result<Value, String> {
        match method {
            "ping" => Ok(json!({"pong": true, "sid": self.sid()})),

            "playbooks.route" => {
                let goal = params.get("goal").and_then(Value::as_str).unwrap_or("");
                match route(goal, &self.playbooks) {
                    Some(book) => Ok(json!({
                        "playbook": book.name,
                        "steps": book.steps,
                        "keyword_hits": book.when.iter()
                            .filter(|k| goal.to_lowercase().contains(k.as_str()))
                            .collect::<Vec<_>>()
                    })),
                    None => Ok(json!({
                        "playbook": Value::Null,
                        "why": "no playbook matched; ask instead of guessing"
                    })),
                }
            }

            "turn" => {
                if let Some(j) = params.get("journal").and_then(Value::as_str) {
                    self.journal = Some(PathBuf::from(j));
                }
                if let Some(c) = params.get("candidate") {
                    if let Some(program) = c.get("program").and_then(Value::as_str) {
                        let args = c
                            .get("args")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default();
                        self.candidate = Some(Candidate::new(program, args));
                    }
                }
                if let Some(b) = params.get("min_branch").and_then(Value::as_f64) {
                    self.min_branch = b;
                }

                let input = params.get("input").and_then(Value::as_str).unwrap_or("");
                let role = params.get("role").and_then(Value::as_str).unwrap_or("code");
                let model = self.llm.roles().get(role).to_string();
                self.messages.push(json!({"role": "user", "content": input}));

                let mut verify_rounds = 0u32;
                loop {
                    let result = {
                        let audit = &mut self.audit;
                        let pending = &mut self.pending;
                        let sink = |call: &crate::provider::ToolCall, out: &str| {
                            audit.push(json!({"id": call.id, "args": call.args, "bytes": out.len()}));
                        };
                        let mut capture = |ev: Value| {
                            if ev["ev"] == "approval.requested" {
                                pending.push(ev.clone());
                            }
                            emit(ev);
                        };
                        run_turn(
                            TurnCtx {
                                llm: self.llm,
                                tools: &mut self.tools,
                                record: &mut self.record,
                                guard: &mut self.guard,
                                policy: &self.policy,
                                messages: &mut self.messages,
                                system: &self.system,
                                model: &model,
                            },
                            &mut { sink },
                            &mut capture,
                        )?
                    };

                    if !result.done {
                        continue;
                    }
                    if result.reason == "budget" {
                        return Ok(json!({"done": true, "reason": "budget"}));
                    }

                    // The gate: replay the oracle before accepting "finished".
                    let (Some(journal), Some(candidate)) = (&self.journal, &self.candidate) else {
                        return Ok(json!({
                            "done": true,
                            "reason": "final",
                            "text": result.text,
                            "verified": Value::Null,
                            "why": "no journal supplied; unverified result"
                        }));
                    };

                    let verdict = gate(journal, candidate, 1, None, self.min_branch, None);
                    let kind = if verdict.ok { "verify.passed" } else { "verify.failed" };
                    self.record
                        .append(kind, json!({"verdict": verdict}))
                        .map_err(|e| e.to_string())?;
                    emit(json!({"ev": kind, "verdict": verdict}));

                    if verdict.ok {
                        return Ok(json!({"done": true, "reason": "final",
                                         "text": result.text, "verified": true}));
                    }
                    if verify_rounds >= self.max_verify_rounds {
                        return Ok(json!({"done": false, "reason": "verify_failed",
                                         "verdict": verdict}));
                    }
                    verify_rounds += 1;
                    self.messages.push(json!({
                        "role": "user",
                        "content": format!(
                            "Replay disagrees with the recorded oracle. Fix the code, not the journal:\n{}",
                            serde_json::to_string_pretty(&verdict).unwrap_or_default()
                        )
                    }));
                }
            }

            "approval.list" => Ok(json!({"pending": self.pending})),

            "approval.resolve" => {
                let id = params.get("id").and_then(Value::as_str).ok_or("approval.resolve needs id")?;
                let allow = params.get("allow").and_then(Value::as_bool).unwrap_or(false);
                let at = self.pending.iter().position(|p| p["id"] == json!(id))
                    .ok_or_else(|| format!("no pending approval {id:?}"))?;
                let request = self.pending.remove(at);
                if !allow {
                    self.record.append("approval.resolved", json!({"id": id, "allow": false}))
                        .map_err(|e| e.to_string())?;
                    emit(json!({"ev": "approval.resolved", "id": id, "allow": false}));
                    return Ok(json!({"id": id, "allow": false}));
                }
                let call = crate::provider::ToolCall {
                    id: id.to_string(),
                    name: "bash".into(),
                    args: request["args"].clone(),
                };
                let output = self.tools.run(&call);
                self.record
                    .append("approval.resolved", json!({"id": id, "allow": true, "output": output}))
                    .map_err(|e| e.to_string())?;
                emit(json!({"ev": "approval.resolved", "id": id, "allow": true}));
                // The model saw "blocked"; tell it what actually happened.
                let clipped = self.tools.clip(&output);
                self.messages.push(json!({
                    "role": "user",
                    "content": format!("A human approved call {id}. It ran:\n{clipped}")
                }));
                Ok(json!({"id": id, "allow": true, "output": clipped}))
            }

            "record.verify" => match Record::verify_chain(self.record.path()) {
                Ok(entries) => Ok(json!({"ok": true, "entries": entries})),
                Err((index, why)) => Ok(json!({"ok": false, "bad_index": index, "why": why})),
            },

            "record.tail" => {
                let n = params.get("n").and_then(Value::as_u64).unwrap_or(20) as usize;
                Ok(json!({"rows": self.record.tail(n).map_err(|e| e.to_string())?}))
            }

            "policy.stats" => {
                let (hits, misses, size) = self.policy.stats();
                Ok(json!({"hits": hits, "misses": misses, "size": size}))
            }

            "guard.report" => {
                let (samples, stall, digest) = self.guard.report();
                Ok(json!({"samples": samples, "stall": stall, "digest": digest,
                          "worktree": self.worktree.to_string_lossy()}))
            }

            "audit.drain" => {
                let drained = std::mem::take(&mut self.audit);
                Ok(json!({"rows": drained}))
            }

            other => Err(format!("unknown method {other:?}")),
        }
    }

    pub fn serve(&mut self, input: impl BufRead, mut out: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let request: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    writeln!(out, "{}", json!({"error": format!("request json: {e}")}))?;
                    out.flush()?;
                    continue;
                }
            };
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let params = request.get("params").cloned().unwrap_or(json!({}));

            let mut events: Vec<Value> = Vec::new();
            let mut emit = |event: Value| events.push(event);
            let outcome = self.dispatch(&method, &params, &mut emit);
            for event in events {
                writeln!(out, "{event}")?;
            }
            let reply = match outcome {
                Ok(result) => json!({"id": id, "result": result}),
                Err(error) => json!({"id": id, "error": error}),
            };
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stub::StubLlm;

    #[test]
    fn approving_a_pending_call_runs_it_and_tells_the_model() {
        let dir = std::env::temp_dir().join(format!("twig-daemon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let llm = StubLlm::new(vec![]);
        let mut d = Daemon::new(&llm, &dir, dir.join("runs"), "appr", &HarnessConfig::default()).unwrap();
        // A held T2 call, as run_turn leaves it. The command itself is harmless:
        // what is under test is the approve path, not the deny list.
        d.pending.push(json!({"ev": "approval.requested", "id": "x", "args": {"command": "echo approved"}}));

        let out = d.dispatch("approval.resolve", &json!({"id": "x", "allow": true}), &mut |_| {}).unwrap();
        assert_eq!(out["allow"], json!(true));
        assert!(out["output"].as_str().unwrap().contains("approved"));
        assert!(d.pending.is_empty());
        assert!(d.messages.iter().any(|m| m["content"].as_str().unwrap_or("").contains("A human approved call x")));
        assert!(Record::verify_chain(d.record.path()).is_ok());
        assert!(d.dispatch("approval.resolve", &json!({"id": "x", "allow": true}), &mut |_| {}).is_err(),
                "a resolved call cannot be resolved twice");
    }
}
