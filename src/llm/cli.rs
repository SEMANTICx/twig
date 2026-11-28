//! Canonical local CLI runner bridge (`cli:codex`, `cli:claude`, `cli:agy`, `cli:gemini`).
//!
//! Single source of truth for parsing `cli:<engine>[:<submodel>]` specs, discovering
//! installed CLI binaries, constructing read-only vs full-auto `Command` invocations,
//! and executing bounded process-group-reaped CLI completions.

use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

use super::types::{Reply, Usage};
use crate::control::Control;
use crate::exec::run_process_cancellable;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliEngine {
    Codex,
    Claude,
    Agy,
    Gemini,
}

impl CliEngine {
    const ALL: [Self; 4] = [Self::Codex, Self::Claude, Self::Agy, Self::Gemini];

    fn binary(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Agy => "agy",
            Self::Gemini => "gemini",
        }
    }

    fn spec_id(self) -> &'static str {
        match self {
            Self::Codex => "cli:codex",
            Self::Claude => "cli:claude",
            Self::Agy => "cli:agy",
            Self::Gemini => "cli:gemini",
        }
    }
}

/// Probe installed local CLI runners (`codex`, `claude`, `agy`, `gemini`) for `/model`.
pub(crate) fn detected_runners() -> Vec<String> {
    let mut out = Vec::new();
    for engine in CliEngine::ALL {
        let found = Command::new(engine.binary())
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if found {
            out.push(engine.spec_id().to_owned());
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliSpec {
    engine: CliEngine,
    submodel: Option<String>,
}

pub(crate) fn is_cli_model(model: &str) -> bool {
    model.trim().starts_with("cli:")
}

/// Construct a `Command` for a `cli:<engine>[:<submodel>]` spec with consistent
/// read-only vs full-auto flags, working directory, submodel, and reasoning effort.
pub(crate) fn build_command(
    raw_spec: &str,
    prompt: &str,
    read_only: bool,
    workdir: Option<&Path>,
    effort: Option<&str>,
) -> Result<Command, String> {
    let spec = CliSpec::parse(raw_spec)?;
    Ok(spec.command(prompt, read_only, false, workdir, effort))
}

impl CliSpec {
    fn parse(raw: &str) -> Result<Self, String> {
        let rest = raw.trim().strip_prefix("cli:").unwrap_or(raw.trim());
        let (engine_raw, submodel) = match rest.split_once(':') {
            Some((e, m)) if !m.trim().is_empty() => (e.trim(), Some(m.trim().to_owned())),
            Some((e, _)) => (e.trim(), None),
            None => (rest.trim(), None),
        };
        let engine = match engine_raw {
            "codex" => CliEngine::Codex,
            "claude" => CliEngine::Claude,
            "agy" | "antigravity" => CliEngine::Agy,
            "gemini" => CliEngine::Gemini,
            other => {
                return Err(format!(
                    "unsupported CLI model {other:?}; expected cli:codex, cli:claude, cli:agy, or cli:gemini"
                ));
            }
        };
        Ok(Self { engine, submodel })
    }

    fn command(
        &self,
        prompt: &str,
        read_only: bool,
        stream_json: bool,
        workdir: Option<&Path>,
        effort: Option<&str>,
    ) -> Command {
        let mut cmd = Command::new(self.engine.binary());
        match self.engine {
            CliEngine::Codex => {
                cmd.args(["exec", "--skip-git-repo-check"]);
                if let Some(dir) = workdir {
                    cmd.arg("-C").arg(dir);
                }
                if stream_json {
                    cmd.arg("--json");
                }
                if read_only {
                    cmd.args(["--sandbox", "read-only"]);
                } else {
                    cmd.arg("--full-auto");
                }
                if let Some(ref m) = self.submodel {
                    cmd.args(["-m", m]);
                }
                cmd.arg(prompt);
            }
            CliEngine::Claude => {
                cmd.args(["-p", prompt]);
                if stream_json {
                    cmd.args(["--output-format", "stream-json", "--verbose"]);
                }
                if read_only {
                    cmd.args(["--permission-mode", "plan"]);
                } else {
                    cmd.arg("--dangerously-skip-permissions");
                }
                if let Some(ref m) = self.submodel {
                    cmd.args(["--model", m]);
                }
            }
            CliEngine::Agy => {
                cmd.args(["-p", prompt, "--disable-slash-commands"]);
                if read_only {
                    cmd.args(["--mode", "plan"]);
                } else {
                    cmd.arg("--dangerously-skip-permissions");
                }
                if let Some(ref m) = self.submodel {
                    cmd.args(["--model", m]);
                }
                if let Some(eff) = effort
                    .map(str::trim)
                    .filter(|e| matches!(*e, "low" | "medium" | "high" | "xhigh"))
                {
                    cmd.args(["--effort", eff]);
                }
            }
            CliEngine::Gemini => {
                cmd.args(["-p", prompt]);
                if !read_only {
                    cmd.arg("--yolo");
                }
                if let Some(ref m) = self.submodel {
                    cmd.args(["-m", m]);
                }
            }
        }
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        cmd
    }
}

fn format_cli_prompt(system: &str, messages: &[Value], read_only: bool) -> String {
    let mut sections = Vec::new();
    if read_only {
        sections.push(
            "[Mode: strict read-only — do NOT modify, create, or delete files.]".to_owned(),
        );
    }
    let sys_trimmed = system.trim();
    if !sys_trimmed.is_empty() && sys_trimmed.len() <= 4_000 {
        sections.push(format!("[System]\n{sys_trimmed}"));
    }
    let mut tail = Vec::new();
    let mut budget = 48_000usize;
    for msg in messages.iter().rev() {
        let role = msg["role"].as_str().unwrap_or("user");
        let content = msg["content"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| msg["content"].to_string());
        let trimmed = content.trim();
        if trimmed.is_empty() || trimmed == "null" {
            continue;
        }
        let clipped: String = trimmed.chars().take(16_000).collect();
        let entry = format!("[{role}]\n{clipped}");
        if entry.len() > budget && !tail.is_empty() {
            break;
        }
        budget = budget.saturating_sub(entry.len());
        tail.push(entry);
    }
    tail.reverse();
    sections.extend(tail);
    sections.join("\n\n")
}

fn absorb_cli_stream_line(
    engine: CliEngine,
    line: &str,
    acc_text: &mut String,
    usage: &mut Usage,
    emit: &mut Option<&mut dyn FnMut(Value)>,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if let Some(u) = v.get("usage") {
            let parsed = Usage::parse(u);
            if parsed.input_tokens.is_some() {
                usage.input_tokens = parsed.input_tokens;
            }
            if parsed.output_tokens.is_some() {
                usage.output_tokens = parsed.output_tokens;
            }
            if parsed.cached_input_tokens.is_some() {
                usage.cached_input_tokens = parsed.cached_input_tokens;
            }
        }
        match engine {
            CliEngine::Codex => {
                if v["type"] == "item.completed" {
                    let item = &v["item"];
                    match item["type"].as_str() {
                        Some("agent_message") => {
                            if let Some(t) = item["text"].as_str().filter(|s| !s.is_empty()) {
                                if !acc_text.is_empty() {
                                    acc_text.push('\n');
                                }
                                acc_text.push_str(t);
                                if let Some(ref mut sink) = emit {
                                    sink(json!({"ev": "model.delta", "text": format!("{t}\n")}));
                                }
                            }
                        }
                        Some("reasoning") => {
                            if let Some(t) = item["text"].as_str().filter(|s| !s.is_empty()) {
                                if let Some(ref mut sink) = emit {
                                    sink(json!({"ev": "model.reasoning", "text": format!("{t}\n")}));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                return;
            }
            CliEngine::Claude => {
                match v["type"].as_str() {
                    Some("stream_event") => {
                        let delta = &v["event"]["delta"];
                        match delta["type"].as_str() {
                            Some("text_delta") => {
                                if let Some(t) = delta["text"].as_str().filter(|s| !s.is_empty()) {
                                    acc_text.push_str(t);
                                    if let Some(ref mut sink) = emit {
                                        sink(json!({"ev": "model.delta", "text": t}));
                                    }
                                }
                            }
                            Some("thinking_delta") => {
                                if let Some(t) =
                                    delta["thinking"].as_str().filter(|s| !s.is_empty())
                                {
                                    if let Some(ref mut sink) = emit {
                                        sink(json!({"ev": "model.reasoning", "text": t}));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("assistant") if acc_text.is_empty() => {
                        for block in v["message"]["content"].as_array().into_iter().flatten() {
                            if block["type"] == "text" {
                                if let Some(t) = block["text"].as_str().filter(|s| !s.is_empty()) {
                                    acc_text.push_str(t);
                                    if let Some(ref mut sink) = emit {
                                        sink(json!({"ev": "model.delta", "text": t}));
                                    }
                                }
                            }
                        }
                    }
                    Some("result") if acc_text.is_empty() => {
                        if let Some(r) = v["result"].as_str().filter(|s| !s.is_empty()) {
                            acc_text.push_str(r);
                            if let Some(ref mut sink) = emit {
                                sink(json!({"ev": "model.delta", "text": r}));
                            }
                        }
                    }
                    _ => {}
                }
                return;
            }
            CliEngine::Agy | CliEngine::Gemini => {}
        }
    }

    if !acc_text.is_empty() {
        acc_text.push('\n');
    }
    let clean = line.trim_end_matches(['\r', '\n']);
    acc_text.push_str(clean);
    if let Some(ref mut sink) = emit {
        sink(json!({"ev": "model.delta", "text": format!("{clean}\n")}));
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn complete(
    cli_spec: &str,
    system: &str,
    messages: &[Value],
    tools: Option<&Value>,
    effort: Option<&str>,
    timeout: Duration,
    control: Option<&Control>,
    mut emit: Option<&mut dyn FnMut(Value)>,
) -> Result<Reply, String> {
    let spec = CliSpec::parse(cli_spec)?;
    let read_only = tools.is_none()
        || system.contains("Plan mode")
        || system.contains("read-only")
        || messages.iter().any(|m| {
            m["role"] == "system"
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("Plan mode is active") || s.contains("read-only"))
        });
    let prompt = format_cli_prompt(system, messages, read_only);
    let mut cmd = spec.command(&prompt, read_only, emit.is_some(), None, effort);
    let mut acc_text = String::new();
    let mut usage = Usage::default();

    let capture = run_process_cancellable(
        &mut cmd,
        &[],
        timeout,
        2 * 1024 * 1024,
        control,
        &mut |line| {
            absorb_cli_stream_line(spec.engine, line, &mut acc_text, &mut usage, &mut emit);
        },
    )
    .map_err(|e| {
        if e.kind() == ErrorKind::Interrupted {
            "cancelled".into()
        } else {
            format!("spawn {cli_spec}: {e}")
        }
    })?;

    if capture.timed_out {
        return Err(format!("{cli_spec} timed out after {}s", timeout.as_secs()));
    }
    if capture.exit_code != Some(0) {
        let stderr = String::from_utf8_lossy(&capture.stderr);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            acc_text.trim()
        };
        let clipped: String = detail.chars().take(600).collect();
        return Err(format!(
            "{cli_spec} failed (exit {:?}): {clipped}",
            capture.exit_code
        ));
    }
    Ok(Reply {
        text: acc_text.trim().to_owned(),
        calls: Vec::new(),
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_spec_parses_engines_submodels_and_flags() {
        let codex = CliSpec::parse("cli:codex:o3").unwrap();
        assert_eq!(codex.engine, CliEngine::Codex);
        assert_eq!(codex.submodel.as_deref(), Some("o3"));
        let cmd = build_command("cli:codex:o3", "review plan", true, Some(Path::new("/tmp")), None).unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"read-only".to_owned()));
        assert!(args.contains(&"o3".to_owned()));

        let agy = CliSpec::parse("cli:antigravity:pro").unwrap();
        assert_eq!(agy.engine, CliEngine::Agy);
        let agy_cmd = build_command("cli:antigravity:pro", "fix bug", false, None, Some("high")).unwrap();
        let agy_args: Vec<String> = agy_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(agy_args.contains(&"--dangerously-skip-permissions".to_owned()));
        assert!(agy_args.contains(&"--effort".to_owned()));
        assert!(agy_args.contains(&"high".to_owned()));
    }

    #[test]
    fn cli_stream_parser_extracts_codex_and_claude_deltas_and_token_usage() {
        let mut codex_text = String::new();
        let mut codex_usage = Usage::default();
        let mut events = Vec::new();
        {
            let mut sink_fn = |ev: Value| events.push(ev);
            let mut opt_sink: Option<&mut dyn FnMut(Value)> = Some(&mut sink_fn);
            absorb_cli_stream_line(
                CliEngine::Codex,
                r#"{"type":"item.completed","item":{"type":"reasoning","text":"checking invariants"}}"#,
                &mut codex_text,
                &mut codex_usage,
                &mut opt_sink,
            );
            absorb_cli_stream_line(
                CliEngine::Codex,
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"fixed parser"}}"#,
                &mut codex_text,
                &mut codex_usage,
                &mut opt_sink,
            );
            absorb_cli_stream_line(
                CliEngine::Codex,
                r#"{"type":"turn.completed","usage":{"input_tokens":1200,"cached_input_tokens":900,"output_tokens":85}}"#,
                &mut codex_text,
                &mut codex_usage,
                &mut opt_sink,
            );
        }
        assert_eq!(codex_text, "fixed parser");
        assert_eq!(codex_usage.input_tokens, Some(1200));
        assert_eq!(codex_usage.cached_input_tokens, Some(900));
        assert_eq!(codex_usage.output_tokens, Some(85));
        assert_eq!(events.len(), 2);

        let mut claude_text = String::new();
        let mut claude_usage = Usage::default();
        let mut no_sink: Option<&mut dyn FnMut(Value)> = None;
        absorb_cli_stream_line(
            CliEngine::Claude,
            r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"hello "}}}"#,
            &mut claude_text,
            &mut claude_usage,
            &mut no_sink,
        );
        absorb_cli_stream_line(
            CliEngine::Claude,
            r#"{"type":"result","result":"hello world","usage":{"input_tokens":500,"cache_read_input_tokens":400,"output_tokens":40}}"#,
            &mut claude_text,
            &mut claude_usage,
            &mut no_sink,
        );
        assert_eq!(claude_text, "hello ");
        assert_eq!(claude_usage.input_tokens, Some(500));
        assert_eq!(claude_usage.cached_input_tokens, Some(400));
        assert_eq!(claude_usage.output_tokens, Some(40));
    }
}
