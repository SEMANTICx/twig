//! Session recovery, repository instruction discovery, non-intrusive Git worktree
//! turn snapshots, and cross-session metadata & search.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::paths;
use crate::exec::Check;

pub(super) const UNKNOWN_OUTCOME_MSG: &str = "PREVIOUS EXECUTION OUTCOME UNKNOWN. Command was NOT replayed after restart. Inspect the workspace manually before taking further action.";
pub(super) const RESOLVED_UNKNOWN_MSG: &str = "Previous execution outcome is unknown. The command was NOT replayed. Inspect the workspace manually before issuing any new side effect.";
const MAX_REPO_INSTRUCTIONS_BYTES: usize = 32_768;

/// Infer model family (`claude`, `gpt`, `gemini`, or custom prefix) for optional `.twig/rules/<family>.md`.
pub(super) fn detect_model_family(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase();
    if m.contains("claude") || m.contains("opus") || m.contains("sonnet") || m.contains("haiku") {
        Some("claude")
    } else if m.contains("gpt") || m.contains("codex") || m.contains("o1") || m.contains("o3") || m.contains("o4") {
        Some("gpt")
    } else if m.contains("gemini") {
        Some("gemini")
    } else if m.contains("deepseek") {
        Some("deepseek")
    } else {
        None
    }
}

/// Load root repository instructions without duplicating `AGENTS.md` + `CLAUDE.md`.
/// Precedence: (`AGENTS.md` else `CLAUDE.md`) + `.twig/preferences.md` + optional `.twig/rules/<family>.md`.
/// Confines every file via `canonicalize()` so repo symlinks cannot exfiltrate host secrets into system prompts.
pub(super) fn append_repo_instructions(worktree: &Path, active_model: &str, system: &mut String) {
    let Ok(canon_root) = worktree.canonicalize() else {
        return;
    };
    let confined_doc = |rel: &str| -> Option<PathBuf> {
        let p = canon_root.join(rel);
        let canon = p.canonicalize().ok()?;
        (canon.starts_with(&canon_root) && canon.is_file()).then_some(canon)
    };
    let mut budget = MAX_REPO_INSTRUCTIONS_BYTES;
    let primary_doc = if confined_doc("AGENTS.md").is_some() {
        Some("AGENTS.md")
    } else if confined_doc("CLAUDE.md").is_some() {
        Some("CLAUDE.md")
    } else {
        None
    };

    let mut candidates: Vec<String> = Vec::new();
    if let Some(doc) = primary_doc {
        candidates.push(doc.to_owned());
    }
    candidates.push(".twig/preferences.md".to_owned());
    if let Some(family) = detect_model_family(active_model) {
        candidates.push(format!(".twig/rules/{family}.md"));
    }

    for rel in candidates {
        if budget == 0 {
            break;
        }
        let Some(canon) = confined_doc(&rel) else {
            continue;
        };
        let Ok(body) = std::fs::read_to_string(&canon) else {
            continue;
        };
        let trimmed = body.trim();
        if trimmed.is_empty() {
            continue;
        }
        let clipped: String = trimmed.chars().take(budget).collect();
        budget = budget.saturating_sub(clipped.len());
        system.push_str(&format!("\n\n## {rel}\n{clipped}"));
    }
}

pub(super) struct RecoveredState {
    pub messages: Vec<Value>,
    pub pending: Vec<Value>,
    pub constraints: Vec<String>,
    pub check: Option<Check>,
    pub snapshots: Vec<String>,
    pub operator_mode: bool,
    pub study_mode: bool,
    pub had_checkpoint: bool,
}

/// Single-pass O(N) session recovery from the append-only hash-chained record.
pub(super) fn recover_session(rows: &[Value], default_check: Option<Check>) -> RecoveredState {
    let check = rows
        .iter()
        .rev()
        .find(|r| r["kind"] == "acceptance.configured")
        .and_then(|r| serde_json::from_value(r["check"].clone()).ok())
        .or(default_check);

    let operator_mode = rows
        .iter()
        .rev()
        .find(|r| r["kind"] == "operator.changed")
        .and_then(|r| r["enabled"].as_bool())
        .unwrap_or(false);

    let study_mode = rows
        .iter()
        .rev()
        .find(|r| r["kind"] == "study.changed")
        .and_then(|r| r["enabled"].as_bool())
        .unwrap_or(false);

    let mut snapshots = Vec::new();
    let mut constraints = Vec::new();
    let mut has_result = HashSet::new();
    let mut resolved_recovered = HashSet::new();
    let mut resolved_any = HashSet::new();
    let mut requested: HashMap<String, (String, Value)> = HashMap::new();
    let mut started: HashMap<String, (String, Value)> = HashMap::new();

    for row in rows {
        let kind = row["kind"].as_str().unwrap_or("");
        match kind {
            "turn.snapshot" => {
                if let Some(tree) = row["tree"].as_str() {
                    snapshots.push(tree.to_owned());
                }
            }
            "session.rewound" => {
                let n = row["turns"].as_u64().unwrap_or(1) as usize;
                let keep = snapshots.len().saturating_sub(n);
                snapshots.truncate(keep);
            }
            "steering.queued" | "steering.applied" => {
                if let Some(text) = row["text"].as_str() {
                    if !text.trim().is_empty() && !constraints.iter().any(|c| c == text) {
                        constraints.push(text.to_owned());
                    }
                }
            }
            "tool.result" => {
                if let Some(id) = row["id"].as_str() {
                    has_result.insert(id.to_owned());
                }
            }
            "approval.resolved" => {
                if let Some(id) = row["id"].as_str() {
                    resolved_any.insert(id.to_owned());
                    if row["recovered"] == true {
                        resolved_recovered.insert(id.to_owned());
                    }
                }
            }
            "approval.requested" => {
                if let Some(id) = row["id"].as_str() {
                    let name = row["name"]
                        .as_str()
                        .or(row["tool"].as_str())
                        .unwrap_or("bash")
                        .to_owned();
                    requested
                        .entry(id.to_owned())
                        .or_insert((name, row["args"].clone()));
                }
            }
            "tool.started" => {
                if let Some(id) = row["id"].as_str() {
                    let name = row["tool"]
                        .as_str()
                        .or(row["name"].as_str())
                        .unwrap_or("bash")
                        .to_owned();
                    started
                        .entry(id.to_owned())
                        .or_insert((name, row["args"].clone()));
                }
            }
            _ => {}
        }
    }

    let mut pending = Vec::new();
    let mut unknown_ids = HashSet::new();
    let mut seen_pending = HashSet::new();
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        let kind = row["kind"].as_str().unwrap_or("");
        if id.is_empty()
            || has_result.contains(id)
            || resolved_recovered.contains(id)
            || !matches!(kind, "approval.requested" | "tool.started")
        {
            continue;
        }
        let started_without_result = started.contains_key(id);
        let unresolved_approval = requested.contains_key(id) && !resolved_any.contains(id);
        if !started_without_result && !unresolved_approval {
            continue;
        }
        unknown_ids.insert(id.to_owned());
        if requested.contains_key(id) && seen_pending.insert(id.to_owned()) {
            let (name, args) = requested
                .get(id)
                .or_else(|| started.get(id))
                .cloned()
                .unwrap_or_else(|| ("bash".into(), Value::Null));
            pending.push(json!({
                "ev": "approval.requested",
                "id": id,
                "name": name,
                "args": args,
                "recovered": true
            }));
        }
    }

    let checkpoint = rows
        .iter()
        .rposition(|r| r["kind"] == "messages.checkpoint");
    let raw_messages: Vec<Value> = {
        let mut base = checkpoint
            .and_then(|at| rows[at]["messages"].as_array().cloned())
            .unwrap_or_default();
        for row in rows.iter().skip(checkpoint.map_or(0, |n| n + 1)) {
            if row["kind"] == "message" {
                base.push(row["message"].clone());
            }
        }
        base
    };

    let mut declared_calls = HashSet::new();
    for message in &raw_messages {
        if message["role"] == "assistant" {
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                if let Some(id) = call["id"].as_str() {
                    declared_calls.insert(id.to_owned());
                }
            }
        }
    }

    let mut recorded_outputs: HashMap<String, String> = HashMap::new();
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        if id.is_empty() || !declared_calls.contains(id) {
            continue;
        }
        let output = match row["kind"].as_str() {
            Some("tool.result") => Some(
                row["output"]
                    .as_str()
                    .unwrap_or("tool completed")
                    .to_owned(),
            ),
            Some("approval.resolved") if row["recovered"] == true => {
                Some(RESOLVED_UNKNOWN_MSG.to_owned())
            }
            Some("approval.resolved") if row["allow"] == false => Some("denied by user".to_owned()),
            _ => None,
        };
        if let Some(out) = output {
            recorded_outputs.entry(id.to_owned()).or_insert(out);
        }
    }

    let mut messages = Vec::with_capacity(raw_messages.len() + 4);
    let mut emitted_replies = HashSet::new();
    let mut idx = 0usize;
    while idx < raw_messages.len() {
        let msg = &raw_messages[idx];
        if msg["role"] == "tool" {
            idx += 1;
            continue;
        }
        messages.push(msg.clone());
        if msg["role"] == "assistant" {
            if let Some(calls) = msg["tool_calls"].as_array() {
                let expected: Vec<String> = calls
                    .iter()
                    .filter_map(|c| c["id"].as_str().map(str::to_owned))
                    .collect();
                let expected_set: HashSet<&str> = expected.iter().map(String::as_str).collect();
                let mut next = idx + 1;
                while next < raw_messages.len() && raw_messages[next]["role"] == "tool" {
                    if let Some(tid) = raw_messages[next]["tool_call_id"].as_str() {
                        if expected_set.contains(tid) && emitted_replies.insert(tid.to_owned()) {
                            messages.push(raw_messages[next].clone());
                        }
                    }
                    next += 1;
                }
                for cid in expected {
                    if emitted_replies.insert(cid.clone()) {
                        let content = recorded_outputs.get(&cid).cloned().unwrap_or_else(|| {
                            if unknown_ids.contains(&cid) {
                                UNKNOWN_OUTCOME_MSG.to_owned()
                            } else {
                                "Session interrupted before this tool call started. It was NOT executed."
                                    .to_owned()
                            }
                        });
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": cid,
                            "content": content
                        }));
                    }
                }
                idx = next;
                continue;
            }
        }
        idx += 1;
    }

    RecoveredState {
        messages,
        pending,
        constraints,
        check,
        snapshots,
        operator_mode,
        study_mode,
        had_checkpoint: checkpoint.is_some(),
    }
}

/// Capture a non-intrusive Git tree object snapshot of the worktree (tracked + untracked
/// non-ignored files) using a temporary `GIT_INDEX_FILE` without touching `.git/index`.
pub(super) fn capture_worktree_snapshot(worktree: &Path) -> Option<String> {
    if !worktree.join(".git").exists() {
        let probe = Command::new("git")
            .args(["-C"])
            .arg(worktree)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .ok()?;
        if !probe.status.success() {
            return None;
        }
    }
    let tmp_idx = std::env::temp_dir().join(format!(
        "twig-snap-idx-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&tmp_idx);
    let read_head = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["read-tree", "HEAD"])
        .env("GIT_INDEX_FILE", &tmp_idx)
        .output();
    if let Ok(o) = read_head {
        let _ = o;
    }
    let add_ok = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["add", "-A"])
        .env("GIT_INDEX_FILE", &tmp_idx)
        .output()
        .is_ok_and(|o| o.status.success());
    if !add_ok {
        let _ = std::fs::remove_file(&tmp_idx);
        return None;
    }
    let tree_out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("write-tree")
        .env("GIT_INDEX_FILE", &tmp_idx)
        .output()
        .ok();
    let _ = std::fs::remove_file(&tmp_idx);
    let out = tree_out?;
    if !out.status.success() {
        return None;
    }
    let oid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!oid.is_empty()).then_some(oid)
}

/// Restore the working tree to a previously captured Git tree OID without altering `HEAD`
/// or the user's staged `.git/index` beyond matching the target snapshot.
pub(super) fn restore_worktree_snapshot(worktree: &Path, tree_oid: &str) -> Result<String, String> {
    if tree_oid.trim().is_empty() {
        return Err("empty snapshot tree id".into());
    }
    let tmp_idx = std::env::temp_dir().join(format!(
        "twig-restore-idx-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let cleanup = |p: &Path| {
        let _ = std::fs::remove_file(p);
    };
    let rt = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["read-tree", tree_oid])
        .env("GIT_INDEX_FILE", &tmp_idx)
        .output()
        .map_err(|e| format!("git read-tree: {e}"))?;
    if !rt.status.success() {
        cleanup(&tmp_idx);
        return Err(format!(
            "git read-tree failed: {}",
            String::from_utf8_lossy(&rt.stderr).trim()
        ));
    }
    // Remove files created after the snapshot that do not exist in `tree_oid` (excluding ignored files).
    let snap_files_out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["ls-files", "-z"])
        .env("GIT_INDEX_FILE", &tmp_idx)
        .output()
        .map_err(|e| format!("git ls-files: {e}"))?;
    let snap_set: HashSet<String> = String::from_utf8_lossy(&snap_files_out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    let cur_files_out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .output()
        .map_err(|e| format!("git ls-files current: {e}"))?;
    let mut removed = 0usize;
    for rel in String::from_utf8_lossy(&cur_files_out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
    {
        if !snap_set.contains(rel) {
            let p = worktree.join(rel);
            if p.is_file() && std::fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
    }

    let co = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["checkout-index", "-a", "-f"])
        .env("GIT_INDEX_FILE", &tmp_idx)
        .output()
        .map_err(|e| format!("git checkout-index: {e}"))?;
    cleanup(&tmp_idx);
    if !co.status.success() {
        return Err(format!(
            "git checkout-index failed: {}",
            String::from_utf8_lossy(&co.stderr).trim()
        ));
    }
    Ok(format!(
        "restored worktree to snapshot {} ({} files, {} extra removed)",
        &tree_oid[..tree_oid.len().min(10)],
        snap_set.len(),
        removed
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub sid: String,
    pub label: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub parent_sid: Option<String>,
    pub role: Option<String>,
    pub model: Option<String>,
    pub turns: usize,
    pub age: String,
    #[serde(default)]
    pub status: String,
}

/// Scan `<state_dir>/sessions/*.jsonl` (and optional `subagents/*-runs/*.jsonl`) newest-first.
pub fn scan_sessions(repo_filter: Option<&Path>, limit: usize) -> Vec<SessionInfo> {
    let filter_canon = repo_filter.and_then(|p| p.canonicalize().ok());
    let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
    for dir in [paths::state_dir().join("sessions")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.len() == 0 {
                continue;
            }
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((mtime, path));
        }
    }
    candidates.sort_by_key(|a| std::cmp::Reverse(a.0));

    let now = SystemTime::now();
    let mut out = Vec::new();
    for (mtime, path) in candidates {
        if out.len() >= limit {
            break;
        }
        let Some(info) = inspect_session_file(&path, mtime, now) else {
            continue;
        };
        if let Some(ref want_repo) = filter_canon {
            let matches_repo = info
                .worktree
                .as_deref()
                .and_then(|w| Path::new(w).canonicalize().ok())
                .is_some_and(|w| &w == want_repo);
            if !matches_repo {
                continue;
            }
        }
        let mut info = info;
        if out.len() < 15 && info.status == "settled" {
            if let Some(ref wt) = info.worktree {
                if Path::new(wt).is_dir() {
                    let dirty = Command::new("git")
                        .args(["-C", wt, "status", "--porcelain", "-uno"])
                        .output()
                        .ok()
                        .is_some_and(|o| o.status.success() && !o.stdout.is_empty());
                    if dirty {
                        info.status = "dirty".to_owned();
                    }
                }
            }
        }
        out.push(info);
    }
    out
}

fn inspect_session_file(path: &Path, mtime: SystemTime, now: SystemTime) -> Option<SessionInfo> {
    let sid = path.file_stem()?.to_str()?.to_owned();
    let file = std::fs::File::open(path).ok()?;
    let mut title: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut worktree: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut parent_sid: Option<String> = None;
    let mut role: Option<String> = None;
    let mut model: Option<String> = None;
    let mut turns = 0usize;
    let mut pending_approvals = HashSet::new();

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match row["kind"].as_str() {
            Some("session.meta") => {
                if let Some(w) = row["worktree"].as_str().filter(|s| !s.is_empty()) {
                    worktree = Some(w.to_owned());
                }
                if let Some(b) = row["branch"].as_str().filter(|s| !s.is_empty()) {
                    branch = Some(b.to_owned());
                }
                if parent_sid.is_none() {
                    parent_sid = row["parent_sid"].as_str().map(str::to_owned);
                }
                if let Some(r) = row["role"].as_str().filter(|s| !s.is_empty()) {
                    role = Some(r.to_owned());
                }
                if let Some(m) = row["model"].as_str().filter(|s| !s.is_empty()) {
                    model = Some(m.to_owned());
                }
            }
            Some("approval.requested") => {
                if let Some(id) = row["id"].as_str() {
                    pending_approvals.insert(id.to_owned());
                }
            }
            Some("approval.resolved") | Some("tool.result") => {
                if let Some(id) = row["id"].as_str() {
                    pending_approvals.remove(id);
                }
            }
            Some("session.forked") if parent_sid.is_none() => {
                parent_sid = row["from"].as_str().map(str::to_owned);
            }
            Some("session.renamed") => {
                if let Some(t) = row["title"].as_str() {
                    title = Some(t.to_owned());
                }
            }
            Some("message") => match row["message"]["role"].as_str() {
                Some("user") if first_user.is_none() => {
                    let text = row["message"]["content"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| {
                            row["message"]["content"]
                                .as_array()
                                .and_then(|arr| arr.iter().find_map(|p| p["text"].as_str()))
                                .map(str::to_owned)
                        });
                    if let Some(c) = text {
                        let one: String = c.replace('\n', " ").trim().chars().take(72).collect();
                        if !one.is_empty() {
                            first_user = Some(one);
                        }
                    }
                }
                Some("assistant") => turns += 1,
                _ => {}
            },
            _ => {}
        }
    }
    let label = title
        .or(first_user)
        .unwrap_or_else(|| "(empty session)".to_owned());
    let secs = now.duration_since(mtime).unwrap_or_default().as_secs();
    let age = if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    };
    let status = if !pending_approvals.is_empty() {
        "approval".to_owned()
    } else {
        "settled".to_owned()
    };
    Some(SessionInfo {
        sid,
        label,
        worktree,
        branch,
        parent_sid,
        role,
        model,
        turns,
        age,
        status,
    })
}

/// Pure-Rust fast keyword search across saved session transcripts (`twig session search`).
pub fn search_sessions(query: &str, repo_filter: Option<&Path>, limit: usize) -> Vec<Value> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let filter_canon = repo_filter.and_then(|p| p.canonicalize().ok());
    let mut dirs = vec![paths::state_dir().join("sessions")];
    let sub_root = paths::state_dir().join("subagents");
    if let Ok(entries) = std::fs::read_dir(&sub_root) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                dirs.push(e.path());
            }
        }
    }
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                if let Ok(m) = e.metadata() {
                    if m.len() > 0 {
                        files.push((m.modified().unwrap_or(SystemTime::UNIX_EPOCH), p));
                    }
                }
            }
        }
    }
    files.sort_by_key(|a| std::cmp::Reverse(a.0));

    let mut hits = Vec::new();
    for (_, path) in files {
        if hits.len() >= limit {
            break;
        }
        let Some(sid) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut session_repo: Option<String> = None;
        let mut matched_snippets: Vec<String> = Vec::new();
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(row) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if row["kind"] == "session.meta" && session_repo.is_none() {
                session_repo = row["worktree"].as_str().map(str::to_owned);
            }
            if row["kind"] == "message" {
                let role = row["message"]["role"].as_str().unwrap_or("msg");
                if let Some(content) = row["message"]["content"].as_str() {
                    if content.to_lowercase().contains(&q) {
                        let snippet: String = content
                            .lines()
                            .find(|l| l.to_lowercase().contains(&q))
                            .unwrap_or(content)
                            .trim()
                            .chars()
                            .take(140)
                            .collect();
                        matched_snippets.push(format!("[{role}] {snippet}"));
                        if matched_snippets.len() >= 3 {
                            break;
                        }
                    }
                }
            }
        }
        if matched_snippets.is_empty() {
            continue;
        }
        if let Some(ref want_repo) = filter_canon {
            let ok = session_repo
                .as_deref()
                .and_then(|w| Path::new(w).canonicalize().ok())
                .is_some_and(|w| &w == want_repo);
            if !ok {
                continue;
            }
        }
        hits.push(json!({
            "sid": sid,
            "worktree": session_repo,
            "matches": matched_snippets,
        }));
    }
    hits
}

/// Format a saved session `<sid>` into readable line-oriented text for `read(path="session:<sid>")`
/// and `twig session show <sid>`.
pub fn format_session_transcript(sid: &str) -> Result<String, String> {
    let trimmed = sid.trim();
    if !paths::valid_session(trimmed) {
        return Err(format!("invalid session id {trimmed:?}"));
    }
    let mut candidates = vec![paths::state_dir()
        .join("sessions")
        .join(format!("{trimmed}.jsonl"))];
    let sub_root = paths::state_dir().join("subagents");
    if let Ok(entries) = std::fs::read_dir(&sub_root) {
        for e in entries.flatten() {
            let p = e.path().join(format!("{trimmed}.jsonl"));
            if p.exists() {
                candidates.push(p);
            }
        }
    }
    let path = candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| format!("session {trimmed:?} not found"))?;
    let file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = String::new();
    out.push_str(&format!("# Session {trimmed}\n"));
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match row["kind"].as_str() {
            Some("session.meta") => {
                out.push_str(&format!(
                    "worktree: {} · branch: {} · role: {} · model: {}\n\n",
                    row["worktree"].as_str().unwrap_or("-"),
                    row["branch"].as_str().unwrap_or("-"),
                    row["role"].as_str().unwrap_or("code"),
                    row["model"].as_str().unwrap_or("-"),
                ));
            }
            Some("message") => {
                let role = row["message"]["role"].as_str().unwrap_or("msg");
                if let Some(text) = row["message"]["content"].as_str() {
                    if !text.trim().is_empty() {
                        out.push_str(&format!("## {role}\n{text}\n\n"));
                    }
                }
                if let Some(calls) = row["message"]["tool_calls"].as_array() {
                    for c in calls {
                        let name = c["function"]["name"].as_str().unwrap_or("tool");
                        let args = c["function"]["arguments"].as_str().unwrap_or("{}");
                        out.push_str(&format!("-> call {name}({args})\n"));
                    }
                    out.push('\n');
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Best-effort lightweight status broadcast for external multiplexer monitors (`herdr` / `tmux`).
pub fn notify_herdr(state: &str, detail: &str) {
    #[cfg(unix)]
    {
        use std::io::Write;
        let Ok(sock_path) = std::env::var("HERDR_SOCKET_PATH") else {
            return;
        };
        if sock_path.trim().is_empty() {
            return;
        }
        let pane_id = std::env::var("HERDR_PANE_ID")
            .or_else(|_| std::env::var("TMUX_PANE"))
            .unwrap_or_default();
        let snippet: String = detail.replace('\n', " ").chars().take(120).collect();
        let payload = json!({
            "event": "status",
            "agent": "twig",
            "pane_id": pane_id,
            "pid": std::process::id(),
            "state": state,
            "detail": snippet,
        });
        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock_path) {
            let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(25)));
            let _ = writeln!(stream, "{payload}");
        } else if let Ok(dgram) = std::os::unix::net::UnixDatagram::unbound() {
            let _ = dgram.send_to(payload.to_string().as_bytes(), &sock_path);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (state, detail);
    }
}

fn locate_session_jsonl(sid: &str) -> Option<PathBuf> {
    if !paths::valid_session(sid) {
        return None;
    }
    let primary = paths::state_dir()
        .join("sessions")
        .join(format!("{sid}.jsonl"));
    if primary.exists() {
        return Some(primary);
    }
    let sub_root = paths::state_dir().join("subagents");
    for entry in std::fs::read_dir(sub_root).ok()?.flatten() {
        let p = entry.path().join(format!("{sid}.jsonl"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn read_session_worktree(sid: &str) -> Option<String> {
    let path = locate_session_jsonl(sid)?;
    let file = std::fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if row["kind"] == "session.meta" {
            if let Some(w) = row["worktree"].as_str().filter(|s| !s.is_empty()) {
                return Some(w.to_owned());
            }
        }
    }
    None
}

pub(super) fn load_session_patch(sid: &str, extra_artifacts: Option<&Path>) -> Result<String, String> {
    let trimmed = sid.trim();
    if !paths::valid_session(trimmed) {
        return Err(format!("invalid session id {trimmed:?}"));
    }
    let patch_name = format!("subagent-{trimmed}.patch");
    let mut candidates = Vec::new();
    if let Some(dir) = extra_artifacts {
        candidates.push(dir.join(&patch_name));
    }
    candidates.push(
        paths::state_dir()
            .join("subagents")
            .join(format!("{trimmed}-runs"))
            .join(&patch_name),
    );
    candidates.push(
        paths::state_dir()
            .join("sessions")
            .join("artifacts")
            .join(&patch_name),
    );
    for p in candidates {
        if p.is_file() {
            return std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()));
        }
    }
    let sub_wt = paths::state_dir()
        .join("subagents")
        .join(format!("{trimmed}-wt"));
    if sub_wt.is_dir() {
        let out = Command::new("git")
            .arg("-C")
            .arg(&sub_wt)
            .args(["diff", "HEAD"])
            .output()
            .map_err(|e| format!("git diff: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !text.trim().is_empty() {
            return Ok(text);
        }
    }
    if let Some(wt) = read_session_worktree(trimmed) {
        let out = Command::new("git")
            .args(["-C", &wt, "diff", "HEAD"])
            .output()
            .map_err(|e| format!("git diff: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !text.trim().is_empty() {
            return Ok(text);
        }
    }
    Err(format!("no saved patch or uncommitted diff found for session {trimmed:?}"))
}

fn apply_patch_in_repo(repo: &Path, patch_text: &str, sid: &str) -> Result<String, String> {
    if patch_text.trim().is_empty() {
        return Err(format!("empty patch for session {sid}"));
    }
    if patch_text.lines().any(|l| {
        l.starts_with("new file mode 120000")
            || l.starts_with("old mode 120000")
            || l.starts_with("new mode 120000")
    }) {
        return Err(format!(
            "refused patch from session {sid}: symlink mode 120000 is disallowed"
        ));
    }
    let tmp = std::env::temp_dir().join(format!(
        "twig-cli-apply-{}-{}.patch",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::write(&tmp, patch_text).map_err(|e| e.to_string())?;
    let chk = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["apply", "--check"])
        .arg(&tmp)
        .output()
        .map_err(|e| format!("git apply --check: {e}"))?;
    if !chk.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "patch from {sid} does not apply cleanly:\n{}",
            String::from_utf8_lossy(&chk.stderr).trim()
        ));
    }
    let app = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("apply")
        .arg(&tmp)
        .output()
        .map_err(|e| format!("git apply: {e}"))?;
    let _ = std::fs::remove_file(&tmp);
    if !app.status.success() {
        return Err(format!(
            "git apply failed:\n{}",
            String::from_utf8_lossy(&app.stderr).trim()
        ));
    }
    Ok(format!("applied patch from session {sid}"))
}

/// Clean up 0-byte empty session/lock remnants and stale clean subagent worktrees.
pub fn gc_sessions_and_subagents() -> Result<(usize, usize), String> {
    let mut empty_removed = 0usize;
    let sessions_dir = paths::state_dir().join("sessions");
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("jsonl")
                && entry.metadata().is_ok_and(|m| m.len() == 0)
            {
                let lock = p.with_extension("lock");
                if std::fs::remove_file(&p).is_ok() {
                    empty_removed += 1;
                    let _ = std::fs::remove_file(lock);
                }
            }
        }
    }
    let mut wt_removed = 0usize;
    let sub_root = paths::state_dir().join("subagents");
    if let Ok(entries) = std::fs::read_dir(&sub_root) {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if p.is_dir() && name.ends_with("-wt") {
                let clean = Command::new("git")
                    .arg("-C")
                    .arg(&p)
                    .args(["status", "--porcelain"])
                    .output()
                    .is_ok_and(|o| o.status.success() && o.stdout.is_empty());
                if clean && std::fs::remove_dir_all(&p).is_ok() {
                    wt_removed += 1;
                }
            }
        }
    }
    let _ = Command::new("git").args(["worktree", "prune"]).output();
    Ok((wt_removed, empty_removed))
}

/// Non-interactive single-shot execution (`twig exec [flags] "<prompt>"`).
pub fn run_headless_exec(args: &[String]) -> Result<i32, String> {
    use std::io::Read;
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

    let sid = resume_sid.unwrap_or_else(paths::session_id);
    if !paths::valid_session(&sid) {
        return Err(format!("invalid session id {sid:?}"));
    }
    if repo == "." {
        if let Some(saved_wt) = read_session_worktree(&sid) {
            if Path::new(&saved_wt).is_dir() {
                repo = saved_wt;
            }
        }
    }

    let (cfg, _) = crate::config::HarnessConfig::resolve()?;
    let provider = crate::llm::Provider::new();
    let runs_dir = paths::state_dir().join("sessions");
    let mut daemon = super::Daemon::new(&provider, &repo, &runs_dir, &sid, &cfg)?;
    if let Some(m) = model {
        daemon.dispatch("model.set", &json!({"model": m}), &mut |_| {})?;
    }
    if let Some(e) = effort {
        daemon.dispatch("effort.set", &json!({"effort": e}), &mut |_| {})?;
    }
    if read_only || matches!(role.as_str(), "plan" | "advisor" | "scout" | "reviewer") {
        daemon.dispatch("mode.set", &json!({"plan": true}), &mut |_| {})?;
    }
    let mut turn_params = json!({"input": prompt, "role": role});
    if let Some(cmd) = check_cmd {
        turn_params["check"] = json!({"command": cmd});
    }

    let out = if events_out {
        use std::io::Write;
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
        .args(["-C", &repo, "diff", "HEAD", "--shortstat"])
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
    let usage = crate::llm::summarize_usage(&rows);

    if events_out {
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
    } else if json_out {
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

/// CLI management for first-class sessions (`twig session <list|search|show|spawn|send|diff|archive>`).
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
            let patch = load_session_patch(sid, None)?;
            let msg = apply_patch_in_repo(Path::new(&repo), &patch, sid)?;
            println!("{msg}");
            Ok(0)
        }
        "gc" => {
            let (wt, empty) = gc_sessions_and_subagents()?;
            println!("gc: removed {wt} clean subagent worktree(s) and {empty} empty session file(s)");
            Ok(0)
        }
        "archive" => {
            let sid = args.get(1).ok_or("usage: twig session archive <SID>")?;
            if !paths::valid_session(sid) {
                return Err(format!("invalid session id {sid:?}"));
            }
            let sessions_dir = paths::state_dir().join("sessions");
            let src = sessions_dir.join(format!("{sid}.jsonl"));
            if !src.exists() {
                return Err(format!("session not found: {}", src.display()));
            }
            let archive_dir = sessions_dir.join("archive");
            std::fs::create_dir_all(&archive_dir).map_err(|e| e.to_string())?;
            let dst = archive_dir.join(format!("{sid}.jsonl"));
            std::fs::rename(&src, &dst).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(sessions_dir.join(format!("{sid}.lock")));
            println!("{}", dst.display());
            Ok(0)
        }
        other => Err(format!(
            "unknown session subcommand {other:?}; expected list, search, show, spawn, send, diff, apply, archive, or gc"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_preserves_adjacent_tool_pairs_across_compaction_and_interruptions() {
        let rows = vec![
            // Old pre-compaction tool result that must NOT resurrect as an orphan after checkpoint:
            json!({"kind": "tool.result", "id": "old-compacted-1", "output": "ancient"}),
            json!({
                "kind": "messages.checkpoint",
                "messages": [
                    {"role": "user", "content": "[compacted summary]"},
                    {"role": "assistant", "content": "ok"}
                ]
            }),
            json!({
                "kind": "message",
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{"id": "c-crash", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]
                }
            }),
            json!({"kind": "tool.started", "id": "c-crash", "tool": "bash", "args": {}}),
            // User message recorded after interrupted tool call:
            json!({
                "kind": "message",
                "message": {"role": "user", "content": "continue carefully"}
            }),
        ];

        let state = recover_session(&rows, None);
        assert!(
            crate::runtime::context::validate_tool_pairs(&state.messages).is_ok(),
            "recovered messages violated tool pairing: {:?}",
            state.messages
        );
        assert!(
            !state
                .messages
                .iter()
                .any(|m| m["tool_call_id"] == "old-compacted-1")
        );
        assert_eq!(state.messages[3]["role"], "tool");
        assert_eq!(state.messages[3]["tool_call_id"], "c-crash");
        assert_eq!(state.messages[4]["role"], "user");
    }

    #[test]
    fn repo_instructions_ignore_symlinks_escaping_worktree() {
        let base = std::env::temp_dir().join(format!("twig-repo-doc-{}", std::process::id()));
        let repo = base.join("repo");
        let outside = base.join("outside.txt");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(&outside, "SECRET_HOST_KEY").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, repo.join("AGENTS.md")).unwrap();
            let mut sys = String::from("base");
            append_repo_instructions(&repo, "claude-sonnet", &mut sys);
            assert!(!sys.contains("SECRET_HOST_KEY"));
        }
        assert!(run_session_cli(&["archive".into(), "../escape".into()]).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
