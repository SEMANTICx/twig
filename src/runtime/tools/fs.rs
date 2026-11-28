//! Worktree-confined file system, progressive edit, multi-file `apply_patch`,
//! Read-Before-Edit staleness guard, lazy subdirectory instructions, search, and undo.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use super::{Tools, PLAN_RO};
use crate::config::IGNORED_DIRS;
use crate::exec::{run_check, Check};

const MAX_UNDO_SNAPSHOTS: usize = 64;
pub(super) const MAX_GREP_FILE_BYTES: u64 = 1_048_576;
const MAX_GREP_MATCHES: usize = 100;
const MAX_GLOB_MATCHES: usize = 200;
const MAX_SUBDIR_DOC_CHARS: usize = 8_000;

pub(super) struct FileSnapshot {
    path: PathBuf,
    rel: String,
    previous: Option<String>,
}

pub(super) fn file_stamp(path: &Path) -> Option<(u128, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((mtime, meta.len()))
}

enum PatchOp {
    Add { path: String, content: String },
    Delete { path: String },
    Update { path: String, hunks: Vec<Vec<String>> },
}

struct PreparedPatch {
    target: PathBuf,
    rel: String,
    previous: Option<String>,
    after: Option<String>,
    summary: String,
}

impl Tools {
    pub(super) fn record_file_stamp(&mut self, target: &Path) {
        if let Some(stamp) = file_stamp(target) {
            self.read_stamps.insert(target.to_path_buf(), stamp);
        }
    }

    pub(crate) fn outcome_stamps(
        &self,
        tool: &str,
        args: &Value,
    ) -> (Option<Value>, Option<Value>) {
        if matches!(tool, "read" | "edit" | "write") {
            let single = args
                .get("path")
                .and_then(Value::as_str)
                .filter(|rel| !rel.starts_with("artifact:") && !rel.starts_with("session:"))
                .and_then(|rel| self.resolve_existing_in_worktree(rel).ok())
                .and_then(|(_, target)| self.read_stamps.get(&target).copied())
                .map(|(ns, len)| serde_json::json!([ns.to_string(), len]));
            return (single, None);
        }
        if tool == "apply_patch" {
            let mut map = serde_json::Map::new();
            if let Some(patch_text) = args.get("patch").and_then(Value::as_str) {
                if let Ok(ops) = parse_patch_ops(patch_text) {
                    for op in ops {
                        let rel_opt = match op {
                            PatchOp::Add { path, .. } | PatchOp::Update { path, .. } => Some(path),
                            PatchOp::Delete { .. } => None,
                        };
                        if let Some(rel) = rel_opt {
                            if let Ok((_, target)) = self.resolve_existing_in_worktree(&rel) {
                                if let Some(&(ns, len)) = self.read_stamps.get(&target) {
                                    map.insert(rel, serde_json::json!([ns.to_string(), len]));
                                }
                            }
                        }
                    }
                }
            }
            let multi = (!map.is_empty()).then_some(Value::Object(map));
            return (None, multi);
        }
        (None, None)
    }

    /// Re-hydrate `read_stamps` from recovered session `tool.result` rows only when the file
    /// on disk matches the exact recorded `(mtime_ns, len)` stamp (or `mtime_ms <= row_ts_ms`
    /// with zero future grace window for legacy transcripts).
    pub(crate) fn restore_read_stamps(&mut self, rows: &[Value]) {
        for row in rows {
            if row["kind"] != "tool.result" || row["exit_code"] != 0 {
                continue;
            }
            let Some(tool) = row["tool"].as_str() else {
                continue;
            };
            if tool == "apply_patch" {
                if let Some(stamps) = row.get("stamps").and_then(Value::as_object) {
                    for (rel, val) in stamps {
                        let Ok((_, target)) = self.resolve_existing_in_worktree(rel) else {
                            continue;
                        };
                        let Some((mtime_ns, len)) = file_stamp(&target) else {
                            continue;
                        };
                        if let Some(arr) = val.as_array() {
                            let rec_ns = arr
                                .first()
                                .and_then(Value::as_str)
                                .and_then(|s| s.parse::<u128>().ok());
                            let rec_len = arr.get(1).and_then(Value::as_u64);
                            if rec_ns == Some(mtime_ns) && rec_len == Some(len) {
                                self.read_stamps.insert(target, (mtime_ns, len));
                            }
                        }
                    }
                }
                continue;
            }
            if !matches!(tool, "read" | "edit" | "write") {
                continue;
            }
            let Some(rel) = row["args"]["path"].as_str() else {
                continue;
            };
            if rel.starts_with("artifact:") || rel.starts_with("session:") {
                continue;
            }
            let Ok((_, target)) = self.resolve_existing_in_worktree(rel) else {
                continue;
            };
            let Some((mtime_ns, len)) = file_stamp(&target) else {
                continue;
            };
            if let Some(arr) = row.get("stamp").and_then(Value::as_array) {
                let rec_ns = arr
                    .first()
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<u128>().ok());
                let rec_len = arr.get(1).and_then(Value::as_u64);
                if rec_ns == Some(mtime_ns) && rec_len == Some(len) {
                    self.read_stamps.insert(target, (mtime_ns, len));
                }
                continue;
            }
            let row_ts_ms = row["ts"].as_u64().unwrap_or(0) as u128;
            let mtime_ms = mtime_ns / 1_000_000;
            if row_ts_ms > 0 && mtime_ms <= row_ts_ms {
                self.read_stamps.insert(target, (mtime_ns, len));
            }
        }
    }

    /// Safely check and apply a unified git diff (e.g. from a subagent session) into the worktree,
    /// recording pre-apply undo snapshots so `/undo` can revert it cleanly.
    pub(crate) fn apply_git_patch(&mut self, patch_text: &str, label: &str) -> Result<String, String> {
        if self.is_read_only() {
            return Err(self.read_only_refusal());
        }
        let targets =
            super::super::git_ops::apply_confined_git_patch(&self.worktree, patch_text, label)?;
        let count = targets.len();
        let rels: Vec<String> = targets.iter().map(|(r, _, _)| r.clone()).collect();
        for (rel, target, prev) in targets {
            if target.exists() {
                self.record_file_stamp(&target);
            } else {
                self.read_stamps.remove(&target);
            }
            self.push_undo(target, rel, prev);
        }
        Ok(format!(
            "applied {label} ({count} file(s): {})",
            rels.join(", ")
        ))
    }

    fn check_read_guard(&self, target: &Path, display_path: &str) -> Result<(), String> {
        if !self.enforce_read_guard {
            return Ok(());
        }
        let Some(&recorded) = self.read_stamps.get(target) else {
            return Err(format!(
                "refused: {display_path} has not been read in this session; call read(path={display_path:?}) before editing or overwriting"
            ));
        };
        if let Some(current) = file_stamp(target) {
            if current != recorded {
                return Err(format!(
                    "refused: {display_path} was modified on disk since it was last read; call read(path={display_path:?}) again to inspect current contents"
                ));
            }
        }
        Ok(())
    }

    pub(super) fn discover_subdir_instructions(&mut self, root: &Path, target: &Path) -> String {
        let mut dirs = Vec::new();
        let mut cur = target.parent();
        while let Some(dir) = cur {
            if dir == root || !dir.starts_with(root) {
                break;
            }
            dirs.push(dir.to_path_buf());
            cur = dir.parent();
        }
        dirs.reverse();

        let mut extra = String::new();
        for dir in dirs {
            if !self.seen_subdir_docs.insert(dir.clone()) {
                continue;
            }
            let doc_path = if dir.join("AGENTS.md").is_file() {
                Some(dir.join("AGENTS.md"))
            } else if dir.join("CLAUDE.md").is_file() {
                Some(dir.join("CLAUDE.md"))
            } else {
                None
            };
            let Some(doc) = doc_path else {
                continue;
            };
            let Ok(canon_doc) = doc.canonicalize() else {
                continue;
            };
            if !canon_doc.starts_with(root) || !canon_doc.is_file() {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&canon_doc) else {
                continue;
            };
            let trimmed = body.trim();
            if trimmed.is_empty() {
                continue;
            }
            let rel = doc
                .strip_prefix(root)
                .unwrap_or(&doc)
                .to_string_lossy()
                .into_owned();
            let clipped: String = trimmed.chars().take(MAX_SUBDIR_DOC_CHARS).collect();
            extra.push_str(&format!("\n\n[Subdirectory instructions: {rel}]\n{clipped}"));
        }
        extra
    }

    fn run_post_edit_check(&self) -> Option<String> {
        let cfg_path = self.worktree.join(".twig/project.json");
        let raw = std::fs::read_to_string(cfg_path).ok()?;
        let val: Value = serde_json::from_str(&raw).ok()?;
        let hook = val.get("post_edit_check")?;
        let check = if let Some(cmd) = hook.as_str() {
            let trimmed = cmd.trim();
            if trimmed.is_empty() {
                return None;
            }
            Check {
                command: trimmed.to_owned(),
                timeout_ms: 30_000,
                max_output_bytes: 16_384,
            }
        } else {
            serde_json::from_value::<Check>(hook.clone()).ok()?
        };
        let res = run_check(&self.worktree, &check).ok()?;
        if res.passed {
            None
        } else {
            Some(format!(
                "\n\n[post_edit_check failed (`{}`): exit {:?}]\n{}",
                check.command,
                res.exit_code,
                res.output.trim()
            ))
        }
    }

    /// Revert the most recent `edit`, `write`, or `apply_patch` file change in this session.
    pub fn undo_last_edit(&mut self) -> Result<String, String> {
        if self.is_read_only() {
            return Err("refused: read-only mode is active".into());
        }
        let snap = self
            .undo_stack
            .pop()
            .ok_or_else(|| "nothing to undo in this session".to_owned())?;
        match snap.previous {
            Some(prev) => {
                if let Some(parent) = snap.path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&snap.path, &prev)
                    .map_err(|e| format!("restore {}: {e}", snap.rel))?;
                self.record_file_stamp(&snap.path);
                Ok(format!("restored {} ({} bytes)", snap.rel, prev.len()))
            }
            None => {
                if snap.path.exists() {
                    std::fs::remove_file(&snap.path)
                        .map_err(|e| format!("remove {}: {e}", snap.rel))?;
                }
                self.read_stamps.remove(&snap.path);
                Ok(format!("removed newly created {}", snap.rel))
            }
        }
    }

    pub(super) fn push_undo(&mut self, path: PathBuf, rel: String, previous: Option<String>) {
        if self.undo_stack.len() >= MAX_UNDO_SNAPSHOTS {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(FileSnapshot {
            path,
            rel,
            previous,
        });
    }

    pub(super) fn resolve_existing_in_worktree(
        &self,
        path: &str,
    ) -> Result<(PathBuf, PathBuf), String> {
        resolve_existing_under(&self.worktree, path)
    }

    pub(super) fn resolve_writable_in_worktree(
        &self,
        path: &str,
    ) -> Result<(PathBuf, PathBuf), String> {
        super::super::git_ops::resolve_writable_under(&self.worktree, path)
    }

    pub(super) fn read_only_refusal(&self) -> String {
        if self.read_only_mask == PLAN_RO {
            "refused: plan mode is active, toolset is read-only".into()
        } else {
            "refused: guard is narrow, toolset is read-only".into()
        }
    }

    pub(super) fn read(
        &mut self,
        path: &str,
        start_line: Option<usize>,
        limit: Option<usize>,
    ) -> String {
        let (out, code, touched) = read_stateless(
            &self.worktree,
            self.artifacts_dir.as_deref(),
            self.read_max_lines,
            path,
            start_line,
            limit,
        );
        self.last_exit_code = code;
        if let Some((root, target)) = touched {
            self.record_file_stamp(&target);
            let extra = self.discover_subdir_instructions(&root, &target);
            if !extra.is_empty() {
                return format!("{out}{extra}");
            }
        }
        out
    }

    pub(super) fn edit(
        &mut self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> String {
        if self.is_read_only() {
            return self.read_only_refusal();
        }
        if old_string.is_empty() {
            return "refused: old_string must not be empty".into();
        }
        if old_string == new_string {
            return "refused: old_string and new_string are identical".into();
        }
        let (root, target) = match self.resolve_existing_in_worktree(path) {
            Ok(v) => v,
            Err(e) => return e,
        };
        if let Err(msg) = self.check_read_guard(&target, path) {
            return msg;
        }
        let before = match std::fs::read_to_string(&target) {
            Ok(s) => s,
            Err(e) => return format!("{path}: {e}"),
        };
        let after = match apply_progressive_edit(&before, old_string, new_string, replace_all, path)
        {
            Ok(s) => s,
            Err(msg) => return msg,
        };
        let rel = target
            .strip_prefix(&root)
            .unwrap_or(&target)
            .to_string_lossy()
            .into_owned();
        if let Err(e) = std::fs::write(&target, &after) {
            return format!("{path}: {e}");
        }
        self.record_file_stamp(&target);
        let subdir_extra = self.discover_subdir_instructions(&root, &target);
        self.push_undo(target, rel.clone(), Some(before.clone()));
        self.last_exit_code = Some(0);
        let mut out = format_unified_diff(&rel, &before, &after);
        if let Some(diag) = self.run_post_edit_check() {
            out.push_str(&diag);
        }
        out.push_str(&subdir_extra);
        out
    }

    pub(super) fn write_file(&mut self, path: &str, content: &str) -> String {
        if self.is_read_only() {
            return self.read_only_refusal();
        }
        let (root, target) = match self.resolve_writable_in_worktree(path) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let previous = if target.exists() {
            if let Err(msg) = self.check_read_guard(&target, path) {
                return msg;
            }
            std::fs::read_to_string(&target).ok()
        } else {
            None
        };
        if let Some(parent) = target.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("create parent dirs for {path}: {e}");
            }
        }
        if let Err(e) = std::fs::write(&target, content) {
            return format!("{path}: {e}");
        }
        self.record_file_stamp(&target);
        let subdir_extra = self.discover_subdir_instructions(&root, &target);
        let rel = target
            .strip_prefix(&root)
            .unwrap_or(&target)
            .to_string_lossy()
            .into_owned();
        let mut diff = match &previous {
            Some(prev) => format_unified_diff(&rel, prev, content),
            None => format!(
                "created {rel} ({} lines, {} bytes)",
                content.lines().count(),
                content.len()
            ),
        };
        self.push_undo(target, rel, previous);
        self.last_exit_code = Some(0);
        if let Some(diag) = self.run_post_edit_check() {
            diff.push_str(&diag);
        }
        diff.push_str(&subdir_extra);
        diff
    }

    pub(super) fn apply_patch(&mut self, patch_text: &str) -> String {
        if self.is_read_only() {
            return self.read_only_refusal();
        }
        let ops = match parse_patch_ops(patch_text) {
            Ok(v) if v.is_empty() => return "apply_patch error: patch contains no file operations".into(),
            Ok(v) => v,
            Err(e) => return format!("apply_patch error: {e}"),
        };

        let mut prepared: Vec<(PathBuf, PreparedPatch)> = Vec::with_capacity(ops.len());
        for op in ops {
            match op {
                PatchOp::Add { path, content } => {
                    let (root, target) = match self.resolve_writable_in_worktree(&path) {
                        Ok(v) => v,
                        Err(e) => return e,
                    };
                    let previous = if target.exists() {
                        if let Err(msg) = self.check_read_guard(&target, &path) {
                            return msg;
                        }
                        std::fs::read_to_string(&target).ok()
                    } else {
                        None
                    };
                    let rel = target
                        .strip_prefix(&root)
                        .unwrap_or(&target)
                        .to_string_lossy()
                        .into_owned();
                    let summary = match &previous {
                        Some(prev) => format_unified_diff(&rel, prev, &content),
                        None => format!(
                            "created {rel} ({} lines, {} bytes)",
                            content.lines().count(),
                            content.len()
                        ),
                    };
                    prepared.push((
                        root,
                        PreparedPatch {
                            target,
                            rel,
                            previous,
                            after: Some(content),
                            summary,
                        },
                    ));
                }
                PatchOp::Delete { path } => {
                    let (root, target) = match self.resolve_existing_in_worktree(&path) {
                        Ok(v) => v,
                        Err(e) => return e,
                    };
                    if let Err(msg) = self.check_read_guard(&target, &path) {
                        return msg;
                    }
                    let previous = std::fs::read_to_string(&target).ok();
                    let rel = target
                        .strip_prefix(&root)
                        .unwrap_or(&target)
                        .to_string_lossy()
                        .into_owned();
                    let summary = format!("deleted {rel}");
                    prepared.push((
                        root,
                        PreparedPatch {
                            target,
                            rel,
                            previous,
                            after: None,
                            summary,
                        },
                    ));
                }
                PatchOp::Update { path, hunks } => {
                    let (root, target) = match self.resolve_existing_in_worktree(&path) {
                        Ok(v) => v,
                        Err(e) => return e,
                    };
                    if let Err(msg) = self.check_read_guard(&target, &path) {
                        return msg;
                    }
                    let before = match std::fs::read_to_string(&target) {
                        Ok(s) => s,
                        Err(e) => return format!("{path}: {e}"),
                    };
                    let after = match apply_update_hunks(&before, &hunks, &path) {
                        Ok(s) => s,
                        Err(e) => return format!("apply_patch error in {path}: {e}"),
                    };
                    let rel = target
                        .strip_prefix(&root)
                        .unwrap_or(&target)
                        .to_string_lossy()
                        .into_owned();
                    let summary = format_unified_diff(&rel, &before, &after);
                    prepared.push((
                        root,
                        PreparedPatch {
                            target,
                            rel,
                            previous: Some(before),
                            after: Some(after),
                            summary,
                        },
                    ));
                }
            }
        }

        let mut summaries = Vec::with_capacity(prepared.len());
        let mut subdir_notes = String::new();
        for (root, item) in prepared {
            match item.after {
                Some(content) => {
                    if let Some(parent) = item.target.parent() {
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            return format!("create parent dirs for {}: {e}", item.rel);
                        }
                    }
                    if let Err(e) = std::fs::write(&item.target, &content) {
                        return format!("{}: {e}", item.rel);
                    }
                    self.record_file_stamp(&item.target);
                    subdir_notes.push_str(&self.discover_subdir_instructions(&root, &item.target));
                }
                None => {
                    if let Err(e) = std::fs::remove_file(&item.target) {
                        return format!("delete {}: {e}", item.rel);
                    }
                    self.read_stamps.remove(&item.target);
                }
            }
            self.push_undo(item.target, item.rel, item.previous);
            summaries.push(item.summary);
        }

        self.last_exit_code = Some(0);
        let mut out = summaries.join("\n\n");
        if let Some(diag) = self.run_post_edit_check() {
            out.push_str(&diag);
        }
        out.push_str(&subdir_notes);
        out
    }

    pub(super) fn grep(
        &mut self,
        pattern: &str,
        subpath: Option<&str>,
        file_glob: Option<&str>,
        ignore_case: bool,
    ) -> String {
        let (out, code) = grep_stateless(&self.worktree, pattern, subpath, file_glob, ignore_case);
        self.last_exit_code = code;
        out
    }

    pub(super) fn glob(&mut self, pattern: &str, subpath: Option<&str>) -> String {
        let (out, code) = glob_stateless(&self.worktree, pattern, subpath);
        self.last_exit_code = code;
        out
    }
}

pub(super) fn resolve_existing_under(
    worktree: &Path,
    path: &str,
) -> Result<(PathBuf, PathBuf), String> {
    let root = worktree
        .canonicalize()
        .map_err(|e| format!("worktree: {e}"))?;
    let expanded = if path == "~" || path.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            let h = PathBuf::from(home);
            if let Some(rel) = path.strip_prefix("~/") {
                h.join(rel)
            } else {
                h
            }
        } else {
            root.join(path)
        }
    } else {
        root.join(path)
    };
    let target = expanded
        .canonicalize()
        .map_err(|e| format!("{path}: {e}"))?;
    if !target.starts_with(&root) {
        return Err(format!("refused: {path} resolves outside the worktree"));
    }
    Ok((root, target))
}

pub(super) fn read_stateless(
    worktree: &Path,
    artifacts_dir: Option<&Path>,
    read_max_lines: usize,
    path: &str,
    start_line: Option<usize>,
    limit: Option<usize>,
) -> (String, Option<i32>, Option<(PathBuf, PathBuf)>) {
    if let Some(sid) = path.strip_prefix("session:") {
        match super::super::session::format_session_transcript(sid) {
            Ok(text) => {
                return (
                    paginate_text(&text, start_line, limit, read_max_lines, false),
                    Some(0),
                    None,
                );
            }
            Err(e) => return (format!("session:{sid}: {e}"), None, None),
        }
    }
    if let Some(fname) = path.strip_prefix("artifact:") {
        let Some(dir) = artifacts_dir else {
            return ("artifact storage is not configured".into(), None, None);
        };
        if fname.is_empty()
            || fname.contains('/')
            || fname.contains('\\')
            || fname.contains("..")
        {
            return (format!("refused: invalid artifact name {fname:?}"), None, None);
        }
        let Ok(root) = dir.canonicalize() else {
            return (format!("artifact:{fname}: not found"), None, None);
        };
        let Ok(real) = root.join(fname).canonicalize() else {
            return (format!("artifact:{fname}: not found"), None, None);
        };
        if !real.starts_with(&root) {
            return (format!("refused: {path} resolves outside artifacts"), None, None);
        }
        return match std::fs::read_to_string(&real) {
            Ok(text) => (
                paginate_text(&text, start_line, limit, read_max_lines, false),
                Some(0),
                None,
            ),
            Err(e) => (format!("{path}: {e}"), None, None),
        };
    }

    let (root, target) = match resolve_existing_under(worktree, path) {
        Ok(v) => v,
        Err(e) => return (e, None, None),
    };
    let bytes = match std::fs::read(&target) {
        Ok(b) => b,
        Err(e) => return (format!("{path}: {e}"), None, None),
    };
    if bytes.iter().take(512).any(|&b| b == 0) {
        return (
            super::inspect::format_binary(&bytes, start_line, limit, read_max_lines),
            Some(0),
            Some((root, target)),
        );
    }
    match String::from_utf8(bytes) {
        Ok(text) => {
            let is_har = target
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("har"));
            if is_har && start_line.is_none() {
                if let Some(distilled) = super::inspect::distill_har(&text) {
                    return (distilled, Some(0), Some((root, target)));
                }
            }
            (
                paginate_text(&text, start_line, limit, read_max_lines, true),
                Some(0),
                Some((root, target)),
            )
        }
        Err(err) => (
            super::inspect::format_binary(err.as_bytes(), start_line, limit, read_max_lines),
            Some(0),
            Some((root, target)),
        ),
    }
}

fn paginate_text(
    text: &str,
    start_line: Option<usize>,
    limit: Option<usize>,
    read_max_lines: usize,
    number_lines: bool,
) -> String {
    let skip = start_line.unwrap_or(1).saturating_sub(1);
    let take = limit.unwrap_or(read_max_lines).clamp(1, read_max_lines);
    let mut lines = text.lines().skip(skip);
    let mut result: String = lines
        .by_ref()
        .take(take)
        .enumerate()
        .map(|(i, line)| {
            if number_lines {
                format!("{:>4}→{line}", skip + i + 1)
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if lines.next().is_some() {
        let next_line = skip + take + 1;
        result.push_str(&format!(
            "\n[truncated after {take} lines; pass start_line={next_line} to continue]"
        ));
    }
    if result.is_empty() {
        "(empty file)".into()
    } else {
        result
    }
}

/// 4-level progressive `seek_sequence` matching:
/// 1. Exact byte match
/// 2. `\r\n` -> `\n` line-ending normalized match
/// 3. Right-trimmed line-sequence match
/// 4. Relative indentation-shifted block match (re-indenting `new_string` by the delta)
fn apply_progressive_edit(
    before: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    path: &str,
) -> Result<String, String> {
    let exact: Vec<usize> = before.match_indices(old_string).map(|(i, _)| i).collect();
    if exact.len() == 1 || (exact.len() > 1 && replace_all) {
        return Ok(if replace_all {
            before.replace(old_string, new_string)
        } else {
            before.replacen(old_string, new_string, 1)
        });
    }
    if exact.len() > 1 {
        let line_nums: Vec<String> = exact
            .iter()
            .take(8)
            .map(|&idx| (before[..idx].bytes().filter(|&b| b == b'\n').count() + 1).to_string())
            .collect();
        return Err(format!(
            "edit failed: old_string matched {} times in {path} (lines {}); include more context or set replace_all=true",
            exact.len(),
            line_nums.join(", ")
        ));
    }

    // Level 2: CRLF <-> LF normalization
    let norm_before = before.replace("\r\n", "\n");
    let norm_old = old_string.replace("\r\n", "\n");
    let norm_new = new_string.replace("\r\n", "\n");
    let norm_hits: Vec<usize> = norm_before
        .match_indices(&norm_old)
        .map(|(i, _)| i)
        .collect();
    if norm_hits.len() == 1 || (norm_hits.len() > 1 && replace_all) {
        let replaced = if replace_all {
            norm_before.replace(&norm_old, &norm_new)
        } else {
            norm_before.replacen(&norm_old, &norm_new, 1)
        };
        return Ok(if before.contains("\r\n") {
            replaced.replace('\n', "\r\n")
        } else {
            replaced
        });
    }
    if norm_hits.len() > 1 {
        return Err(format!(
            "edit failed: old_string matched {} times in {path}; include more context or set replace_all=true",
            norm_hits.len()
        ));
    }

    let trailing_nl = norm_before.ends_with('\n');
    let file_lines: Vec<&str> = norm_before.lines().collect();
    let old_lines: Vec<&str> = norm_old.lines().collect();
    let new_lines: Vec<&str> = norm_new.lines().collect();
    if old_lines.is_empty() || old_lines.len() > file_lines.len() {
        return Err(format!("edit failed: old_string not found in {path}"));
    }

    // Level 3: Right-trimmed line sequence match
    let mut rtrim_hits = Vec::new();
    for start in 0..=(file_lines.len() - old_lines.len()) {
        if file_lines[start..start + old_lines.len()]
            .iter()
            .zip(&old_lines)
            .all(|(a, b)| a.trim_end() == b.trim_end())
        {
            rtrim_hits.push(start);
        }
    }
    if rtrim_hits.len() == 1 {
        let start = rtrim_hits[0];
        let mut out_lines: Vec<String> = file_lines[..start]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        out_lines.extend(new_lines.iter().map(|s| (*s).to_owned()));
        out_lines.extend(
            file_lines[start + old_lines.len()..]
                .iter()
                .map(|s| (*s).to_owned()),
        );
        return Ok(join_lines_preserving_newline(&out_lines, trailing_nl, before.contains("\r\n")));
    }
    if rtrim_hits.len() > 1 {
        return Err(format!(
            "edit failed: old_string matched {} locations (ignoring trailing whitespace) in {path}; include more context",
            rtrim_hits.len()
        ));
    }

    // Level 4: Relative indentation-shifted block match
    let old_min_indent = min_space_indent(&old_lines);
    let mut indent_hits: Vec<(usize, isize)> = Vec::new();
    for start in 0..=(file_lines.len() - old_lines.len()) {
        let win = &file_lines[start..start + old_lines.len()];
        let win_min = min_space_indent(win);
        let delta = win_min as isize - old_min_indent as isize;
        let matches = win.iter().zip(&old_lines).all(|(a, b)| {
            if a.trim().is_empty() && b.trim().is_empty() {
                true
            } else {
                let sa = a.get(win_min..).unwrap_or("").trim_end();
                let sb = b.get(old_min_indent..).unwrap_or("").trim_end();
                sa == sb
            }
        });
        if matches {
            indent_hits.push((start, delta));
        }
    }
    if indent_hits.len() == 1 {
        let (start, delta) = indent_hits[0];
        let mut out_lines: Vec<String> = file_lines[..start]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        for line in &new_lines {
            if line.trim().is_empty() {
                out_lines.push(String::new());
            } else {
                let cur_spaces = line.bytes().take_while(|&b| b == b' ').count() as isize;
                let target_spaces = (cur_spaces + delta).max(0) as usize;
                let stripped = line.trim_start_matches(' ');
                out_lines.push(format!("{}{stripped}", " ".repeat(target_spaces)));
            }
        }
        out_lines.extend(
            file_lines[start + old_lines.len()..]
                .iter()
                .map(|s| (*s).to_owned()),
        );
        return Ok(join_lines_preserving_newline(&out_lines, trailing_nl, before.contains("\r\n")));
    }
    if indent_hits.len() > 1 {
        return Err(format!(
            "edit failed: old_string matched {} locations (ignoring indentation shift) in {path}; include more context",
            indent_hits.len()
        ));
    }

    Err(format!("edit failed: old_string not found in {path}"))
}

fn min_space_indent(lines: &[&str]) -> usize {
    lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.bytes().take_while(|&b| b == b' ').count())
        .min()
        .unwrap_or(0)
}

fn join_lines_preserving_newline(lines: &[String], trailing_nl: bool, crlf: bool) -> String {
    let sep = if crlf { "\r\n" } else { "\n" };
    let mut out = lines.join(sep);
    if trailing_nl && !out.is_empty() {
        out.push_str(sep);
    }
    out
}

fn parse_patch_ops(raw: &str) -> Result<Vec<PatchOp>, String> {
    let norm = raw.replace("\r\n", "\n");
    let lines: Vec<&str> = norm.lines().collect();
    let mut i = 0usize;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i < lines.len() && lines[i].trim() == "*** Begin Patch" {
        i += 1;
    }
    let mut ops = Vec::new();
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if trimmed == "*** End Patch" {
            break;
        }
        if let Some(path) = trimmed.strip_prefix("*** Add File:") {
            let path = path.trim().to_owned();
            i += 1;
            let mut content_lines = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let l = lines[i];
                content_lines.push(l.strip_prefix('+').unwrap_or(l));
                i += 1;
            }
            let mut content = content_lines.join("\n");
            if !content.is_empty() {
                content.push('\n');
            }
            ops.push(PatchOp::Add { path, content });
        } else if let Some(path) = trimmed.strip_prefix("*** Delete File:") {
            ops.push(PatchOp::Delete {
                path: path.trim().to_owned(),
            });
            i += 1;
        } else if let Some(path) = trimmed.strip_prefix("*** Update File:") {
            let path = path.trim().to_owned();
            i += 1;
            if i < lines.len() && lines[i].starts_with("*** Move to:") {
                return Err("`*** Move to:` is not supported; use Delete + Add or bash git mv".into());
            }
            let mut hunks: Vec<Vec<String>> = Vec::new();
            let mut cur_hunk: Vec<String> = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let l = lines[i];
                if l.starts_with("@@") {
                    if !cur_hunk.is_empty() {
                        hunks.push(std::mem::take(&mut cur_hunk));
                    }
                } else if l.starts_with('+') || l.starts_with('-') || l.starts_with(' ') {
                    cur_hunk.push(l.to_owned());
                } else if l.is_empty() {
                    cur_hunk.push(" ".into());
                } else {
                    return Err(format!(
                        "invalid hunk line in {path}: expected ' ', '+', '-', or '@@', got {l:?}"
                    ));
                }
                i += 1;
            }
            if !cur_hunk.is_empty() {
                hunks.push(cur_hunk);
            }
            if hunks.is_empty() {
                return Err(format!("Update File {path} has no hunks"));
            }
            ops.push(PatchOp::Update { path, hunks });
        } else {
            return Err(format!("unexpected patch header {trimmed:?}"));
        }
    }
    Ok(ops)
}

fn apply_update_hunks(before: &str, hunks: &[Vec<String>], path: &str) -> Result<String, String> {
    let crlf = before.contains("\r\n");
    let norm = before.replace("\r\n", "\n");
    let trailing_nl = norm.ends_with('\n');
    let mut lines: Vec<String> = norm.lines().map(str::to_owned).collect();
    let mut cursor = 0usize;

    for (h_idx, hunk) in hunks.iter().enumerate() {
        let mut expected: Vec<&str> = Vec::new();
        let mut replacement: Vec<String> = Vec::new();
        for hl in hunk {
            let (tag, body) = hl.split_at(1);
            match tag {
                " " => {
                    expected.push(body);
                    replacement.push(body.to_owned());
                }
                "-" => expected.push(body),
                "+" => replacement.push(body.to_owned()),
                _ => {}
            }
        }
        if expected.is_empty() {
            let pos = cursor.min(lines.len());
            let new_len = replacement.len();
            lines.splice(pos..pos, replacement);
            cursor = pos + new_len;
            continue;
        }
        let found = find_hunk_position(&lines, &expected, cursor)
            .or_else(|| find_hunk_position(&lines, &expected, 0))
            .ok_or_else(|| format!("hunk #{} context not found in {path}", h_idx + 1))?;
        let old_len = expected.len();
        let new_len = replacement.len();
        lines.splice(found..found + old_len, replacement);
        cursor = found + new_len;
    }

    Ok(join_lines_preserving_newline(&lines, trailing_nl || !lines.is_empty(), crlf))
}

fn find_hunk_position(lines: &[String], expected: &[&str], from: usize) -> Option<usize> {
    if expected.len() > lines.len() || from > lines.len() - expected.len() {
        return None;
    }
    // Pass 1: exact match from `from`
    for pos in from..=(lines.len() - expected.len()) {
        if lines[pos..pos + expected.len()]
            .iter()
            .zip(expected)
            .all(|(a, b)| a == b)
        {
            return Some(pos);
        }
    }
    // Pass 2: right-trimmed match from `from`
    for pos in from..=(lines.len() - expected.len()) {
        if lines[pos..pos + expected.len()]
            .iter()
            .zip(expected)
            .all(|(a, b)| a.trim_end() == b.trim_end())
        {
            return Some(pos);
        }
    }
    // Pass 3: trimmed match from `from`
    for pos in from..=(lines.len() - expected.len()) {
        if lines[pos..pos + expected.len()]
            .iter()
            .zip(expected)
            .all(|(a, b)| a.trim() == b.trim())
        {
            return Some(pos);
        }
    }
    None
}

pub(super) fn grep_stateless(
    worktree: &Path,
    pattern: &str,
    subpath: Option<&str>,
    file_glob: Option<&str>,
    ignore_case: bool,
) -> (String, Option<i32>) {
    if pattern.is_empty() {
        return ("refused: empty grep pattern".into(), None);
    }
    let (root, base) = match resolve_existing_under(worktree, subpath.unwrap_or(".")) {
        Ok(v) => v,
        Err(e) => return (e, None),
    };
    let needle = if ignore_case {
        pattern.to_lowercase()
    } else {
        pattern.to_owned()
    };
    let mut files = Vec::new();
    collect_files(&base, &root, &mut files);
    files.sort();

    let mut matches = Vec::new();
    let mut truncated = false;
    let is_dir_walk = base.is_dir();
    if !is_dir_walk && super::super::policy::is_sensitive_secret_path(subpath.unwrap_or("")) {
        return (
            "refused: grep on sensitive secret file is blocked; use read(path=...) which requires human approval".into(),
            Some(1),
        );
    }
    for (rel, abs) in files {
        if super::super::policy::is_sensitive_secret_path(&rel) {
            if !is_dir_walk {
                return (
                    "refused: grep on sensitive secret file is blocked; use read(path=...) which requires human approval".into(),
                    Some(1),
                );
            }
            continue;
        }
        if let Some(g) = file_glob {
            if !glob_match(g, &rel)
                && abs
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_none_or(|n| !glob_match(g, n))
            {
                continue;
            }
        }
        let Ok(meta) = std::fs::metadata(&abs) else {
            continue;
        };
        if meta.len() > MAX_GREP_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        if bytes.iter().take(512).any(|&b| b == 0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        for (idx, line) in text.lines().enumerate() {
            let hit = if ignore_case {
                line.to_lowercase().contains(&needle)
            } else {
                line.contains(&needle)
            };
            if hit {
                matches.push(format!("{rel}:{}: {line}", idx + 1));
                if matches.len() >= MAX_GREP_MATCHES {
                    truncated = true;
                    break;
                }
            }
        }
        if truncated {
            break;
        }
    }
    if matches.is_empty() {
        return (format!("no matches for {pattern:?}"), Some(0));
    }
    if truncated {
        matches.push(format!(
            "[capped at {MAX_GREP_MATCHES} matches; narrow path or glob]"
        ));
    }
    (matches.join("\n"), Some(0))
}

pub(super) fn glob_stateless(
    worktree: &Path,
    pattern: &str,
    subpath: Option<&str>,
) -> (String, Option<i32>) {
    if pattern.trim().is_empty() {
        return ("refused: empty glob pattern".into(), None);
    }
    let (root, base) = match resolve_existing_under(worktree, subpath.unwrap_or(".")) {
        Ok(v) => v,
        Err(e) => return (e, None),
    };
    let mut files = Vec::new();
    collect_files(&base, &root, &mut files);
    let mut matched: Vec<String> = files
        .into_iter()
        .filter_map(|(rel, _)| {
            let target = if let Some(sub) = subpath {
                let prefix = format!("{}/", sub.trim_end_matches('/'));
                rel.strip_prefix(&prefix).unwrap_or(&rel).to_owned()
            } else {
                rel.clone()
            };
            (glob_match(pattern, &target) || glob_match(pattern, &rel)).then_some(rel)
        })
        .collect();
    matched.sort();
    if matched.is_empty() {
        return (format!("no files matched {pattern:?}"), Some(0));
    }
    let truncated = matched.len() > MAX_GLOB_MATCHES;
    matched.truncate(MAX_GLOB_MATCHES);
    if truncated {
        matched.push(format!("[capped at {MAX_GLOB_MATCHES} files]"));
    }
    (matched.join("\n"), Some(0))
}

pub(super) fn collect_files(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
    if dir.is_file() {
        if let Ok(rel) = dir.strip_prefix(root) {
            out.push((rel.to_string_lossy().into_owned(), dir.to_path_buf()));
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            let name = entry.file_name();
            if name.to_str().is_some_and(|n| IGNORED_DIRS.contains(&n)) {
                continue;
            }
            collect_files(&path, root, out);
        } else if ft.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push((rel.to_string_lossy().into_owned(), path));
            }
        }
    }
}

/// Pure-Rust glob matcher supporting `*`, `**`, and `?` over `/` path segments.
pub(super) fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut memo = vec![vec![None; t.len() + 1]; p.len() + 1];
    fn go(p: &[char], t: &[char], pi: usize, ti: usize, memo: &mut [Vec<Option<bool>>]) -> bool {
        if let Some(hit) = memo[pi][ti] {
            return hit;
        }
        let res = if pi == p.len() {
            ti == t.len()
        } else if p[pi] == '*' {
            if pi + 1 < p.len() && p[pi + 1] == '*' {
                let next_pi = if pi + 2 < p.len() && p[pi + 2] == '/' {
                    pi + 3
                } else {
                    pi + 2
                };
                go(p, t, next_pi, ti, memo) || (ti < t.len() && go(p, t, pi, ti + 1, memo))
            } else {
                go(p, t, pi + 1, ti, memo)
                    || (ti < t.len() && t[ti] != '/' && go(p, t, pi, ti + 1, memo))
            }
        } else if ti < t.len() && (p[pi] == '?' && t[ti] != '/' || p[pi] == t[ti]) {
            go(p, t, pi + 1, ti + 1, memo)
        } else {
            false
        };
        memo[pi][ti] = Some(res);
        res
    }
    go(&p, &t, 0, 0, &mut memo)
}

/// Compact unified diff with 2 lines of surrounding context for `edit`, `write`, and `apply_patch`.
pub(super) fn format_unified_diff(rel: &str, before: &str, after: &str) -> String {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (old.len() - prefix)
        && suffix < (new.len() - prefix)
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    if prefix == old.len() && prefix == new.len() {
        return format!("no changes in {rel}");
    }
    let ctx_start = prefix.saturating_sub(2);
    let old_end = (old.len() - suffix + 2).min(old.len());
    let new_end = (new.len() - suffix + 2).min(new.len());
    let old_count = old_end - ctx_start;
    let new_count = new_end - ctx_start;

    let mut out = format!(
        "--- a/{rel}\n+++ b/{rel}\n@@ -{},{} +{},{} @@\n",
        ctx_start + 1,
        old_count,
        ctx_start + 1,
        new_count
    );
    for line in &old[ctx_start..prefix] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in &old[prefix..old.len() - suffix] {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &new[prefix..new.len() - suffix] {
        out.push_str(&format!("+{line}\n"));
    }
    for line in &old[old.len() - suffix..old_end] {
        out.push_str(&format!(" {line}\n"));
    }
    out.trim_end().to_owned()
}
