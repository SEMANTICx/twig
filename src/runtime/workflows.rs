//! Multi-agent engineering workflows built on `Daemon`, `SubagentPool`, and `Provider`:
//! - `/diff` & `/review` independent fresh-context two-tier review (`blockers` vs `suggestions`)
//! - `/walkthrough` intent-grouped diff explanation
//! - `/dual-plan` blind parallel `plan` × `advisor` investigation and synthesis
//! - `/evolve` guard-first ratchet prompt

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

use super::subagent::{diff_touches_sensitive_paths, Role, SubagentConfig, SubagentPool};
use super::Daemon;
use crate::config::HarnessConfig;
use crate::llm::{cli, Llm, Provider};

/// Prompt sent by `/evolve` for guard-first ratchet + bounded memory distillation.
pub const EVOLVE_PROMPT: &str = "\
Reflect on this session's friction, repeated errors, tool papercuts, or repo-specific conventions you discovered. \
Apply a strict Guard-First Ratchet: \
(1) First check whether the lesson can be enforced mechanically via a unit test, lint, or `post_edit_check` in `.twig/project.json` — prefer executable guards over prose! \
(2) Only distill non-automatable conventions into `.twig/preferences.md` (strictly keep total `.twig/preferences.md` <= 2,400 chars by merging/pruning stale bullets rather than appending endlessly) or a focused skill under `.twig/skills/<name>/SKILL.md` (<500 lines). \
Do NOT modify files yet — present ONE concise unified diff proposal with rationale for human review.";

/// Collect working-tree diff (or staged diff if unstaged is empty) for `/diff`, `/review`, and `/walkthrough`.
pub fn run_git_diff(repo: &str) -> Result<String, String> {
    let out = Command::new("git")
        .args(["-C", repo, "diff"])
        .output()
        .map_err(|e| format!("git diff: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !text.trim().is_empty() {
        return Ok(text);
    }
    let staged = Command::new("git")
        .args(["-C", repo, "diff", "--cached"])
        .output()
        .map_err(|e| format!("git diff --cached: {e}"))?;
    Ok(String::from_utf8_lossy(&staged.stdout).into_owned())
}

/// Invoke `twig review <repo> <diff>` as an independent fresh read-only reviewer for `/review`.
pub fn run_independent_review(repo: &str) -> Result<String, String> {
    let diff = run_git_diff(repo)?;
    if diff.trim().is_empty() {
        return Err("no unstaged or staged git changes to review".into());
    }
    let sensitive = diff_touches_sensitive_paths(&diff);
    let raw = invoke_review_subprocess(repo, &diff)?;
    Ok(format_review_output(&raw, sensitive))
}

fn invoke_review_subprocess(repo: &str, diff: &str) -> Result<String, String> {
    let diff_path = std::env::temp_dir().join(format!("twig-review-{}.diff", std::process::id()));
    std::fs::write(&diff_path, diff).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = Command::new(exe)
        .arg("review")
        .arg(repo)
        .arg(&diff_path)
        .output()
        .map_err(|e| format!("spawn twig review: {e}"))?;
    let _ = std::fs::remove_file(&diff_path);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        return Err(if !err.is_empty() { err } else { stdout });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn format_review_output(raw: &str, sensitive: bool) -> String {
    let Ok(v) = serde_json::from_str::<Value>(raw.trim()) else {
        return raw.to_owned();
    };
    let approved = v.get("approved").and_then(Value::as_bool);
    let summary = v
        .get("review_summary")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/result/text").and_then(Value::as_str))
        .unwrap_or("review completed");
    let blockers = v
        .get("blockers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let suggestions = v
        .get("suggestions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let verdict = match approved {
        Some(true) => "**APPROVED** (0 blockers)",
        Some(false) => "**CHANGES REQUESTED**",
        None => "**REVIEW COMPLETED**",
    };
    let effort_tag = if sensitive {
        " · effort escalated to `xhigh` (sensitive paths touched)"
    } else {
        " · effort `high`"
    };
    let mut md = format!("### Independent Review: {verdict}{effort_tag}\n\n{summary}\n");
    if !blockers.is_empty() {
        md.push_str("\n#### Blockers (must fix)\n");
        for b in &blockers {
            let file = b["file"].as_str().unwrap_or("?");
            let trigger = b["trigger"].as_str().unwrap_or("-");
            let issue = b["issue"].as_str().unwrap_or("");
            md.push_str(&format!("- **`{file}`** (trigger: `{trigger}`): {issue}\n"));
        }
    }
    if !suggestions.is_empty() {
        md.push_str("\n#### Suggestions (non-blocking)\n");
        for s in &suggestions {
            let file = s["file"].as_str().unwrap_or("?");
            let note = s["note"]
                .as_str()
                .or_else(|| s["issue"].as_str())
                .unwrap_or("");
            md.push_str(&format!("- `{file}`: {note}\n"));
        }
    }
    md
}

fn collect_repo_brief_context(repo: &str) -> String {
    let mut ctx = String::new();
    for doc in ["AGENTS.md", "CLAUDE.md", ".twig/preferences.md"] {
        let p = Path::new(repo).join(doc);
        if let Ok(body) = std::fs::read_to_string(p) {
            let clipped: String = body.chars().take(4_000).collect();
            ctx.push_str(&format!("## {doc}\n{clipped}\n\n"));
            if doc == "AGENTS.md" {
                break;
            }
        }
    }
    if let Ok(out) = Command::new("git").args(["-C", repo, "ls-files"]).output() {
        if out.status.success() {
            let files: Vec<&str> = std::str::from_utf8(&out.stdout)
                .unwrap_or("")
                .lines()
                .take(180)
                .collect();
            ctx.push_str(&format!("## Tracked files (sample)\n{}\n", files.join("\n")));
        }
    }
    ctx
}

/// Generate a concise, logical human-readable walkthrough of the current `git diff` (`/walkthrough`).
pub fn run_walkthrough(repo: &str) -> Result<String, String> {
    let diff = run_git_diff(repo)?;
    if diff.trim().is_empty() {
        return Err("working tree clean — no diff to explain".into());
    }
    let clipped_diff: String = diff.chars().take(36_000).collect();
    let provider = Provider::new();
    let model = provider.roles().get("plan").to_owned();
    let reply = provider.complete(
        &model,
        "You are a senior staff engineer producing a concise change walkthrough for code review. Group changes by logical intent rather than raw alphabetical file order, highlight key invariants/trade-offs, and list how to verify.",
        &[json!({
            "role": "user",
            "content": format!("Produce a clear Markdown walkthrough of this git diff:\n\n```diff\n{clipped_diff}\n```")
        })],
        None,
    )?;
    Ok(reply.text)
}

fn run_readonly_planning_agent(
    repo: &str,
    role: &str,
    model: &str,
    prompt: &str,
) -> Result<String, String> {
    if cli::is_cli_model(model) {
        let cfg = SubagentConfig {
            concurrency: 2,
            timeout: Duration::from_secs(180),
            ..SubagentConfig::for_repo(repo).map_err(|e| e.to_string())?
        };
        let pool = SubagentPool::new(cfg).map_err(|e| e.to_string())?;
        let res = pool
            .run_with_options(Role::Advisor, prompt, Some(model), Some("high"))
            .map_err(|e| e.to_string())?;
        let text = res
            .result
            .as_ref()
            .and_then(|r| r["text"].as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or(res.output);
        return Ok(text);
    }

    let (mut cfg, _) = HarnessConfig::resolve()?;
    cfg.max_turns = 8;
    let provider = Provider::new();
    let scratch = std::env::temp_dir().join(format!(
        "twig-dualplan-{}-{}-{}",
        role,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let res = (|| -> Result<String, String> {
        let mut daemon = Daemon::new(&provider, repo, &scratch, "plan", &cfg)?;
        daemon.dispatch("mode.set", &json!({"plan": true}), &mut |_| {})?;
        daemon.dispatch("model.set", &json!({"model": model}), &mut |_| {})?;
        let out = daemon.dispatch("turn", &json!({"input": prompt, "role": role}), &mut |_| {})?;
        Ok(out["text"].as_str().unwrap_or("").to_owned())
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    res
}

fn synthesize_dual_plans(
    provider: &Provider,
    goal: &str,
    plan_model: &str,
    advisor_model: &str,
    plan_a: &str,
    plan_b: &str,
) -> Result<String, String> {
    if cli::is_cli_model(plan_model) {
        return Ok(format!(
            "*Dual-Plan (`plan`: `{plan_model}` × `advisor`: `{advisor_model}`)*\n\n### Plan A (`{plan_model}`)\n{plan_a}\n\n### Plan B (`{advisor_model}`)\n{plan_b}"
        ));
    }
    let synth = provider.complete(
        plan_model,
        "Synthesize two independent blind code-verified engineering plans into one actionable brief using four sections: 1) Shared Consensus, 2) Adopted Advisor Insights, 3) Filtered Over-engineering / Hallucinations, 4) Open Decisions & Step-by-Step Plan.",
        &[json!({
            "role": "user",
            "content": format!(
                "Original Goal: {goal}\n\n--- Plan A (`{plan_model}`) ---\n{plan_a}\n\n--- Plan B (`{advisor_model}`) ---\n{plan_b}"
            )
        })],
        None,
    )?;
    Ok(format!(
        "*Dual-Plan (`plan`: `{plan_model}` × `advisor`: `{advisor_model}`)*\n\n{}",
        synth.text
    ))
}

/// Run blind independent tool-driven dual-model planning (`plan` + `advisor`) on the raw user goal,
/// then synthesize a concrete comparison & decision brief (`/dual-plan`).
pub fn run_dual_plan(repo: &str, goal: &str) -> Result<String, String> {
    let context = collect_repo_brief_context(repo);
    let provider = Provider::new();
    let plan_model = provider.roles().get("plan").to_owned();
    let advisor_model = provider.roles().get("advisor").to_owned();

    let prompt_a = format!(
        "[Primary Planner — Read-Only Investigation]\n\
         Use `outline`, `grep`, `glob`, and `read` to verify real file paths, structs, and signatures before writing your plan.\n\
         Repository overview:\n{context}\n\nGoal:\n{goal}\n\n\
         Produce a grounded implementation plan: exact files/functions to modify, invariants, edge cases, and verification steps."
    );
    let prompt_b = format!(
        "[Independent Oracle/Advisor — Blind Read-Only Investigation]\n\
         Independently inspect relevant files with `outline`, `grep`, `glob`, and `read`. Call out architectural traps, security/concurrency hazards, and simpler alternatives.\n\
         Repository overview:\n{context}\n\nGoal:\n{goal}\n\n\
         Produce a grounded independent design and risk analysis."
    );

    let (res_a, res_b) = std::thread::scope(|s| {
        let pm = plan_model.as_str();
        let am = advisor_model.as_str();
        let pa = prompt_a.as_str();
        let pb = prompt_b.as_str();
        let ha = s.spawn(move || run_readonly_planning_agent(repo, "plan", pm, pa));
        let hb = s.spawn(move || run_readonly_planning_agent(repo, "advisor", am, pb));
        (
            ha.join().unwrap_or_else(|_| Err("planner panicked".into())),
            hb.join().unwrap_or_else(|_| Err("advisor panicked".into())),
        )
    });
    let plan_a = res_a?;
    let plan_b = res_b?;
    synthesize_dual_plans(&provider, goal, &plan_model, &advisor_model, &plan_a, &plan_b)
}
