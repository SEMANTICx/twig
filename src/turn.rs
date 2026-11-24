//! One round-trip per turn.
//!
//! Codex-shaped interaction, minus the tax:
//!   * the model is called once per turn (N tool calls, 1 RTT)
//!   * risk is classified at plan time in one pass, capped at a dict lookup
//!   * `Deny` is the only tier that blocks; `Audit` runs and is recorded
//!   * a guard level degrades the toolset, it never silently ends the turn

use serde_json::{json, Value};

use crate::guard::{Guard, Level};
use crate::policy::{Policy, Tier};
use crate::provider::{Llm, ToolCall};
use crate::record::Record;
use crate::tools::Tools;

pub struct TurnResult {
    pub done: bool,
    pub reason: &'static str,
    pub text: String,
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

pub fn run_turn(
    ctx: TurnCtx<'_>,
    audit_sink: &mut dyn FnMut(&ToolCall, &str),
    emit: &mut dyn FnMut(Value),
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

    let turns_used = messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .count() as u32
        + 1;

    let level = guard.tick(turns_used);
    if level != Level::Ok {
        // Recorded, not only emitted: failure classification reads the record,
        // and a trip that lives only on stdout is invisible to it.
        record
            .append("guard.tripped", json!({"level": level.as_str(), "turns_used": turns_used}))
            .map_err(|e| e.to_string())?;
        emit(json!({"ev": "guard.tripped", "level": level.as_str()}));
    }
    match level {
        Level::Abort => {
            return Ok(TurnResult { done: true, reason: "budget", text: String::new() });
        }
        Level::Narrow => tools.read_only = true,
        Level::Warn | Level::Ok => {}
    }

    emit(json!({"ev": "turn.started", "model": model, "turns_used": turns_used}));
    let reply = llm.complete(model, system, messages, Some(&tools.schema()))?;
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

    if reply.calls.is_empty() {
        record
            .append("turn.completed", json!({"reason": "final"}))
            .map_err(|e| e.to_string())?;
        emit(json!({"ev": "turn.completed", "reason": "final", "text": reply.text}));
        return Ok(TurnResult { done: true, reason: "final", text: reply.text });
    }

    // Plan-time classification: one pass, cached by shape, never in the exec path.
    let mut risk = serde_json::Map::new();
    let mut tiers = Vec::with_capacity(reply.calls.len());
    for call in &reply.calls {
        let tier = if call.name == "bash" {
            policy.classify(call.args.get("command").and_then(Value::as_str).unwrap_or(""))
        } else {
            Tier::Auto
        };
        risk.insert(call.id.clone(), json!(tier.as_str()));
        tiers.push(tier);
    }
    let (hits, misses, size) = policy.stats();
    emit(json!({
        "ev": "tool.planned",
        "risk": Value::Object(risk),
        "policy": {"hits": hits, "misses": misses, "size": size}
    }));

    let mut results: Vec<Value> = Vec::with_capacity(reply.calls.len());
    for (call, tier) in reply.calls.iter().zip(tiers) {
        if tier == Tier::Deny {
            emit(json!({"ev": "approval.requested", "id": call.id, "args": call.args}));
            record
                .append(
                    "approval.requested",
                    json!({"id": call.id, "args": call.args}),
                )
                .map_err(|e| e.to_string())?;
            results.push(json!({
                "tool_call_id": call.id,
                "content": "blocked: hard-deny, needs approval"
            }));
            continue;
        }

        emit(json!({"ev": "tool.started", "id": call.id, "tier": tier.as_str()}));
        let output = tools.run(call);
        record
            .append(
                "tool.result",
                json!({"id": call.id, "tool": call.name, "args": call.args,
                       "tier": tier.as_str(), "output": output, "exit_code": tools.last_exit_code()}),
            )
            .map_err(|e| e.to_string())?;
        if call.name == "papercut" && output == "papercut recorded" {
            record.append("papercut", json!({"report": call.args, "call_id": call.id}))
                .map_err(|e| e.to_string())?;
            emit(json!({"ev": "papercut.filed", "report": call.args}));
        }
        emit(json!({"ev": "tool.completed", "id": call.id, "bytes": output.len()}));

        if tier == Tier::Audit {
            audit_sink(call, &output); // out of band, never awaited
        }
        results.push(json!({
            "tool_call_id": call.id,
            "content": tools.clip(&output)
        }));
    }

    let assistant_calls: Vec<Value> = reply
        .calls
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "type": "function",
                "function": {"name": c.name, "arguments": c.args.to_string()}
            })
        })
        .collect();
    messages.push(json!({
        "role": "assistant",
        "content": reply.text,
        "tool_calls": assistant_calls
    }));
    for item in &results {
        messages.push(json!({
            "role": "tool",
            "tool_call_id": item["tool_call_id"],
            "content": item["content"]
        }));
    }

    emit(json!({
        "ev": "turn.completed",
        "reason": "tools",
        "round_trips": llm.calls()
    }));
    Ok(TurnResult { done: false, reason: "tools", text: String::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stub::{call, StubLlm};

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
                call("d", "bash", json!({"command": "rm -rf /"})),
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
        // cat/ls are allow-listed (Auto) and read is confined (Auto): neither is
        // audited. `make deploy` is Audit. `rm -rf /` is Deny and must not run.
        assert_eq!(audited, vec!["e"], "only the Audit tier is recorded out of band");
        assert!(messages.iter().any(|m| m["role"] == json!("tool")));
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
        let mut guard = Guard::new(&dir, 1, 99, 1); // budget of one turn
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
        let report = json!({"component":"bash","attempted":"run tests","expected":"partial output",
            "actual":"output disappeared","recovery":"preserve output on timeout"});
        let mut reply = StubLlm::with_tools("", vec![call("p", "papercut", report.clone())]);
        reply.usage = crate::telemetry::Usage { input_tokens: Some(100), output_tokens: Some(20), cached_input_tokens: Some(80) };
        let llm = StubLlm::new(vec![reply]);
        let mut tools = Tools::new(&dir);
        let mut record = Record::open(dir.join("record.jsonl"), "p").unwrap();
        let mut guard = Guard::new(&dir, 40, 99, 1);
        run_turn(TurnCtx { llm: &llm, tools: &mut tools, record: &mut record, guard: &mut guard,
            policy: &Policy::new(), messages: &mut Vec::new(), system: "s", model: "m" },
            &mut |_, _| {}, &mut |_| {}).unwrap();
        let rows = record.tail(20).unwrap();
        assert!(rows.iter().any(|r| r["kind"] == "papercut" && r["report"] == report));
        let usage = crate::telemetry::summarize(&rows);
        assert_eq!(usage["input_tokens"], 100);
        assert_eq!(usage["complete"], true);
    }
}
