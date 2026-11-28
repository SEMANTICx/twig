//! One round-trip per turn.
//!
//! Each tool result is appended to `messages` and `record` as soon as it
//! completes so a mid-batch approval pause never drops prior tool results or
//! corrupts `tool_call`/`tool` message pairing.

use serde_json::{json, Value};

use super::guard::{Guard, Level};
use super::policy::{is_control_plane_path, is_sensitive_secret_path, Policy, Tier};
use super::tools::Tools;
use crate::control::Control;
use crate::llm::{Llm, ToolCall};
use crate::record::Record;

pub struct TurnResult {
    pub done: bool,
    pub reason: &'static str,
    pub text: String,
    /// Steering messages applied at the turn boundary so the caller can persist them as constraints.
    pub applied_steering: Vec<String>,
    /// Unstarted tool calls remaining in the batch when non-interactive approval pauses mid-batch.
    pub deferred_calls: Vec<(ToolCall, Tier)>,
}

pub struct TurnCtx<'a> {
    pub llm: &'a dyn Llm,
    pub tools: &'a mut Tools,
    pub record: &'a mut Record,
    pub guard: &'a mut Guard,
    pub policy: &'a Policy,
    pub messages: &'a mut Vec<Value>,
    pub system: &'a str,
    pub model: &'a str,
}

pub struct ExecEnv<'a> {
    pub tools: &'a mut Tools,
    pub guard: &'a mut Guard,
    pub record: &'a mut Record,
    pub messages: &'a mut Vec<Value>,
    pub control: &'a Control,
}

fn record_tool_outcome(
    env: &mut ExecEnv<'_>,
    call: &ToolCall,
    tier: Tier,
    output: String,
    exit_code: Option<i32>,
    audit_sink: &mut dyn FnMut(&ToolCall, &str),
    emit: &mut dyn FnMut(Value),
) -> Result<String, String> {
    let (stamp, multi_stamps) = if exit_code == Some(0) {
        env.tools.outcome_stamps(&call.name, &call.args)
    } else {
        (None, None)
    };
    let mut rec_row = json!({
        "id": call.id,
        "tool": call.name,
        "args": call.args,
        "tier": tier.as_str(),
        "output": output,
        "exit_code": exit_code
    });
    if let Some(obj) = rec_row.as_object_mut() {
        if let Some(s) = stamp {
            obj.insert("stamp".into(), s);
        }
        if let Some(ms) = multi_stamps {
            obj.insert("stamps".into(), ms);
        }
    }
    env.record
        .append("tool.result", rec_row)
        .map_err(|e| e.to_string())?;
    if call.name == "papercut" && output == "papercut recorded" {
        env.record
            .append("papercut", json!({"report": call.args, "call_id": call.id}))
            .map_err(|e| e.to_string())?;
        emit(json!({"ev": "papercut.filed", "report": call.args}));
    }
    if call.name == "todo" && output.starts_with("todo updated") {
        if let Some(items) = call.args.get("items").and_then(Value::as_array) {
            let (normalized, _) = super::tools::normalize_todo_items(items);
            env.record
                .append("todo.updated", json!({"items": normalized}))
                .map_err(|e| e.to_string())?;
        }
    }
    emit(json!({"ev": "tool.completed", "id": call.id, "bytes": output.len(), "exit_code": exit_code}));
    if tier == Tier::Audit {
        audit_sink(call, &output);
    }
    let mut clipped = env.tools.clip_for_call(&call.id, &output);
    if let Some(nudge) = env
        .guard
        .observe_tool_call(&call.name, &call.args, &output, exit_code)
    {
        env.record
            .append("guard.repetition", json!({"id": call.id, "tool": call.name}))
            .map_err(|e| e.to_string())?;
        emit(json!({"ev": "guard.repetition", "id": call.id, "tool": call.name}));
        clipped.push_str("\n\n");
        clipped.push_str(nudge);
    }
    let message = json!({"role": "tool", "tool_call_id": call.id, "content": clipped});
    env.messages.push(message.clone());
    env.record
        .append("message", json!({"message": message}))
        .map_err(|e| e.to_string())?;
    Ok(clipped)
}

fn forkable_messages(messages: &[Value]) -> &[Value] {
    if let Some((idx, last_asst)) = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| m["role"] == "assistant" && m.get("tool_calls").is_some())
    {
        let expected: Vec<&str> = last_asst["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["id"].as_str())
            .collect();
        let all_resolved = expected.iter().all(|want| {
            messages[idx + 1..]
                .iter()
                .any(|m| m["role"] == "tool" && m["tool_call_id"].as_str() == Some(*want))
        });
        if !all_resolved {
            return &messages[..idx];
        }
    }
    messages
}

fn patch_touches_gated_path(patch: &str) -> bool {
    patch.lines().any(|line| {
        line.trim()
            .strip_prefix("*** Add File:")
            .or_else(|| line.trim().strip_prefix("*** Update File:"))
            .or_else(|| line.trim().strip_prefix("*** Delete File:"))
            .is_some_and(|p| {
                let trimmed = p.trim();
                is_sensitive_secret_path(trimmed) || is_control_plane_path(trimmed)
            })
    })
}

pub fn execute_recorded_call(
    mut env: ExecEnv<'_>,
    call: &ToolCall,
    tier: Tier,
    audit_sink: &mut dyn FnMut(&ToolCall, &str),
    emit: &mut dyn FnMut(Value),
) -> Result<String, String> {
    env.record
        .append(
            "tool.started",
            json!({"id": call.id, "tool": call.name, "args": call.args, "tier": tier.as_str()}),
        )
        .map_err(|e| e.to_string())?;
    emit(json!({"ev": "tool.started", "id": call.id, "tool": call.name, "args": call.args, "tier": tier.as_str()}));
    let output = if env.control.cancelled() {
        "cancelled before execution".into()
    } else if call.name == "subagent" {
        let forkable = forkable_messages(env.messages);
        let mut batch = env.tools.run_subagents_batch(&[call], Some(forkable), emit);
        batch.pop().map(|(t, _)| t).unwrap_or_default()
    } else {
        env.tools.run_live(call, env.control, emit)
    };
    let exit_code = env.tools.last_exit_code();
    record_tool_outcome(&mut env, call, tier, output, exit_code, audit_sink, emit)
}

pub fn run_turn(
    ctx: TurnCtx<'_>,
    audit_sink: &mut dyn FnMut(&ToolCall, &str),
    emit: &mut dyn FnMut(Value),
) -> Result<TurnResult, String> {
    run_turn_live(ctx, audit_sink, emit, &Control::default(), false)
}

pub fn run_turn_live(
    ctx: TurnCtx<'_>,
    audit_sink: &mut dyn FnMut(&ToolCall, &str),
    emit: &mut dyn FnMut(Value),
    control: &Control,
    interactive: bool,
) -> Result<TurnResult, String> {
    let TurnCtx {
        llm,
        tools,
        record,
        guard,
        policy,
        messages,
        system,
        model,
    } = ctx;

    if control.cancelled() {
        return Ok(TurnResult {
            done: true,
            reason: "cancelled",
            text: String::new(),
            applied_steering: Vec::new(),
            deferred_calls: Vec::new(),
        });
    }

    let applied_steering = control.take_steering();
    for steering in &applied_steering {
        let message = json!({"role": "user", "content": steering});
        messages.push(message.clone());
        record
            .append("message", json!({"message": message}))
            .map_err(|e| e.to_string())?;
        record
            .append("steering.applied", json!({"text": steering}))
            .map_err(|e| e.to_string())?;
    }

    // Count model turns within the current user request (since the latest `role: "user"`
    // message) so typing "continue" / a follow-up prompt gets a fresh turn budget instead
    // of permanently bricking the session after a long audit.
    let since_last_user = messages
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .map_or(0, |idx| idx + 1);
    let turns_used = messages[since_last_user..]
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .count() as u32
        + 1;

    // Interactive TUI sessions get 3x headroom (120 turns at default `max_turns = 40`)
    // and a tool-free final synthesis turn when budget is reached so deep whole-repo audits
    // always deliver a complete report instead of cutting off mid-thought with empty output.
    let guard_turns = if interactive {
        turns_used.div_ceil(3)
    } else {
        turns_used
    };
    let level = guard.tick(guard_turns);
    if level != Level::Ok {
        record
            .append(
                "guard.tripped",
                json!({"level": level.as_str(), "turns_used": turns_used}),
            )
            .map_err(|e| e.to_string())?;
        emit(json!({"ev": "guard.tripped", "level": level.as_str()}));
    }
    let mut force_final_synthesis = false;
    match level {
        Level::Abort => {
            let already_synthesized = messages[since_last_user..].iter().any(|m| {
                m.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains("<budget-synthesis>"))
            });
            if interactive && !already_synthesized {
                force_final_synthesis = true;
                messages.push(json!({
                    "role": "system",
                    "content": "<system-reminder>\n<budget-synthesis>Turn budget for this request is reached. Tools are now disabled — synthesize all verified evidence gathered so far and write your complete, structured final answer to the user now.</budget-synthesis>\n</system-reminder>"
                }));
            } else {
                return Ok(TurnResult {
                    done: true,
                    reason: "budget",
                    text: String::new(),
                    applied_steering,
                    deferred_calls: Vec::new(),
                });
            }
        }
        Level::Narrow => tools.restrict_read_only(),
        Level::Warn | Level::Ok => {}
    }

    emit(json!({"ev": "turn.started", "model": model, "turns_used": turns_used}));
    let schema_opt = (!force_final_synthesis).then(|| tools.schema());
    let reply = llm.stream(
        model,
        system,
        messages,
        schema_opt.as_ref(),
        control,
        emit,
    )?;
    record
        .append(
            "model.reply",
            json!({
                "model": model,
                "text": reply.text,
                "usage": reply.usage,
                "tool_calls": reply.calls.iter()
                    .map(|c| json!({"id": c.id, "name": c.name, "args": c.args}))
                    .collect::<Vec<_>>(),
            }),
        )
        .map_err(|e| e.to_string())?;
    emit(json!({"ev": "model.usage", "model": model, "usage": reply.usage}));
    let mut assistant = json!({
        "role": "assistant",
        "content": reply.text,
        "tool_calls": reply.calls.iter()
            .map(|c| json!({
                "id": c.id,
                "type": "function",
                "function": {"name": c.name, "arguments": c.args.to_string()}
            }))
            .collect::<Vec<_>>()
    });
    if reply.calls.is_empty() {
        assistant.as_object_mut().unwrap().remove("tool_calls");
    }
    messages.push(assistant.clone());
    record
        .append("message", json!({"message": assistant}))
        .map_err(|e| e.to_string())?;

    if reply.calls.is_empty() {
        record
            .append("turn.completed", json!({"reason": "final"}))
            .map_err(|e| e.to_string())?;
        emit(json!({"ev": "turn.completed", "reason": "final", "text": reply.text, "usage": reply.usage}));
        return Ok(TurnResult {
            done: true,
            reason: "final",
            text: reply.text,
            applied_steering,
            deferred_calls: Vec::new(),
        });
    }

    let mut risk = serde_json::Map::new();
    let mut planned = Vec::with_capacity(reply.calls.len());
    for call in reply.calls {
        let tier = if call.name == "bash"
            || (call.name == "task" && call.args["action"].as_str() == Some("start"))
        {
            policy.classify(
                call.args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )
        } else if (matches!(
            call.name.as_str(),
            "read" | "edit" | "write" | "grep" | "outline"
        ) && call
            .args
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(is_sensitive_secret_path))
            || (matches!(call.name.as_str(), "edit" | "write")
                && call
                    .args
                    .get("path")
                    .and_then(Value::as_str)
                    .is_some_and(is_control_plane_path))
            || (call.name == "apply_patch"
                && call
                    .args
                    .get("patch")
                    .and_then(Value::as_str)
                    .is_some_and(patch_touches_gated_path))
            || (matches!(call.name.as_str(), "edit" | "write" | "apply_patch")
                && tools.gates_edits())
        {
            Tier::Deny
        } else if matches!(call.name.as_str(), "edit" | "write" | "apply_patch")
            || (call.name == "mcp"
                && matches!(call.args["action"].as_str().unwrap_or("call"), "call" | ""))
        {
            Tier::Audit
        } else {
            Tier::Auto
        };
        risk.insert(call.id.clone(), json!(tier.as_str()));
        planned.push((call, tier));
    }
    let (hits, misses, size) = policy.stats();
    emit(json!({
        "ev": "tool.planned",
        "risk": Value::Object(risk),
        "policy": {"hits": hits, "misses": misses, "size": size}
    }));

    let mut idx = 0;
    while idx < planned.len() {
        // Fan out consecutive Auto read-only calls (`read`, `grep`, `glob`, `skill`, `Tier::Auto bash`)
        // in parallel while preserving exact deterministic record & message ordering.
        if !control.cancelled() && Tools::is_parallel_readonly(&planned[idx].0, planned[idx].1) {
            let mut end = idx + 1;
            while end < planned.len()
                && Tools::is_parallel_readonly(&planned[end].0, planned[end].1)
            {
                end += 1;
            }
            if end - idx >= 2 {
                let slice = &planned[idx..end];
                for (call, tier) in slice {
                    record
                        .append(
                            "tool.started",
                            json!({"id": call.id, "tool": call.name, "args": call.args, "tier": tier.as_str()}),
                        )
                        .map_err(|e| e.to_string())?;
                    emit(json!({"ev": "tool.started", "id": call.id, "tool": call.name, "args": call.args, "tier": tier.as_str()}));
                }
                let refs: Vec<&ToolCall> = slice.iter().map(|(c, _)| c).collect();
                let outcomes = tools.run_readonly_batch(&refs, control);
                let mut env = ExecEnv {
                    tools,
                    guard,
                    record,
                    messages,
                    control,
                };
                for ((call, tier), (output, exit_code)) in slice.iter().zip(outcomes) {
                    record_tool_outcome(
                        &mut env, call, *tier, output, exit_code, audit_sink, emit,
                    )?;
                }
                idx = end;
                continue;
            }
        }
        // Fan out consecutive Auto subagent calls in parallel while preserving
        // exact record & message ordering.
        if !control.cancelled()
            && planned[idx].0.name == "subagent"
            && planned[idx].1 == Tier::Auto
        {
            let mut end = idx + 1;
            while end < planned.len()
                && planned[end].0.name == "subagent"
                && planned[end].1 == Tier::Auto
            {
                end += 1;
            }
            if end - idx >= 2 {
                let slice = &planned[idx..end];
                for (call, tier) in slice {
                    record
                        .append(
                            "tool.started",
                            json!({"id": call.id, "tool": call.name, "args": call.args, "tier": tier.as_str()}),
                        )
                        .map_err(|e| e.to_string())?;
                    emit(json!({"ev": "tool.started", "id": call.id, "tool": call.name, "args": call.args, "tier": tier.as_str()}));
                }
                let refs: Vec<&ToolCall> = slice.iter().map(|(c, _)| c).collect();
                let forkable = forkable_messages(messages);
                let outcomes = tools.run_subagents_batch(&refs, Some(forkable), emit);
                let mut env = ExecEnv {
                    tools,
                    guard,
                    record,
                    messages,
                    control,
                };
                for ((call, tier), (output, exit_code)) in slice.iter().zip(outcomes) {
                    record_tool_outcome(
                        &mut env, call, *tier, output, exit_code, audit_sink, emit,
                    )?;
                }
                idx = end;
                continue;
            }
        }

        let (call, tier) = planned[idx].clone();
        idx += 1;
        if tier == Tier::Deny {
            if interactive {
                control.begin_approval(&call.id);
            }
            record
                .append(
                    "approval.requested",
                    json!({"id": call.id, "name": call.name, "args": call.args, "paused": !interactive}),
                )
                .map_err(|e| e.to_string())?;
            emit(json!({"ev": "approval.requested", "id": call.id, "name": call.name, "args": call.args}));
            record
                .append(
                    "messages.checkpoint",
                    json!({"messages": messages, "pending_call": {"id": call.id, "name": call.name, "args": call.args}}),
                )
                .map_err(|e| e.to_string())?;
            let decision = if interactive {
                control.wait_approval(&call.id)
            } else {
                None
            };
            let Some((allow, remember, note)) = decision else {
                return Ok(TurnResult {
                    done: true,
                    reason: "awaiting_approval",
                    text: String::new(),
                    applied_steering,
                    deferred_calls: planned[idx..].to_vec(),
                });
            };
            if allow && remember {
                if matches!(call.name.as_str(), "bash" | "task") {
                    let cmd = call
                        .args
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let pattern = policy.allow_for_session(cmd);
                    record
                        .append("policy.allowed", json!({"pattern": pattern}))
                        .map_err(|e| e.to_string())?;
                    emit(json!({"ev": "policy.allowed", "pattern": pattern}));
                } else if matches!(call.name.as_str(), "edit" | "write" | "apply_patch") {
                    let (ro, _) = tools.configure_mode(false, false);
                    emit(json!({
                        "ev": "mode.changed",
                        "plan": false,
                        "gate_edits": false,
                        "read_only": ro
                    }));
                }
            }
            let clean_note = note.as_deref().map(str::trim).filter(|s| !s.is_empty());
            record
                .append(
                    "approval.resolved",
                    json!({"id": call.id, "allow": allow, "remember": remember, "note": clean_note}),
                )
                .map_err(|e| e.to_string())?;
            emit(json!({"ev": "approval.resolved", "id": call.id, "allow": allow, "remember": remember, "note": clean_note}));
            if !allow {
                let deny_out = match clean_note {
                    Some(n) => format!("denied by user — do this instead: {n}"),
                    None => "denied by user".to_owned(),
                };
                record
                    .append(
                        "tool.result",
                        json!({"id": call.id, "tool": call.name, "args": call.args, "tier": "deny", "output": deny_out, "exit_code": null}),
                    )
                    .map_err(|e| e.to_string())?;
                let message =
                    json!({"role": "tool", "tool_call_id": call.id, "content": deny_out});
                messages.push(message.clone());
                record
                    .append("message", json!({"message": message}))
                    .map_err(|e| e.to_string())?;
                continue;
            }
        }

        execute_recorded_call(
            ExecEnv {
                tools,
                guard,
                record,
                messages,
                control,
            },
            &call,
            tier,
            audit_sink,
            emit,
        )?;
    }

    emit(json!({
        "ev": "turn.completed",
        "reason": "tools",
        "round_trips": llm.calls()
    }));
    Ok(TurnResult {
        done: false,
        reason: "tools",
        text: String::new(),
        applied_steering,
        deferred_calls: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::stub::{call, StubLlm};
    use crate::llm::{summarize_usage, Usage};

    fn dir(tag: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let d = std::env::temp_dir().join(format!(
            "twig-turn-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("note.txt"), "hello\n").unwrap();
        d
    }

    #[test]
    fn four_tool_calls_cost_one_model_call() {
        let dir = dir("rtt");
        let llm = StubLlm::new(vec![StubLlm::with_tools(
            "",
            vec![
                call("a", "bash", json!({"command": "cat note.txt"})),
                call("b", "bash", json!({"command": "ls"})),
                call("c", "read", json!({"path": "note.txt"})),
                call("e", "bash", json!({"command": "make deploy"})),
            ],
        )]);
        let mut tools = Tools::new(&dir);
        let mut record = Record::open(dir.join("r.jsonl"), "s").unwrap();
        let mut guard = Guard::new(&dir, 40, 99, 1);
        let policy = Policy::new();
        let mut messages = Vec::new();
        let mut audited: Vec<String> = Vec::new();
        let mut sink = |c: &ToolCall, _o: &str| audited.push(c.id.clone());

        let result = run_turn(
            TurnCtx {
                llm: &llm,
                tools: &mut tools,
                record: &mut record,
                guard: &mut guard,
                policy: &policy,
                messages: &mut messages,
                system: "sys",
                model: "m",
            },
            &mut sink,
            &mut |_e| {},
        )
        .unwrap();

        assert_eq!(llm.calls(), 1, "four tool calls must share one round-trip");
        assert!(!result.done);
        assert_eq!(
            audited,
            vec!["e"],
            "only the Audit tier is recorded out of band"
        );
        assert!(messages.iter().any(|m| m["role"] == json!("tool")));
    }

    #[test]
    fn hard_deny_pauses_noninteractive_turn_and_preserves_prior_tool_results() {
        let dir = dir("deny-pauses");
        let llm = StubLlm::new(vec![StubLlm::with_tools(
            "",
            vec![
                call("ok", "read", json!({"path": "note.txt"})),
                call("deny", "bash", json!({"command": "rm -rf /"})),
                call("later", "bash", json!({"command": "ls"})),
            ],
        )]);
        let mut tools = Tools::new(&dir);
        let mut record = Record::open(dir.join("r.jsonl"), "deny").unwrap();
        let mut guard = Guard::new(&dir, 40, 99, 1);
        let mut messages = Vec::new();
        let result = run_turn(
            TurnCtx {
                llm: &llm,
                tools: &mut tools,
                record: &mut record,
                guard: &mut guard,
                policy: &Policy::new(),
                messages: &mut messages,
                system: "sys",
                model: "m",
            },
            &mut |_, _| {},
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(result.reason, "awaiting_approval");
        assert_eq!(llm.calls(), 1);
        assert_eq!(result.deferred_calls.len(), 1);
        assert_eq!(result.deferred_calls[0].0.id, "later");
        assert!(
            messages
                .iter()
                .any(|m| m["role"] == "tool" && m["tool_call_id"] == "ok"),
            "earlier tool results in the same batch must survive a mid-batch pause"
        );
        let rows = record.tail(20).unwrap();
        assert!(rows.iter().any(|row| row["kind"] == "approval.requested"));
        assert!(!rows
            .iter()
            .any(|row| row["kind"] == "tool.result" && row["id"] == "deny"));
    }

    #[test]
    fn no_tool_calls_finishes_the_turn() {
        let dir = dir("final");
        let llm = StubLlm::new(vec![StubLlm::text("all done")]);
        let mut tools = Tools::new(&dir);
        let mut record = Record::open(dir.join("r.jsonl"), "s").unwrap();
        let mut guard = Guard::new(&dir, 40, 99, 1);
        let result = run_turn(
            TurnCtx {
                llm: &llm,
                tools: &mut tools,
                record: &mut record,
                guard: &mut guard,
                policy: &Policy::new(),
                messages: &mut Vec::new(),
                system: "sys",
                model: "m",
            },
            &mut |_, _| {},
            &mut |_e| {},
        )
        .unwrap();
        assert!(result.done);
        assert_eq!(result.text, "all done");
    }

    #[test]
    fn budget_stops_without_asking_the_model() {
        let dir = dir("budget");
        let llm = StubLlm::new(vec![StubLlm::text("should not be reached")]);
        let mut tools = Tools::new(&dir);
        let mut record = Record::open(dir.join("r.jsonl"), "s").unwrap();
        let mut guard = Guard::new(&dir, 1, 99, 1);
        let mut messages = vec![json!({"role": "assistant", "content": "prior"})];
        let result = run_turn(
            TurnCtx {
                llm: &llm,
                tools: &mut tools,
                record: &mut record,
                guard: &mut guard,
                policy: &Policy::new(),
                messages: &mut messages,
                system: "sys",
                model: "m",
            },
            &mut |_, _| {},
            &mut |_e| {},
        )
        .unwrap();
        assert_eq!(result.reason, "budget");
        assert_eq!(llm.calls(), 0, "an exhausted budget must not spend a call");

        // A follow-up user turn ("继续") must receive a fresh per-request budget rather than bricking the session.
        messages.push(json!({"role": "user", "content": "继续"}));
        let llm2 = StubLlm::new(vec![StubLlm::text("resumed cleanly")]);
        let mut guard2 = Guard::new(&dir, 2, 99, 1);
        let resumed = run_turn(
            TurnCtx {
                llm: &llm2,
                tools: &mut tools,
                record: &mut record,
                guard: &mut guard2,
                policy: &Policy::new(),
                messages: &mut messages,
                system: "sys",
                model: "m",
            },
            &mut |_, _| {},
            &mut |_e| {},
        )
        .unwrap();
        assert_eq!(resumed.reason, "final");
        assert_eq!(resumed.text, "resumed cleanly");
        assert_eq!(llm2.calls(), 1);

        // Interactive TUI mode performs a tool-free final synthesis turn instead of dropping empty output on budget limit.
        let mut msgs_int = vec![
            json!({"role": "user", "content": "audit all"}),
            json!({"role": "assistant", "content": "s1"}),
            json!({"role": "assistant", "content": "s2"}),
        ];
        let llm3 = StubLlm::new(vec![StubLlm::text("complete final audit report")]);
        let mut guard3 = Guard::new(&dir, 1, 99, 1);
        let ctrl = Control::default();
        let synth = run_turn_live(
            TurnCtx {
                llm: &llm3,
                tools: &mut tools,
                record: &mut record,
                guard: &mut guard3,
                policy: &Policy::new(),
                messages: &mut msgs_int,
                system: "sys",
                model: "m",
            },
            &mut |_, _| {},
            &mut |_e| {},
            &ctrl,
            true,
        )
        .unwrap();
        assert!(synth.done);
        assert_eq!(synth.reason, "final");
        assert_eq!(synth.text, "complete final audit report");
    }

    #[test]
    fn filed_papercut_and_provider_usage_survive_the_record_boundary() {
        let dir = dir("papercut");
        let report = json!({
            "component": "bash",
            "attempted": "run tests",
            "expected": "partial output",
            "actual": "output disappeared",
            "recovery": "preserve output on timeout"
        });
        let mut reply = StubLlm::with_tools("", vec![call("p", "papercut", report.clone())]);
        reply.usage = Usage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            cached_input_tokens: Some(80),
        };
        let llm = StubLlm::new(vec![reply]);
        let mut tools = Tools::new(&dir);
        let mut record = Record::open(dir.join("record.jsonl"), "p").unwrap();
        let mut guard = Guard::new(&dir, 40, 99, 1);
        run_turn(
            TurnCtx {
                llm: &llm,
                tools: &mut tools,
                record: &mut record,
                guard: &mut guard,
                policy: &Policy::new(),
                messages: &mut Vec::new(),
                system: "s",
                model: "m",
            },
            &mut |_, _| {},
            &mut |_| {},
        )
        .unwrap();
        let rows = record.tail(20).unwrap();
        assert!(rows
            .iter()
            .any(|r| r["kind"] == "papercut" && r["report"] == report));
        let usage = summarize_usage(&rows);
        assert_eq!(usage["input_tokens"], 100);
        assert_eq!(usage["complete"], true);
    }
}
