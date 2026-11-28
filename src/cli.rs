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

fn current_release_target() -> Result<(&'static str, &'static str), String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok(("x86_64-unknown-linux-musl", "tar.gz")),
        ("linux", "aarch64") => Ok(("aarch64-unknown-linux-gnu", "tar.gz")),
        ("macos", "aarch64") => Ok(("aarch64-apple-darwin", "tar.gz")),
        ("macos", "x86_64") => Ok(("x86_64-apple-darwin", "tar.gz")),
        ("windows", "x86_64") => Ok(("x86_64-pc-windows-msvc", "zip")),
        (os, arch) => Err(format!("precompiled self-update is not available for {os}/{arch}; build via `cargo install --git https://github.com/SEMANTICx/twig`")),
    }
}

fn fetch_latest_dist_tag(dist_repo: &str) -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{dist_repo}/releases/latest");
    let resp = ureq::get(&url)
        .set("User-Agent", concat!("twig/", env!("CARGO_PKG_VERSION")))
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("query {url}: {e}"))?;
    let raw = resp
        .into_string()
        .map_err(|e| format!("read release JSON: {e}"))?;
    let val: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("parse release JSON: {e}"))?;
    val["tag_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("no latest release tag found on {dist_repo}"))
}

fn verify_file_sha256(archive: &Path, sha_file: &Path) -> Result<(), String> {
    let expected = std::fs::read_to_string(sha_file)
        .map_err(|e| format!("read {}: {e}", sha_file.display()))?
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("invalid SHA-256 digest in {}", sha_file.display()));
    }
    let computed = if let Ok(o) = Command::new("sha256sum").arg(archive).output() {
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).split_whitespace().next().unwrap_or("").to_ascii_lowercase())
    } else {
        None
    }
    .or_else(|| {
        Command::new("shasum")
            .args(["-a", "256"])
            .arg(archive)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().next().unwrap_or("").to_ascii_lowercase())
    })
    .or_else(|| {
        Command::new("openssl")
            .args(["dgst", "-sha256"])
            .arg(archive)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().last().unwrap_or("").to_ascii_lowercase())
    })
    .ok_or("no SHA-256 verifier found (install sha256sum, shasum, or openssl)")?;

    if computed != expected {
        return Err(format!(
            "SHA-256 checksum mismatch (expected {expected}, got {computed})"
        ));
    }
    Ok(())
}

fn download_to_file(url: &str, dest: &Path) -> Result<(), String> {
    let resp = ureq::get(url)
        .set("User-Agent", concat!("twig/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|e| format!("download {url}: {e}"))?;
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    std::io::copy(&mut reader, &mut file).map_err(|e| format!("write {}: {e}", dest.display()))?;
    file.flush().map_err(|e| e.to_string())
}

/// Self-update `twig` binary in-place from `SEMANTICx/twig-dist` (`twig update [--check] [--force] [--version vX.Y.Z]`).
pub fn run_self_update(args: &[String]) -> Result<i32, String> {
    let check_only = args.iter().any(|a| a == "--check" || a == "-c");
    let force = args.iter().any(|a| a == "--force" || a == "-f");
    let pinned = args
        .iter()
        .position(|a| a == "--version" || a == "-v")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| std::env::var("TWIG_VERSION").ok().filter(|s| !s.is_empty()));
    let dist_repo = std::env::var("TWIG_REPO").unwrap_or_else(|_| "SEMANTICx/twig-dist".into());
    let cur_tag = concat!("v", env!("CARGO_PKG_VERSION"));
    let target_tag = match pinned {
        Some(t) if t.starts_with('v') => t,
        Some(t) => format!("v{t}"),
        None => fetch_latest_dist_tag(&dist_repo)?,
    };

    if check_only {
        if target_tag == cur_tag {
            println!("twig is up to date ({cur_tag})");
        } else {
            println!("update available: {cur_tag} -> {target_tag} (run `twig update` to install)");
        }
        return Ok(0);
    }
    if target_tag == cur_tag && !force {
        println!("twig is already at {cur_tag} (pass `--force` to reinstall)");
        return Ok(0);
    }

    let (target, ext) = current_release_target()?;
    let pkg = format!("twig-{target_tag}-{target}");
    let base_url = format!("https://github.com/{dist_repo}/releases/download/{target_tag}");
    let tmp_dir = std::env::temp_dir().join(format!("twig-selfupdate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("tempdir: {e}"))?;

    let res = (|| -> Result<(), String> {
        let archive_path = tmp_dir.join(format!("{pkg}.{ext}"));
        let sha_path = tmp_dir.join(format!("{pkg}.{ext}.sha256"));
        println!("Downloading {pkg}.{ext} from {dist_repo}...");
        download_to_file(&format!("{base_url}/{pkg}.{ext}"), &archive_path)?;
        download_to_file(&format!("{base_url}/{pkg}.{ext}.sha256"), &sha_path)?;
        verify_file_sha256(&archive_path, &sha_path)?;

        let bin_name = if cfg!(windows) { "twig.exe" } else { "twig" };
        if ext == "zip" {
            let status = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!(
                        "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                        archive_path.display(),
                        tmp_dir.display()
                    ),
                ])
                .status()
                .map_err(|e| format!("Expand-Archive: {e}"))?;
            if !status.success() {
                return Err("Expand-Archive failed".into());
            }
        } else {
            let status = Command::new("tar")
                .arg("-C")
                .arg(&tmp_dir)
                .arg("-xzf")
                .arg(&archive_path)
                .status()
                .map_err(|e| format!("tar extract: {e}"))?;
            if !status.success() {
                return Err("tar extract failed".into());
            }
        }

        let unpacked = tmp_dir.join(&pkg).join(bin_name);
        if !unpacked.is_file() {
            return Err(format!("unpacked binary missing at {}", unpacked.display()));
        }
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let exe_dir = exe.parent().ok_or("cannot resolve binary parent directory")?;
        let staged = exe_dir.join(format!(".{bin_name}.update-{}", std::process::id()));
        std::fs::copy(&unpacked, &staged)
            .map_err(|e| format!("stage update in {}: {e}", exe_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
        }
        #[cfg(windows)]
        {
            let old = exe.with_extension("old.exe");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&exe, &old);
        }
        std::fs::rename(&staged, &exe).map_err(|e| {
            let _ = std::fs::remove_file(&staged);
            format!("activate {}: {e}", exe.display())
        })?;
        println!("Updated twig {cur_tag} -> {target_tag} ({})", exe.display());
        Ok(())
    })();

    let _ = std::fs::remove_dir_all(&tmp_dir);
    res.map(|()| 0)
}
