//! One round-trip per turn.
//!
//! Each tool result is appended to `messages` and `record` as soon as it
//! completes so a mid-batch approval pause never drops prior tool results or
//! corrupts `tool_call`/`tool` message pairing.

use serde_json::{json, Value};

use super::guard::{Guard, Level};
use super::policy::{Policy, Tier};
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
    pub record: &'a mut Record,
    pub messages: &'a mut Vec<Value>,
    pub control: &'a Control,
}

pub fn execute_recorded_call(
    env: ExecEnv<'_>,
    call: &ToolCall,
    tier: Tier,
    audit_sink: &mut dyn FnMut(&ToolCall, &str),
    emit: &mut dyn FnMut(Value),
) -> Result<String, String> {
    let ExecEnv {
        tools,
        record,
        messages,
        control,
    } = env;
    record
        .append(
            "tool.started",
            json!({"id": call.id, "tool": call.name, "args": call.args, "tier": tier.as_str()}),
        )
        .map_err(|e| e.to_string())?;
    emit(json!({"ev": "tool.started", "id": call.id, "tool": call.name, "args": call.args, "tier": tier.as_str()}));
    let output = if control.cancelled() {
        "cancelled before execution".into()
    } else {
        tools.run_live(call, control, emit)
    };
    let exit_code = tools.last_exit_code();
    record
        .append(
            "tool.result",
            json!({
                "id": call.id,
                "tool": call.name,
                "args": call.args,
                "tier": tier.as_str(),
                "output": output,
                "exit_code": exit_code
            }),
        )
        .map_err(|e| e.to_string())?;
    if call.name == "papercut" && output == "papercut recorded" {
        record
            .append("papercut", json!({"report": call.args, "call_id": call.id}))
            .map_err(|e| e.to_string())?;
        emit(json!({"ev": "papercut.filed", "report": call.args}));
    }
    if call.name == "todo" && output.starts_with("todo updated") {
        if let Some(items) = call.args.get("items") {
            record
                .append("todo.updated", json!({"items": items}))
                .map_err(|e| e.to_string())?;
        }
    }
    emit(json!({"ev": "tool.completed", "id": call.id, "bytes": output.len(), "exit_code": exit_code}));
    if tier == Tier::Audit {
        audit_sink(call, &output);
    }
    let clipped = tools.clip(&output);
    let message = json!({"role": "tool", "tool_call_id": call.id, "content": clipped});
    messages.push(message.clone());
    record
        .append("message", json!({"message": message}))
        .map_err(|e| e.to_string())?;
    Ok(clipped)
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

    let turns_used = messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .count() as u32
        + 1;

    let level = guard.tick(turns_used);
    if level != Level::Ok {
        record
            .append(
                "guard.tripped",
                json!({"level": level.as_str(), "turns_used": turns_used}),
            )
            .map_err(|e| e.to_string())?;
        emit(json!({"ev": "guard.tripped", "level": level.as_str()}));
    }
    match level {
        Level::Abort => {
            return Ok(TurnResult {
                done: true,
                reason: "budget",
                text: String::new(),
                applied_steering,
                deferred_calls: Vec::new(),
            });
        }
        Level::Narrow => tools.restrict_read_only(),
        Level::Warn | Level::Ok => {}
    }

    emit(json!({"ev": "turn.started", "model": model, "turns_used": turns_used}));
    let reply = llm.stream(
        model,
        system,
        messages,
        Some(&tools.schema()),
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
        let tier = if call.name == "bash" {
            policy.classify(
                call.args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )
        } else if matches!(call.name.as_str(), "edit" | "write") {
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

    let mut iter = planned.into_iter();
    while let Some((call, tier)) = iter.next() {
        if tier == Tier::Deny {
            if interactive {
                control.begin_approval(&call.id);
            }
            record
                .append(
                    "approval.requested",
                    json!({"id": call.id, "name": call.name, "args": call.args}),
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
                control.approval_decision(&call.id)
            } else {
                None
            };
            let Some((allow, remember)) = decision else {
                return Ok(TurnResult {
                    done: true,
                    reason: "awaiting_approval",
                    text: String::new(),
                    applied_steering,
                    deferred_calls: iter.collect(),
                });
            };
            if allow && remember && call.name == "bash" {
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
            }
            record
                .append(
                    "approval.resolved",
                    json!({"id": call.id, "allow": allow, "remember": remember}),
                )
                .map_err(|e| e.to_string())?;
            emit(json!({"ev": "approval.resolved", "id": call.id, "allow": allow, "remember": remember}));
            if !allow {
                record
                    .append(
                        "tool.result",
                        json!({"id": call.id, "tool": call.name, "args": call.args, "tier": "deny", "output": "denied by user", "exit_code": null}),
                    )
                    .map_err(|e| e.to_string())?;
                let message =
                    json!({"role": "tool", "tool_call_id": call.id, "content": "denied by user"});
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

    record
        .append("messages.checkpoint", json!({"messages": messages}))
        .map_err(|e| e.to_string())?;
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
