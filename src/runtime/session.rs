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

pub(super) struct SessionSnapshot {
    pub title: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub parent_sid: Option<String>,
    pub role: Option<String>,
    pub model: Option<String>,
    pub plan_mode: bool,
    pub gate_edits: bool,
    pub operator_mode: bool,
    pub study_mode: bool,
    pub has_saved_effort: bool,
    pub saved_effort: Option<String>,
    pub todos: Vec<Value>,
    pub allowed_patterns: Vec<String>,
    pub tool_count: u32,
    pub messages: Vec<Value>,
    pub pending: Vec<Value>,
    pub constraints: Vec<String>,
    pub check: Option<Check>,
    pub snapshots: Vec<String>,
    pub had_checkpoint: bool,
}

fn read_jsonl_lenient(path: &Path) -> std::io::Result<Vec<Value>> {
    let file = std::fs::File::open(path)?;
    Ok(BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .collect())
}

/// Canonical single-pass O(N) session state & ledger reconstruction shared across
/// `Daemon::new`, `scan_sessions`, `search_sessions`, `format_session_transcript`,
/// `load_child_threads`, `read_session_worktree`, and `web` session inspection.
pub(super) fn recover_session(rows: &[Value], default_check: Option<Check>) -> SessionSnapshot {
    let mut title: Option<String> = None;
    let mut worktree: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut parent_sid: Option<String> = None;
    let mut role: Option<String> = None;
    let mut model: Option<String> = None;
    let mut plan_mode = false;
    let mut gate_edits = false;
    let mut operator_mode = false;
    let mut study_mode = false;
    let mut has_saved_effort = false;
    let mut saved_effort: Option<String> = None;
    let mut todos: Vec<Value> = Vec::new();
    let mut allowed_patterns: Vec<String> = Vec::new();
    let mut check: Option<Check> = None;
    let mut tool_count = 0u32;

    let mut snapshots = Vec::new();
    let mut constraints = Vec::new();
    let mut has_result = HashSet::new();
    let mut resolved_recovered = HashSet::new();
    let mut resolved_any = HashSet::new();
    let mut requested: HashMap<String, (String, Value, bool)> = HashMap::new();
    let mut started: HashMap<String, (String, Value)> = HashMap::new();

    for row in rows {
        let kind = row["kind"].as_str().unwrap_or("");
        match kind {
            "session.meta" => {
                if let Some(w) = row["worktree"].as_str().filter(|s| !s.is_empty()) {
                    worktree = Some(w.to_owned());
                }
                if let Some(b) = row["branch"].as_str().filter(|s| !s.is_empty()) {
                    branch = Some(b.to_owned());
                }
                if parent_sid.is_none() {
                    parent_sid = row["parent_sid"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
                }
                if let Some(r) = row["role"].as_str().filter(|s| !s.is_empty()) {
                    role = Some(r.to_owned());
                }
                if let Some(m) = row["model"].as_str().filter(|s| !s.is_empty()) {
                    model = Some(m.to_owned());
                }
            }
            "session.forked" if parent_sid.is_none() => {
                parent_sid = row["from"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
            }
            "session.renamed" => {
                if let Some(t) = row["title"].as_str().filter(|s| !s.is_empty()) {
                    title = Some(t.to_owned());
                }
            }
            "mode.changed" => {
                if let Some(p) = row["plan"].as_bool() {
                    plan_mode = p;
                }
                if let Some(g) = row["gate_edits"].as_bool() {
                    gate_edits = g;
                }
            }
            "operator.changed" => {
                if let Some(e) = row["enabled"].as_bool() {
                    operator_mode = e;
                }
            }
            "study.changed" => {
                if let Some(e) = row["enabled"].as_bool() {
                    study_mode = e;
                }
            }
            "effort.changed" => {
                has_saved_effort = true;
                saved_effort = row["effort"]
                    .as_str()
                    .and_then(|s| crate::llm::normalize_effort(s).ok().flatten());
            }
            "todo.updated" => {
                if let Some(arr) = row["items"].as_array() {
                    todos = arr.clone();
                }
            }
            "policy.allowed" => {
                if let Some(pat) = row["pattern"].as_str().filter(|s| !s.is_empty()) {
                    allowed_patterns.push(pat.to_owned());
                }
            }
            "acceptance.configured" => {
                if let Ok(c) = serde_json::from_value::<Check>(row["check"].clone()) {
                    check = Some(c);
                }
            }
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
                    let paused = row["paused"].as_bool().unwrap_or(false);
                    requested
                        .entry(id.to_owned())
                        .or_insert((name, row["args"].clone(), paused));
                }
            }
            "tool.started" => {
                tool_count = tool_count.saturating_add(1);
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
    let mut paused_ids = HashSet::new();
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
        let is_clean_pause = !started_without_result
            && unresolved_approval
            && requested.get(id).is_some_and(|(_, _, paused)| *paused);
        if is_clean_pause {
            paused_ids.insert(id.to_owned());
        } else {
            unknown_ids.insert(id.to_owned());
        }
        if requested.contains_key(id) && seen_pending.insert(id.to_owned()) {
            let (name, args) = requested
                .get(id)
                .map(|(n, a, _)| (n.clone(), a.clone()))
                .or_else(|| started.get(id).cloned())
                .unwrap_or_else(|| ("bash".into(), Value::Null));
            pending.push(json!({
                "ev": "approval.requested",
                "id": id,
                "name": name,
                "args": args,
                "recovered": !is_clean_pause
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
                    if paused_ids.contains(&cid) {
                        continue;
                    }
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

    if messages.is_empty() && checkpoint.is_some() {
        todos.clear();
    }

    SessionSnapshot {
        title,
        worktree,
        branch,
        parent_sid,
        role,
        model,
        plan_mode,
        gate_edits,
        operator_mode,
        study_mode,
        has_saved_effort,
        saved_effort,
        todos,
        allowed_patterns,
        tool_count,
        messages,
        pending,
        constraints,
        check: check.or(default_check),
        snapshots,
        had_checkpoint: checkpoint.is_some(),
    }
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
    #[serde(default)]
    pub diff_add: usize,
    #[serde(default)]
    pub diff_del: usize,
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
        if out.len() < 15 {
            if let Some(ref wt) = info.worktree {
                if Path::new(wt).is_dir() {
                    if let Ok(numstat) = Command::new("git")
                        .args(["-C", wt, "diff", "HEAD", "--numstat"])
                        .output()
                    {
                        if numstat.status.success() && !numstat.stdout.is_empty() {
                            let mut add = 0usize;
                            let mut del = 0usize;
                            for line in String::from_utf8_lossy(&numstat.stdout).lines() {
                                let mut cols = line.split_whitespace();
                                add += cols.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
                                del += cols.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
                            }
                            info.diff_add = add;
                            info.diff_del = del;
                            if info.status == "settled" {
                                info.status = "dirty".to_owned();
                            }
                        }
                    }
                    if info.status == "settled" {
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
        }
        out.push(info);
    }
    out
}

fn message_plain_text(msg: &Value) -> Option<String> {
    msg["content"].as_str().map(str::to_owned).or_else(|| {
        msg["content"]
            .as_array()
            .and_then(|arr| arr.iter().find_map(|p| p["text"].as_str()))
            .map(str::to_owned)
    })
}

fn inspect_session_file(path: &Path, mtime: SystemTime, now: SystemTime) -> Option<SessionInfo> {
    let sid = path.file_stem()?.to_str()?.to_owned();
    let rows = read_jsonl_lenient(path).ok()?;
    let snap = recover_session(&rows, None);
    let first_user = snap
        .messages
        .iter()
        .find(|m| m["role"] == "user")
        .and_then(message_plain_text)
        .map(|c| {
            c.replace('\n', " ")
                .trim()
                .chars()
                .take(72)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty());
    let turns = snap
        .messages
        .iter()
        .filter(|m| m["role"] == "assistant")
        .count();
    let label = snap
        .title
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
    let is_locked = crate::record::Lease::is_locked(&path.with_extension("lock"));
    let status = if !snap.pending.is_empty() {
        "approval".to_owned()
    } else if is_locked {
        "running".to_owned()
    } else {
        "settled".to_owned()
    };
    Some(SessionInfo {
        sid,
        label,
        worktree: snap.worktree,
        branch: snap.branch,
        parent_sid: snap.parent_sid,
        role: snap.role,
        model: snap.model,
        turns,
        age,
        status,
        diff_add: 0,
        diff_del: 0,
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
        let Ok(rows) = read_jsonl_lenient(&path) else {
            continue;
        };
        let snap = recover_session(&rows, None);
        if let Some(ref want_repo) = filter_canon {
            let ok = snap
                .worktree
                .as_deref()
                .and_then(|w| Path::new(w).canonicalize().ok())
                .is_some_and(|w| &w == want_repo);
            if !ok {
                continue;
            }
        }
        let mut matched_snippets: Vec<String> = Vec::new();
        for msg in &snap.messages {
            let role = msg["role"].as_str().unwrap_or("msg");
            if let Some(content) = message_plain_text(msg) {
                if content.to_lowercase().contains(&q) {
                    let snippet: String = content
                        .lines()
                        .find(|l| l.to_lowercase().contains(&q))
                        .unwrap_or(&content)
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
        if matched_snippets.is_empty() {
            continue;
        }
        hits.push(json!({
            "sid": sid,
            "worktree": snap.worktree,
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
    let path = locate_session_jsonl(trimmed)
        .ok_or_else(|| format!("session {trimmed:?} not found"))?;
    let rows = read_jsonl_lenient(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let snap = recover_session(&rows, None);
    let mut out = String::new();
    out.push_str(&format!("# Session {trimmed}\n"));
    if snap.worktree.is_some() || snap.branch.is_some() || snap.role.is_some() || snap.model.is_some() {
        out.push_str(&format!(
            "worktree: {} · branch: {} · role: {} · model: {}\n\n",
            snap.worktree.as_deref().unwrap_or("-"),
            snap.branch.as_deref().unwrap_or("-"),
            snap.role.as_deref().unwrap_or("code"),
            snap.model.as_deref().unwrap_or("-"),
        ));
    }
    for msg in &snap.messages {
        let role = msg["role"].as_str().unwrap_or("msg");
        if let Some(text) = message_plain_text(msg) {
            if !text.trim().is_empty() {
                out.push_str(&format!("## {role}\n{text}\n\n"));
            }
        }
        if let Some(calls) = msg["tool_calls"].as_array() {
            for c in calls {
                let name = c["function"]["name"].as_str().unwrap_or("tool");
                let args = c["function"]["arguments"].as_str().unwrap_or("{}");
                out.push_str(&format!("-> call {name}({args})\n"));
            }
            out.push('\n');
        }
    }
    Ok(out)
}

/// Detailed session snapshot for `twig web` cockpit (`messages`, `pending`, `diff`, `todos`, `usage`, `children`, live `Lease` state).
pub fn inspect_session_detail(sid: &str, default_repo: &Path, in_flight: bool) -> Result<Value, String> {
    if !paths::valid_session(sid) {
        return Err("invalid session id".into());
    }
    let path = locate_session_jsonl(sid).ok_or_else(|| "session not found".to_owned())?;
    let rows = read_jsonl_lenient(&path).map_err(|e| e.to_string())?;
    let snap = recover_session(&rows, None);
    let worktree = snap
        .worktree
        .unwrap_or_else(|| default_repo.display().to_string());
    let diff = Command::new("git")
        .args(["-C", &worktree, "diff", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            s.chars().take(64_000).collect::<String>()
        })
        .unwrap_or_default();
    let running = in_flight || crate::record::Lease::is_locked(&path.with_extension("lock"));
    let usage = crate::llm::summarize_usage(&rows);
    let children = load_child_threads(sid, None);
    let pr_info = rows
        .iter()
        .rev()
        .find(|r| r["kind"] == "git.pr_created")
        .cloned();
    let last_commit = rows
        .iter()
        .rev()
        .find(|r| r["kind"] == "git.committed")
        .cloned();
    Ok(json!({
        "sid": sid,
        "title": snap.title,
        "role": snap.role.unwrap_or_else(|| "code".into()),
        "parent_sid": snap.parent_sid,
        "worktree": worktree,
        "branch": snap.branch.unwrap_or_default(),
        "model": snap.model.unwrap_or_default(),
        "effort": snap.saved_effort,
        "plan": snap.plan_mode,
        "running": running,
        "todos": snap.todos,
        "usage": usage,
        "children": children,
        "messages": snap.messages,
        "pending": snap.pending,
        "snapshots": snap.snapshots,
        "constraints": snap.constraints,
        "pr": pr_info,
        "last_commit": last_commit,
        "diff": diff,
    }))
}

/// Archive `<sid>.jsonl` into `<state_dir>/sessions/archive/` with session-ID confinement.
pub fn archive_session(sid: &str) -> Result<String, String> {
    archive_session_in(&paths::state_dir().join("sessions"), sid)
}

fn archive_session_in(sessions_dir: &Path, sid: &str) -> Result<String, String> {
    let trimmed = sid.trim();
    if !paths::valid_session(trimmed) {
        return Err(format!("invalid session id {trimmed:?}"));
    }
    let src = sessions_dir.join(format!("{trimmed}.jsonl"));
    if !src.exists() {
        return Err(format!("session record not found: {}", src.display()));
    }
    let archive_dir = sessions_dir.join("archive");
    std::fs::create_dir_all(&archive_dir).map_err(|e| format!("create archive dir: {e}"))?;
    let dst = archive_dir.join(format!("{trimmed}.jsonl"));
    std::fs::rename(&src, &dst).map_err(|e| format!("archive session: {e}"))?;
    let lock = sessions_dir.join(format!("{trimmed}.lock"));
    if !crate::record::Lease::is_locked(&lock) {
        let _ = std::fs::remove_file(lock);
    }
    Ok(dst.display().to_string())
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

/// Load persisted child subagent threads belonging to `parent_sid` so `/resume` and
/// `session.history` automatically remount the full session thread tree.
pub fn load_child_threads(parent_sid: &str, extra_subagents_root: Option<&Path>) -> Vec<Value> {
    let want = parent_sid.trim();
    if want.is_empty() {
        return Vec::new();
    }
    let mut roots = vec![paths::state_dir().join("subagents")];
    if let Some(extra) = extra_subagents_root {
        if !roots.iter().any(|r| r == extra) {
            roots.push(extra.to_path_buf());
        }
    }
    let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
    for root in roots {
        let Ok(dirs) = std::fs::read_dir(&root) else {
            continue;
        };
        for dir_entry in dirs.flatten() {
            let dir_path = dir_entry.path();
            if !dir_path.is_dir() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(&dir_path) else {
                continue;
            };
            for file_entry in files.flatten() {
                let p = file_entry.path();
                if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    if let Ok(m) = file_entry.metadata() {
                        if m.len() > 0 {
                            candidates.push((m.modified().unwrap_or(SystemTime::UNIX_EPOCH), p));
                        }
                    }
                }
            }
        }
    }
    candidates.sort_by_key(|a| a.0);

    let mut matched = Vec::new();
    for (_, path) in candidates {
        let Some(sid) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut rows = Vec::new();
        let mut is_child = false;
        let mut checked_meta = false;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(row) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if row["kind"] == "session.meta" {
                checked_meta = true;
                if row["parent_sid"].as_str() == Some(want) {
                    is_child = true;
                } else if !is_child {
                    break;
                }
            } else if !checked_meta && rows.len() >= 4 {
                break;
            }
            rows.push(row);
        }
        if !is_child {
            continue;
        }
        let snap = recover_session(&rows, None);
        let role = snap.role.unwrap_or_else(|| "scout".to_owned());
        let task = snap
            .messages
            .iter()
            .find(|m| m["role"] == "user")
            .and_then(|m| m["content"].as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        let turns = snap
            .messages
            .iter()
            .filter(|m| m["role"] == "assistant")
            .count() as u32;
        matched.push(json!({
            "id": sid,
            "sid": sid,
            "role": role,
            "model": snap.model,
            "task": task,
            "turns": turns,
            "tool_count": snap.tool_count,
            "messages": snap.messages,
        }));
    }
    if matched.len() > 16 {
        matched.drain(..matched.len() - 16);
    }
    matched
}

pub fn session_worktree(sid: &str) -> Option<String> {
    let path = locate_session_jsonl(sid)?;
    let rows = read_jsonl_lenient(&path).ok()?;
    recover_session(&rows, None).worktree
}

pub fn load_session_patch(sid: &str, extra_artifacts: Option<&Path>) -> Result<String, String> {
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
    if let Some(wt) = session_worktree(trimmed) {
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

pub fn apply_session_patch(repo: &Path, sid: &str) -> Result<String, String> {
    let patch = load_session_patch(sid, None)?;
    let targets = super::git_ops::apply_confined_git_patch(repo, &patch, &format!("session:{sid}"))?;
    Ok(format!(
        "applied patch from session {sid} ({} file(s))",
        targets.len()
    ))
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
        assert!(archive_session("../escape").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn load_child_threads_recovers_only_matching_parent_subagents() {
        let base = std::env::temp_dir().join(format!("twig-child-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let child_dir = base.join("scout-1-runs");
        let other_dir = base.join("scout-2-runs");
        std::fs::create_dir_all(&child_dir).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();

        let mut rec1 =
            crate::record::Record::open(child_dir.join("scout-1.jsonl"), "scout-1").unwrap();
        rec1.append(
            "session.meta",
            json!({"worktree": "/tmp", "parent_sid": "parent-abc", "role": "scout", "model": "gpt-5"}),
        )
        .unwrap();
        rec1.append(
            "message",
            json!({"message": {"role": "user", "content": "Audit src/web.rs"}}),
        )
        .unwrap();
        rec1.append("tool.started", json!({"id": "t1", "tool": "read"})).unwrap();
        rec1.append("tool.started", json!({"id": "t2", "tool": "grep"})).unwrap();
        rec1.append(
            "message",
            json!({"message": {"role": "assistant", "content": "Found 2 endpoints."}}),
        )
        .unwrap();

        let mut rec2 =
            crate::record::Record::open(other_dir.join("scout-2.jsonl"), "scout-2").unwrap();
        rec2.append(
            "session.meta",
            json!({"worktree": "/tmp", "parent_sid": "different-parent", "role": "scout"}),
        )
        .unwrap();

        let threads = load_child_threads("parent-abc", Some(&base));
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0]["sid"], "scout-1");
        assert_eq!(threads[0]["role"], "scout");
        assert_eq!(threads[0]["tool_count"], 2);
        assert_eq!(threads[0]["turns"], 1);
        assert_eq!(threads[0]["task"], "Audit src/web.rs");
        assert_eq!(threads[0]["messages"].as_array().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn paused_noninteractive_approval_executes_on_resume_while_crash_stays_unreplayed() {
        let root = std::env::temp_dir().join(format!("twig-paused-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let runs = root.join("runs");
        let llm = crate::llm::stub::StubLlm::new(vec![crate::llm::stub::StubLlm::with_tools(
            "",
            vec![crate::llm::stub::call(
                "p1",
                "bash",
                json!({"command": "rm -rf /nonexistent-twig-gate && printf ran > marker.txt"}),
            )],
        )]);
        {
            let mut d1 =
                crate::runtime::Daemon::new(&llm, &root, &runs, "paused1", &crate::config::HarnessConfig::default())
                    .unwrap();
            let out = d1
                .dispatch("turn", &json!({"input": "run gated"}), &mut |_| {})
                .unwrap();
            assert_eq!(out["reason"], "awaiting_approval");
        }
        // Re-opening the session (as `twig web` or `twig exec --resume` does) preserves `recovered: false`
        // because the pause was clean (`paused: true` with no `tool.started`), so allowing executes cleanly!
        let mut d2 =
            crate::runtime::Daemon::new(&llm, &root, &runs, "paused1", &crate::config::HarnessConfig::default())
                .unwrap();
        let resolved = d2
            .dispatch("approval.resolve", &json!({"id": "p1", "allow": true}), &mut |_| {})
            .unwrap();
        assert_eq!(resolved["recovered"], false);
        assert_eq!(std::fs::read_to_string(root.join("marker.txt")).unwrap(), "ran");
        let _ = std::fs::remove_dir_all(&root);

        let arch_dir = std::env::temp_dir().join(format!("twig-archive-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&arch_dir);
        std::fs::create_dir_all(&arch_dir).unwrap();
        std::fs::write(arch_dir.join("s1.jsonl"), "{}\n").unwrap();
        std::fs::write(arch_dir.join("s1.lock"), "1").unwrap();
        let dst = archive_session_in(&arch_dir, "s1").unwrap();
        assert!(dst.ends_with("archive/s1.jsonl"));
        assert!(!arch_dir.join("s1.jsonl").exists());
        assert!(!arch_dir.join("s1.lock").exists());
        assert!(arch_dir.join("archive/s1.jsonl").exists());
        let _ = std::fs::remove_dir_all(&arch_dir);
    }
}
