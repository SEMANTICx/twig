//! CLI command handlers for headless execution (`twig exec`), session lifecycle
//! (`twig session`), environment diagnostics (`twig doctor`), and clean-room
//! differential replay (`twig verify`).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::config::{paths, HarnessConfig};
use crate::llm::{summarize_usage, ConfigOp, Llm, Provider};
use crate::runtime::guidance::SkillStore;
use crate::runtime::session::{
    apply_session_patch, archive_session, format_session_transcript, gc_sessions_and_subagents,
    load_session_patch, scan_sessions, search_sessions, session_worktree,
};
use crate::runtime::Daemon;
use crate::verify::{derive_edges, gate, record_oracle, Candidate};

struct ExecFlags {
    repo: String,
    role: String,
    model: Option<String>,
    effort: Option<String>,
    resume_sid: Option<String>,
    check_cmd: Option<String>,
    read_only: bool,
    json_out: bool,
    events_out: bool,
    prompt: String,
}

fn parse_exec_flags(args: &[String]) -> Result<ExecFlags, String> {
    let mut repo = String::from(".");
    let mut role = String::from("code");
    let mut model: Option<String> = None;
    let mut effort: Option<String> = None;
    let mut resume_sid: Option<String> = None;
    let mut check_cmd: Option<String> = None;
    let mut read_only = false;
    let mut json_out = false;
    let mut events_out = false;
    let mut prompt_parts: Vec<String> = Vec::new();

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" | "-C" => {
                i += 1;
                repo = args.get(i).cloned().ok_or("--repo requires a path")?;
            }
            "--role" => {
                i += 1;
                role = args.get(i).cloned().ok_or("--role requires a role name")?;
            }
            "--model" | "-m" => {
                i += 1;
                model = Some(args.get(i).cloned().ok_or("--model requires a model id")?);
            }
            "--effort" => {
                i += 1;
                effort = Some(args.get(i).cloned().ok_or("--effort requires a level")?);
            }
            "--resume" | "-r" => {
                i += 1;
                resume_sid = Some(args.get(i).cloned().ok_or("--resume requires a sid")?);
            }
            "--check" => {
                i += 1;
                check_cmd = Some(args.get(i).cloned().ok_or("--check requires a command")?);
            }
            "--read-only" | "--plan" => read_only = true,
            "--json" => json_out = true,
            "--events" | "--stream-json" => events_out = true,
            "--" => {
                prompt_parts.extend(args[i + 1..].iter().cloned());
                break;
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown twig exec flag {other:?}"));
            }
            word => prompt_parts.push(word.to_owned()),
        }
        i += 1;
    }

    let mut prompt = prompt_parts.join(" ");
    if prompt.trim().is_empty() || prompt.trim() == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("read stdin: {e}"))?;
        prompt = buf;
    }
    if prompt.trim().is_empty() {
        return Err(
            "usage: twig exec [--repo DIR] [--role ROLE] [--model M] [--effort E] [--resume SID] [--read-only] [--check CMD] [--json] <PROMPT>"
                .into(),
        );
    }
    Ok(ExecFlags {
        repo,
        role,
        model,
        effort,
        resume_sid,
        check_cmd,
        read_only,
        json_out,
        events_out,
        prompt,
    })
}

/// Non-interactive single-shot execution (`twig exec [flags] "<prompt>"`).
pub fn run_headless_exec(args: &[String]) -> Result<i32, String> {
    let mut flags = parse_exec_flags(args)?;
    let sid = flags.resume_sid.unwrap_or_else(paths::session_id);
    if !paths::valid_session(&sid) {
        return Err(format!("invalid session id {sid:?}"));
    }
    if flags.repo == "." {
        if let Some(saved_wt) = session_worktree(&sid) {
            if Path::new(&saved_wt).is_dir() {
                flags.repo = saved_wt;
            }
        }
    }

    let (cfg, _) = HarnessConfig::resolve()?;
    let provider = Provider::new();
    let runs_dir = paths::state_dir().join("sessions");
    let mut daemon = Daemon::new(&provider, &flags.repo, &runs_dir, &sid, &cfg)?;
    if let Some(m) = flags.model {
        daemon.dispatch("model.set", &json!({"model": m}), &mut |_| {})?;
    }
    if let Some(e) = flags.effort {
        daemon.dispatch("effort.set", &json!({"effort": e}), &mut |_| {})?;
    }
    if flags.read_only || matches!(flags.role.as_str(), "plan" | "advisor" | "scout" | "reviewer") {
        daemon.dispatch("mode.set", &json!({"plan": true}), &mut |_| {})?;
    }
    let mut turn_params = json!({"input": flags.prompt, "role": flags.role});
    if let Some(cmd) = flags.check_cmd {
        turn_params["check"] = json!({"command": cmd});
    }

    let out = if flags.events_out {
        daemon.dispatch("turn", &turn_params, &mut |ev| {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{ev}");
            let _ = stdout.flush();
        })?
    } else {
        daemon.dispatch("turn", &turn_params, &mut |_| {})?
    };
    let done = out["done"] == true;
    let reason = out["reason"].as_str().unwrap_or("unknown");
    let text = out["text"].as_str().unwrap_or("");
    let diff_stat = Command::new("git")
        .args(["-C", &flags.repo, "diff", "HEAD", "--shortstat"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty());

    let rows: Vec<Value> = std::fs::read_to_string(daemon.record_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let usage = summarize_usage(&rows);

    if flags.events_out {
        println!(
            "{}",
            json!({
                "ev": "exec.completed",
                "sid": sid,
                "done": done,
                "reason": reason,
                "text": text,
                "verified": out.get("verified").cloned().unwrap_or(Value::Null),
                "diff_stat": diff_stat,
                "usage": usage,
            })
        );
    } else if flags.json_out {
        println!(
            "{}",
            json!({
                "sid": sid,
                "done": done,
                "reason": reason,
                "text": text,
                "verified": out.get("verified").cloned().unwrap_or(Value::Null),
                "diff_stat": diff_stat,
                "usage": usage,
            })
        );
    } else if !text.is_empty() {
        println!("{text}");
    } else if reason != "final" {
        eprintln!("twig exec [{sid}]: ended with reason={reason}");
    }

    Ok(if done && reason == "final" {
        0
    } else if reason == "awaiting_approval" {
        2
    } else {
        1
    })
}

/// CLI management for first-class sessions (`twig session <list|search|show|spawn|send|diff|apply|archive|gc>`).
pub fn run_session_cli(args: &[String]) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let has_flag = |f: &str| args.iter().any(|a| a == f);
    let flag_val = |f: &str| -> Option<String> {
        args.iter()
            .position(|a| a == f)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    match sub {
        "list" | "ls" => {
            let all = has_flag("--all") || has_flag("-a");
            let repo = flag_val("--repo").unwrap_or_else(|| ".".into());
            let filter = (!all).then(|| Path::new(repo.as_str()));
            let items = scan_sessions(filter, 40);
            if has_flag("--json") {
                println!("{}", serde_json::to_string_pretty(&items).unwrap_or_default());
            } else if items.is_empty() {
                println!("no sessions found (pass `--all` to list across all repositories)");
            } else {
                for s in items {
                    let role = s.role.as_deref().unwrap_or("code");
                    let wt = s.worktree.as_deref().unwrap_or("-");
                    let badge = match s.status.as_str() {
                        "approval" => "[▲ wait]  ",
                        "running" => "[● run]   ",
                        "dirty" => "[● diff]  ",
                        _ => "[✓ ok]    ",
                    };
                    println!(
                        "{badge}{:<24} {:<8} {:>3}t {:>7}  {}  ({})",
                        s.sid, role, s.turns, s.age, s.label, wt
                    );
                }
            }
            Ok(0)
        }
        "search" => {
            let query = args
                .get(1)
                .filter(|s| !s.starts_with('-'))
                .ok_or("usage: twig session search <QUERY> [--all] [--json]")?;
            let all = has_flag("--all") || has_flag("-a");
            let repo = flag_val("--repo").unwrap_or_else(|| ".".into());
            let filter = (!all).then(|| Path::new(repo.as_str()));
            let hits = search_sessions(query, filter, 25);
            if has_flag("--json") {
                println!("{}", serde_json::to_string_pretty(&hits).unwrap_or_default());
            } else if hits.is_empty() {
                println!("no session matches for {query:?}");
            } else {
                for h in hits {
                    println!(
                        "session {} ({})",
                        h["sid"].as_str().unwrap_or("?"),
                        h["worktree"].as_str().unwrap_or("-")
                    );
                    for m in h["matches"].as_array().into_iter().flatten() {
                        if let Some(line) = m.as_str() {
                            println!("  {line}");
                        }
                    }
                }
            }
            Ok(0)
        }
        "show" | "inspect" => {
            let sid = args.get(1).ok_or("usage: twig session show <SID>")?;
            let text = format_session_transcript(sid)?;
            print!("{text}");
            Ok(0)
        }
        "spawn" => {
            let mut exec_args = vec!["--json".to_owned()];
            exec_args.extend(args[1..].iter().cloned());
            run_headless_exec(&exec_args)
        }
        "send" => {
            let sid = args.get(1).ok_or("usage: twig session send <SID> <PROMPT...>")?;
            if args.len() < 3 {
                return Err("usage: twig session send <SID> <PROMPT...>".into());
            }
            let mut exec_args = vec![
                "--resume".to_owned(),
                sid.clone(),
                "--json".to_owned(),
            ];
            exec_args.extend(args[2..].iter().cloned());
            run_headless_exec(&exec_args)
        }
        "diff" => {
            let sid = args.get(1).ok_or("usage: twig session diff <SID>")?;
            let patch = load_session_patch(sid, None)?;
            print!("{patch}");
            Ok(0)
        }
        "apply" => {
            let sid = args
                .get(1)
                .filter(|s| !s.starts_with('-'))
                .ok_or("usage: twig session apply <SID> [--repo DIR]")?;
            let repo = flag_val("--repo").unwrap_or_else(|| ".".into());
            let msg = apply_session_patch(Path::new(&repo), sid)?;
            println!("{msg}");
            Ok(0)
        }
        "gc" => {
            let (wt, empty) = gc_sessions_and_subagents()?;
            println!(
                "gc: removed {wt} clean subagent worktree(s) and {empty} empty session file(s)"
            );
            Ok(0)
        }
        "archive" => {
            let sid = args.get(1).ok_or("usage: twig session archive <SID>")?;
            let dst = archive_session(sid)?;
            println!("{dst}");
            Ok(0)
        }
        other => Err(format!(
            "unknown session subcommand {other:?}; expected list, search, show, spawn, send, diff, apply, archive, or gc"
        )),
    }
}

fn audit_instruction_docs() -> (bool, Vec<String>) {
    let mut doc_notes = Vec::new();
    let mut doc_warn = false;
    let has_agents = Path::new("AGENTS.md").is_file();
    let has_claude = Path::new("CLAUDE.md").is_file();
    if has_agents && has_claude {
        doc_warn = true;
        doc_notes.push(
            "WARN duplicate root AGENTS.md + CLAUDE.md (twig prioritizes AGENTS.md; consolidate to avoid drift)"
                .to_owned(),
        );
    }
    for (doc, max_lines, max_chars) in [
        ("AGENTS.md", 200usize, 8_000usize),
        ("CLAUDE.md", 200usize, 8_000usize),
        (".twig/preferences.md", 80usize, 2_400usize),
    ] {
        if let Ok(body) = std::fs::read_to_string(doc) {
            let lines = body.lines().count();
            let chars = body.chars().count();
            if lines > max_lines || chars > max_chars {
                doc_warn = true;
                doc_notes.push(format!(
                    "WARN {doc}: {lines} lines / {chars} chars exceeds {max_lines}L/{max_chars}c budget — move style rules to deterministic linters"
                ));
            } else {
                doc_notes.push(format!("ok   {doc}: {lines} lines ({chars} chars)"));
            }
        }
    }
    (doc_warn, doc_notes)
}

/// Environment & configuration diagnostic (`twig doctor`); returns `true` when healthy.
pub fn run_doctor() -> bool {
    let mut failed = false;
    println!(
        "config_path: {}\nstate_dir:   {}",
        paths::config_path().display(),
        paths::state_dir().display()
    );
    match HarnessConfig::resolve() {
        Ok((_, src)) => println!("config:      ok ({src})"),
        Err(e) => {
            println!("config:      FAIL ({e})");
            failed = true;
        }
    }
    let provider = Provider::new();
    let env_model = std::env::var_os("TWIG_MODEL").is_some();
    let env_roles = std::env::var_os("TWIG_ROLES").is_some();
    let saved_model = Provider::saved_model();
    let model_src = if env_model {
        "env TWIG_MODEL"
    } else if env_roles {
        "env TWIG_ROLES"
    } else if saved_model.is_some() {
        "saved model.txt"
    } else {
        "default"
    };
    println!(
        "model:       {} ({model_src}) · effort: {}",
        provider.roles().get("code"),
        provider.effort().as_deref().unwrap_or("off")
    );
    if let Ok(roles_val) = Provider::apply_config(ConfigOp::RolesReport) {
        if let Some(roles_text) = roles_val["report"].as_str() {
            for line in roles_text.lines().skip(1) {
                println!("{line}");
            }
        }
    }
    if (env_model || env_roles) && saved_model.is_some() {
        println!(
            "  warn: saved model ({}) is overridden by shell env (TWIG_MODEL/TWIG_ROLES)",
            saved_model.as_deref().unwrap_or("")
        );
    }
    let tools_to_check: &[&str] = if cfg!(target_os = "linux") {
        &["git", "gh", "bwrap"]
    } else {
        &["git", "gh"]
    };
    for exe in tools_to_check {
        let ok = Command::new(exe)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        println!("{exe:<12} {}", if ok { "available" } else { "missing" });
        if *exe == "git" && !ok {
            failed = true;
        }
    }
    #[cfg(target_os = "linux")]
    {
        println!(
            "bwrap_ns:    {}",
            if crate::exec::isolation::bwrap_userns_usable() {
                "ok (user namespaces enabled)"
            } else {
                "unavailable (sandbox/subagent bwrap restricted in this environment)"
            }
        );
    }
    #[cfg(not(target_os = "linux"))]
    println!("sandbox:     worktree-only (bwrap OS isolation is Linux-only)");
    match provider.list_models() {
        Ok(models) => println!(
            "magpie:      ok ({} models at {})",
            models.len(),
            provider.base()
        ),
        Err(e) => println!("magpie:      unreachable ({e})"),
    }
    let repo_opt = Path::new(".twig/skills").is_dir().then(|| PathBuf::from("."));
    let store = SkillStore::default_for(repo_opt.as_deref());
    let (skills_ok, skills_report) = store.doctor_report();
    println!("skills:      {}", if skills_ok { "ok" } else { "WARN" });
    for line in skills_report.lines() {
        println!("  {line}");
    }
    let (doc_warn, doc_notes) = audit_instruction_docs();
    println!(
        "prompt_audit:{}",
        if doc_warn {
            " WARN"
        } else if doc_notes.is_empty() {
            " ok (no repo instruction files)"
        } else {
            " ok"
        }
    );
    for note in doc_notes {
        println!("  {note}");
    }
    !failed
}

fn run_verify_record(args: &[String]) -> i32 {
    let mut raw_cli = false;
    let mut inputs_file: Option<String> = None;
    let mut inline_cases: Vec<Value> = Vec::new();
    let mut pos: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--raw" => raw_cli = true,
            "--inputs" => {
                i += 1;
                inputs_file = args.get(i).cloned();
            }
            "--stdin" => {
                i += 1;
                if let Some(s) = args.get(i) {
                    raw_cli = true;
                    inline_cases.push(json!({"_cli": {"args": [], "stdin": s}}));
                }
            }
            "--arg-case" => {
                i += 1;
                if let Some(s) = args.get(i) {
                    raw_cli = true;
                    let split: Vec<String> = s.split_whitespace().map(str::to_owned).collect();
                    inline_cases.push(json!({"_cli": {"args": split, "stdin": ""}}));
                }
            }
            "--" => {
                pos.extend(args[i + 1..].iter().cloned());
                break;
            }
            other => pos.push(other.to_owned()),
        }
        i += 1;
    }
    if pos.len() < 2 {
        eprintln!(
            "usage: twig verify record <journal.jsonl> <oracle_program> [args...] [--raw] [--inputs seeds.jsonl] [--stdin TEXT] [--arg-case \"FLAGS\"]"
        );
        return 2;
    }
    if let Some(path) = inputs_file {
        let body = match std::fs::read_to_string(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("read {path}: {e}");
                return 1;
            }
        };
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let v = serde_json::from_str::<Value>(line)
                .unwrap_or_else(|_| Value::String(format!("{line}\n")));
            inline_cases.push(v);
        }
    }
    if inline_cases.is_empty() {
        inline_cases = if raw_cli {
            vec![json!({"_cli": {"args": [], "stdin": ""}})]
        } else {
            derive_edges(&[json!({"ping": 1})])
        };
    }
    let journal = Path::new(&pos[0]);
    let oracle = Candidate::new(pos[1].clone(), pos[2..].to_vec()).with_raw_cli(raw_cli);
    match record_oracle(journal, &oracle, &inline_cases) {
        Ok(n) => {
            println!("recorded {n} oracle case(s) into {}", journal.display());
            0
        }
        Err(e) => {
            eprintln!("verify record failed: {e}");
            1
        }
    }
}

/// Clean-room differential verification CLI (`twig verify [record] ...`); returns exit code.
pub fn run_verify_cli(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("record") {
        return run_verify_record(&args[1..]);
    }

    let mut min_cases = 1usize;
    let mut min_branch = 0.0f64;
    let mut json_out = false;
    let mut raw_cli = false;
    let mut ignore_fields: Vec<String> = Vec::new();
    let mut pos: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--min-cases" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<usize>().ok()) {
                    Some(n) if n >= 1 => min_cases = n,
                    _ => {
                        eprintln!("twig verify: --min-cases must be an integer >= 1");
                        return 2;
                    }
                }
            }
            "--min-branch" => {
                i += 1;
                min_branch = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            }
            "--ignore" => {
                i += 1;
                if let Some(s) = args.get(i) {
                    ignore_fields.extend(
                        s.split(',')
                            .map(str::trim)
                            .filter(|x| !x.is_empty())
                            .map(str::to_owned),
                    );
                }
            }
            "--raw" => raw_cli = true,
            "--json" => json_out = true,
            "--" => {
                pos.extend(args[i + 1..].iter().cloned());
                break;
            }
            other => pos.push(other.to_owned()),
        }
        i += 1;
    }
    if pos.len() < 2 {
        eprintln!(
            "usage: twig verify [record] <journal.jsonl> <program> [args...] [--raw] [--ignore k1,k2] [--min-cases N] [--min-branch 0..1] [--json]"
        );
        return 2;
    }
    let journal = Path::new(&pos[0]);
    let candidate = Candidate::new(pos[1].clone(), pos[2..].to_vec())
        .with_raw_cli(raw_cli)
        .with_ignore_fields(ignore_fields);
    let verdict = gate(journal, &candidate, min_cases, None, min_branch, None);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&verdict).unwrap_or_default());
    } else if verdict.ok {
        println!(
            "verify ok: {}/{} case(s) matched oracle ({})",
            verdict.covered,
            verdict.total,
            journal.display()
        );
    } else {
        eprintln!(
            "verify FAILED ({}/{} matched): {}",
            verdict.covered,
            verdict.total,
            verdict.why.as_deref().unwrap_or("mismatch")
        );
        for fail in verdict.fails.iter().take(10) {
            eprintln!(
                "  input={} want={} got={}",
                fail.input, fail.want, fail.got
            );
        }
    }
    if verdict.ok { 0 } else { 1 }
}
