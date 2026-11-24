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
use crate::turn::{run_turn_live, TurnCtx};
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
    pub control: std::sync::Arc<crate::control::Control>,
    interactive: bool,
    _lease: crate::lease::Lease,
    context: crate::context::Context,
    check: Option<crate::execution::Check>,
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
        let lease = crate::lease::Lease::acquire(&runs_dir.join(format!("{sid}.lock"))).map_err(|e|e.to_string())?;
        let task_file=worktree.join(".twig/project.json");
        let project: Value=if task_file.exists(){serde_json::from_str(&std::fs::read_to_string(&task_file).map_err(|e|e.to_string())?).map_err(|e|format!("project config: {e}"))?}else{json!({})};
        let check=project.get("check").map(|c|serde_json::from_value(c.clone()).map_err(|e|format!("project check: {e}"))).transpose()?;
        let record =
            Record::open(runs_dir.join(format!("{sid}.jsonl")), sid).map_err(|e| e.to_string())?;
        // the record must never read as agent progress
        let restored=record.tail(usize::MAX).map_err(|e|e.to_string())?;
        let check=restored.iter().rev().find(|r|r["kind"]=="acceptance.configured")
            .and_then(|r|serde_json::from_value(r["check"].clone()).ok()).or(check);
        let guard = Guard::new(&worktree, cfg.max_turns, cfg.stall_turns, 3).ignoring([runs_dir.clone()]);
        let playbooks = [
            ("bug-fix", include_str!("../playbooks/bug-fix.md")),
            ("feature", include_str!("../playbooks/feature.md")),
            ("autonomous-run", include_str!("../playbooks/autonomous-run.md")),
        ].into_iter().filter_map(|(name, body)|Playbook::parse(body,name)).collect();
        let rows = record.tail(usize::MAX).map_err(|e|e.to_string())?;
        let checkpoint = rows.iter().rposition(|r|r["kind"]=="messages.checkpoint");
        let mut messages: Vec<Value> = checkpoint.and_then(|at|rows[at]["messages"].as_array().cloned()).unwrap_or_default();
        let pending: Vec<Value> = checkpoint.and_then(|at|rows[at].get("pending_call").cloned()).into_iter()
            .filter(|c|!rows.iter().skip(checkpoint.unwrap_or(0)).any(|r|r["kind"]=="approval.resolved" && r["id"]==c["id"]))
            .collect();
        for row in rows.iter().skip(checkpoint.map_or(0, |n|n+1)) {
            if row["kind"]=="message" { messages.push(row["message"].clone()); }
        }
        let completed: std::collections::HashSet<String> = messages.iter().filter_map(|m|m["tool_call_id"].as_str().map(str::to_string)).collect();
        let unfinished: Vec<_> = messages.iter().flat_map(|m|m["tool_calls"].as_array().into_iter().flatten())
            .filter_map(|c|c["id"].as_str()).filter(|id|!completed.contains(*id)
                && !pending.iter().any(|p|p["id"]==*id)).map(str::to_string).collect();
        for id in unfinished { messages.push(json!({"role":"tool","tool_call_id":id,
            "content":"Session interrupted; execution outcome unknown. Inspect workspace before retrying side effects."})); }
        Ok(Self {
            llm,
            _lease: lease,
            context: crate::context::Context {max_bytes:cfg.context_max_bytes,artifact_dir:runs_dir.join("artifacts"),constraints:Vec::new(),artifact_refs:Vec::new()},
            check,
            tools: Tools::with_config(&worktree, cfg),
            system: cfg.system.clone(),
            worktree,
            record,
            guard,
            policy: Policy::new(),
            messages,
            audit: Vec::new(),
            pending,
            journal: None,
            candidate: None,
            min_branch: 0.0,
            max_verify_rounds: 2,
            playbooks,
            control: Default::default(),
            interactive: false,
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
                if !self.pending.is_empty() { return Err("resolve recovered pending approvals before starting another turn".into()); }
                self.messages.push(json!({"role": "user", "content": input}));
                for constraint in params["constraints"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    if !self.context.constraints.iter().any(|c|c==constraint){self.context.constraints.push(constraint.into());}
                }
                self.record.append("message", json!({"message":{"role":"user","content":input}})).map_err(|e|e.to_string())?;
                if let Some(check)=params.get("check") { self.check=Some(serde_json::from_value(check.clone()).map_err(|e|format!("check config: {e}"))?); }
                if let Some(check)=&self.check {self.record.append("acceptance.configured",json!({"check":check})).map_err(|e|e.to_string())?;}
                let mut system=self.system.clone();
                if let Some(book)=route(input,&self.playbooks) { crate::skills::inject_playbook(&mut system,book,""); emit(json!({"ev":"playbook.selected","name":book.name,"steps":book.steps})); }
                let preferences=self.worktree.join(".twig/preferences.md");
                if preferences.exists(){system.push_str(&std::fs::read_to_string(preferences).map_err(|e|e.to_string())?);}
                let metadata=self.tools.skill_metadata();
                if !metadata.is_empty(){system.push_str(&format!("\nAvailable project skills (load body via skill tool only when relevant): {metadata}"));}

                let mut verify_rounds = 0u32;
                loop {
                    if let Some(compaction)=self.context.compact(&mut self.messages,self.llm,&model)? {
                        self.record.append("context.compacted",json!({"compaction":compaction})).map_err(|e|e.to_string())?;
                        self.record.append("messages.checkpoint",json!({"messages":self.messages})).map_err(|e|e.to_string())?;
                        emit(json!({"ev":"context.compacted","compaction":compaction}));
                    }
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
                            if ev["ev"]=="approval.resolved" { pending.retain(|p|p["id"]!=ev["id"]); }
                            emit(ev);
                        };
                        run_turn_live(
                            TurnCtx {
                                llm: self.llm,
                                tools: &mut self.tools,
                                record: &mut self.record,
                                guard: &mut self.guard,
                                policy: &self.policy,
                                messages: &mut self.messages,
                                system: &system,
                                model: &model,
                            },
                            &mut { sink },
                            &mut capture,
                            &self.control,
                            self.interactive,
                        )?
                    };

                    if !result.done {
                        continue;
                    }
                    if ["budget","cancelled","awaiting_approval"].contains(&result.reason) {
                        return Ok(json!({"done": true, "reason": result.reason}));
                    }
                    if let Some(check)=&self.check {
                        let verdict=crate::execution::run_check(&self.worktree,check).map_err(|e|e.to_string())?;
                        self.record.append("verify.completed",json!({"verdict":verdict})).map_err(|e|e.to_string())?;
                        emit(json!({"ev":"verify.completed","verdict":verdict}));
                        if !verdict.passed {
                            if verify_rounds>=self.max_verify_rounds {return Ok(json!({"done":false,"reason":"verify_failed","verdict":verdict}));}
                            verify_rounds+=1;
                            self.messages.push(json!({"role":"user","content":format!("External acceptance check failed. Fix the implementation, not the check.\n{}",verdict.output)}));
                            self.record.append("messages.checkpoint",json!({"messages":self.messages})).map_err(|e|e.to_string())?;
                            continue;
                        }
                        if self.journal.is_none() && self.candidate.is_none() {
                            return Ok(json!({"done":true,"reason":"final","text":result.text,"verified":true}));
                        }
                    }

                    // The gate: replay the oracle before accepting "finished".
                    if self.journal.is_some() != self.candidate.is_some() {return Err("differential verification requires both journal and candidate".into());}
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
                    self.messages.push(json!({"role":"tool","tool_call_id":id,"content":"denied by user"}));
                    self.record.append("messages.checkpoint",json!({"messages":self.messages})).map_err(|e|e.to_string())?;
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
                self.messages.push(json!({"role":"tool","tool_call_id":id,"content":clipped}));
                self.record.append("messages.checkpoint",json!({"messages":self.messages})).map_err(|e|e.to_string())?;
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

    pub fn serve(&mut self, input: impl BufRead + Send, out: impl Write + Send) -> std::io::Result<()> {
        use std::sync::{mpsc, Arc, Mutex};
        self.interactive = true;
        let output = Arc::new(Mutex::new(out));
        let control = self.control.clone();
        let (tx, rx) = mpsc::channel::<Value>();
        std::thread::scope(|scope| {
            let worker_output = output.clone();
            let worker = scope.spawn(move || -> std::io::Result<()> {
                for req in rx {
                    let id = req["id"].clone();
                    let method = req["method"].as_str().unwrap_or("");
                    if method == "turn" { self.control.reset(); }
                    let mut emit = |v: Value| {
                        if let Ok(mut out) = worker_output.lock() {
                            let _ = writeln!(out,"{v}"); let _ = out.flush();
                        }
                    };
                    let result = self.dispatch(method, &req["params"], &mut emit);
                    let reply = match result { Ok(v)=>json!({"id":id,"result":v}), Err(e)=>json!({"id":id,"error":e}) };
                    let mut out = worker_output.lock().map_err(|_|std::io::Error::other("output lock"))?;
                    writeln!(out,"{reply}")?; out.flush()?;
                }
                Ok(())
            });
            for line in input.lines() {
                let line = line?;
                let req: Value = match serde_json::from_str(&line) {
                    Ok(v)=>v,
                    Err(e)=>{ let mut out=output.lock().unwrap(); writeln!(out,"{}",json!({"error":e.to_string()}))?; out.flush()?; continue; }
                };
                let params = &req["params"];
                let immediate = match req["method"].as_str().unwrap_or("") {
                    "cancel" => { control.cancel(); Some(json!({"cancelled":true})) },
                    "steer" => { control.steer(params["input"].as_str().unwrap_or("").into()); Some(json!({"queued":true})) },
                    "approval.resolve" if control.waiting().as_deref()==params["id"].as_str() => {
                        control.resolve(params["id"].as_str().unwrap_or("").into(), params["allow"].as_bool().unwrap_or(false));
                        Some(json!({"queued":true}))
                    },
                    "ping"=>Some(json!({"pong":true})),
                    _=>None,
                };
                if let Some(result)=immediate {
                    let mut out=output.lock().unwrap(); writeln!(out,"{}",json!({"id":req["id"],"result":result}))?; out.flush()?;
                } else if tx.send(req).is_err() { break; }
            }
            control.close_input();
            drop(tx);
            worker.join().map_err(|_|std::io::Error::other("session worker panicked"))?
        })
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
        assert!(d.messages.iter().any(|m|m["role"]=="tool" && m["tool_call_id"]=="x" && m["content"].as_str().unwrap_or("").contains("approved")));
        assert!(Record::verify_chain(d.record.path()).is_ok());
        assert!(d.dispatch("approval.resolve", &json!({"id": "x", "allow": true}), &mut |_| {}).is_err(),
                "a resolved call cannot be resolved twice");
    }
}
