//! T3-style Git workflow primitives: live mid-session worktree handoff (`/worktree`),
//! non-intrusive `GIT_INDEX_FILE` turn snapshots, confined patch application,
//! zero-trailer one-click Pull Request shipping (`/pr`), AI trailer hygiene, and
//! host keep-awake inhibition during long-running agent turns.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::policy::is_sensitive_secret_path;
use crate::exec::run_process;
use crate::llm::Llm;

/// Strip AI-generated `Co-Authored-By` / promotional trailers while preserving human co-authors.
pub(super) fn strip_ai_coauthor_trailers(msg: &str) -> String {
    const AI_MARKERS: &[&str] = &[
        "claude",
        "anthropic",
        "noreply@anthropic.com",
        "codex",
        "openai",
        "chatgpt",
        "gemini",
        "cursor",
        "copilot",
        "twig",
        "generated with [",
        "🤖 generated",
    ];
    let mut kept: Vec<&str> = Vec::new();
    for line in msg.lines() {
        let low = line.trim().to_ascii_lowercase();
        let is_ai_coauthor = low.starts_with("co-authored-by:")
            && AI_MARKERS.iter().any(|m| low.contains(m));
        let is_ai_badge = low.starts_with("generated with [") || low.starts_with("🤖 generated");
        if !is_ai_coauthor && !is_ai_badge {
            kept.push(line);
        }
    }
    kept.join("\n").trim().to_owned()
}

/// Prevent host idle/suspend while an active agent turn is running (`systemd-inhibit` on Linux).
pub(super) struct KeepAwakeGuard {
    child: Option<Child>,
}

impl KeepAwakeGuard {
    pub fn acquire() -> Self {
        if cfg!(test) || !cfg!(target_os = "linux") {
            return Self { child: None };
        }
        let child = Command::new("systemd-inhibit")
            .args([
                "--what=idle:sleep",
                "--who=twig",
                "--why=Twig agent turn active",
                "--mode=block",
                "sleep",
                "3600",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        Self { child }
    }
}

impl Drop for KeepAwakeGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn sanitize_slug(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(['-', '/'])
        .to_owned()
}

/// Copy tracked uncommitted diffs and non-ignored untracked files (`<= 4 MiB`, non-symlink,
/// non-secret) from `src_wt` into `dst_wt`, returning `true` if `dst_wt` received any changes.
pub(super) fn carry_uncommitted_state(src_wt: &Path, dst_wt: &Path) -> bool {
    let mut dirty = false;
    if let Ok(diff_out) = Command::new("git")
        .arg("-C")
        .arg(src_wt)
        .args(["diff", "HEAD", "--binary"])
        .output()
    {
        if diff_out.status.success() && !diff_out.stdout.is_empty() {
            if let Ok(mut child) = Command::new("git")
                .arg("-C")
                .arg(dst_wt)
                .args(["apply", "--whitespace=nowarn", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&diff_out.stdout);
                }
                if child.wait().is_ok_and(|s| s.success()) {
                    dirty = true;
                }
            }
        }
    }
    if let Ok(untracked) = Command::new("git")
        .arg("-C")
        .arg(src_wt)
        .args(["ls-files", "-z", "--others", "--exclude-standard"])
        .output()
    {
        if untracked.status.success() {
            for rel in String::from_utf8_lossy(&untracked.stdout)
                .split('\0')
                .filter(|s| !s.is_empty())
            {
                if is_sensitive_secret_path(rel) {
                    continue;
                }
                let src = src_wt.join(rel);
                if let Ok(meta) = std::fs::symlink_metadata(&src) {
                    if !meta.file_type().is_symlink()
                        && meta.is_file()
                        && meta.len() <= 4 * 1024 * 1024
                    {
                        let dst = dst_wt.join(rel);
                        if let Some(p) = dst.parent() {
                            let _ = std::fs::create_dir_all(p);
                        }
                        if std::fs::copy(&src, &dst).is_ok() {
                            dirty = true;
                        }
                    }
                }
            }
        }
    }
    dirty
}

/// Resolve a relative writable path strictly inside `worktree` (rejecting absolute paths,
/// `..` traversal, and ancestor symlinks escaping the canonical worktree root).
pub(crate) fn resolve_writable_under(
    worktree: &Path,
    path: &str,
) -> Result<(PathBuf, PathBuf), String> {
    if path.trim().is_empty() {
        return Err("refused: empty path".into());
    }
    let rel = Path::new(path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("refused: {path} must stay inside the worktree"));
    }
    let root = worktree
        .canonicalize()
        .map_err(|e| format!("worktree: {e}"))?;
    let candidate = root.join(rel);
    let mut ancestor = candidate.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| format!("refused: invalid path {path}"))?;
    }
    let canon_ancestor = ancestor
        .canonicalize()
        .map_err(|e| format!("{path}: {e}"))?;
    if !canon_ancestor.starts_with(&root) {
        return Err(format!("refused: {path} resolves outside the worktree"));
    }
    let tail = candidate
        .strip_prefix(ancestor)
        .map_err(|_| format!("refused: {path} resolves outside the worktree"))?;
    Ok((root, canon_ancestor.join(tail)))
}

/// Apply a unified git diff inside `worktree` after blocking mode `120000` symlinks and
/// verifying every touched path stays strictly confined inside `worktree`.
/// Returns `(rel, target_abs, previous_content)` for undo/stamp bookkeeping.
pub(crate) fn apply_confined_git_patch(
    worktree: &Path,
    patch_text: &str,
    label: &str,
) -> Result<Vec<(String, PathBuf, Option<String>)>, String> {
    if patch_text.trim().is_empty() {
        return Err(format!("no diff changes found for {label}"));
    }
    if patch_text.lines().any(|l| {
        l.starts_with("new file mode 120000")
            || l.starts_with("old mode 120000")
            || l.starts_with("new mode 120000")
    }) {
        return Err("refused: symlink git patches (mode 120000) are not permitted".into());
    }
    let mut touched_rels: Vec<String> = Vec::new();
    for line in patch_text.lines() {
        if let Some(p) = line
            .strip_prefix("+++ b/")
            .or_else(|| line.strip_prefix("--- a/"))
        {
            let trimmed = p.trim();
            if trimmed != "/dev/null"
                && !trimmed.is_empty()
                && !touched_rels.iter().any(|x| x == trimmed)
            {
                touched_rels.push(trimmed.to_owned());
            }
        }
    }
    let mut targets = Vec::with_capacity(touched_rels.len());
    for rel in &touched_rels {
        let (_, target) = resolve_writable_under(worktree, rel)?;
        let prev = target
            .exists()
            .then(|| std::fs::read_to_string(&target).ok())
            .flatten();
        targets.push((rel.clone(), target, prev));
    }

    let tmp = std::env::temp_dir().join(format!(
        "twig-apply-{}-{}.patch",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::write(&tmp, patch_text).map_err(|e| e.to_string())?;
    let check_out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["apply", "--check"])
        .arg(&tmp)
        .output()
        .map_err(|e| format!("git apply --check: {e}"))?;
    if !check_out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "patch {label} does not apply cleanly:\n{}",
            String::from_utf8_lossy(&check_out.stderr).trim()
        ));
    }
    let apply_out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("apply")
        .arg(&tmp)
        .output()
        .map_err(|e| format!("git apply: {e}"))?;
    let _ = std::fs::remove_file(&tmp);
    if !apply_out.status.success() {
        return Err(format!(
            "git apply failed:\n{}",
            String::from_utf8_lossy(&apply_out.stderr).trim()
        ));
    }
    Ok(targets)
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
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&tmp_idx);
    let _ = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["read-tree", "HEAD"])
        .env("GIT_INDEX_FILE", &tmp_idx)
        .output();
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
            .duration_since(UNIX_EPOCH)
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
    let snap_files_out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["ls-files", "-z"])
        .env("GIT_INDEX_FILE", &tmp_idx)
        .output()
        .map_err(|e| format!("git ls-files: {e}"))?;
    if !snap_files_out.status.success() {
        cleanup(&tmp_idx);
        return Err(format!(
            "git ls-files snapshot failed: {}",
            String::from_utf8_lossy(&snap_files_out.stderr).trim()
        ));
    }
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
    if !cur_files_out.status.success() {
        cleanup(&tmp_idx);
        return Err(format!(
            "git ls-files current failed: {}",
            String::from_utf8_lossy(&cur_files_out.stderr).trim()
        ));
    }
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

fn primary_worktree_path(repo: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .map(|s| PathBuf::from(s.trim()))
}

/// Live mid-session worktree handoff (`/worktree [slug|list|back]`):
/// creates an isolated sibling Git worktree, carries uncommitted edits across,
/// runs optional `.twig/worktree_setup.sh`, and returns the new path + branch.
pub(super) fn handoff_worktree(current_wt: &Path, raw_arg: Option<&str>) -> Result<Value, String> {
    let arg = raw_arg.unwrap_or("").trim();
    if arg.is_empty() || arg == "list" {
        let out = Command::new("git")
            .arg("-C")
            .arg(current_wt)
            .args(["worktree", "list"])
            .output()
            .map_err(|e| format!("git worktree list: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        return Ok(json!({
            "action": "list",
            "report": String::from_utf8_lossy(&out.stdout).trim_end(),
        }));
    }

    if matches!(arg, "back" | "main" | "primary") {
        let primary = primary_worktree_path(current_wt)
            .ok_or("could not determine primary git worktree")?;
        let canon_cur = current_wt
            .canonicalize()
            .unwrap_or_else(|_| current_wt.to_path_buf());
        let canon_pri = primary
            .canonicalize()
            .unwrap_or_else(|_| primary.clone());
        if canon_cur == canon_pri {
            return Err("already in the primary checkout".into());
        }
        carry_uncommitted_state(&canon_cur, &canon_pri);
        let branch = Command::new("git")
            .arg("-C")
            .arg(&canon_pri)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_else(|| "main".into());
        return Ok(json!({
            "action": "switched",
            "worktree": canon_pri.display().to_string(),
            "branch": branch,
            "summary": format!("handed session back to primary checkout {} ({branch})", canon_pri.display()),
        }));
    }

    let slug = sanitize_slug(arg);
    if slug.is_empty() {
        return Err("invalid worktree branch slug".into());
    }
    let branch = if slug.contains('/') {
        slug.clone()
    } else {
        format!("twig/{slug}")
    };
    let primary = primary_worktree_path(current_wt)
        .unwrap_or_else(|| current_wt.to_path_buf());
    let repo_name = primary
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let wt_parent = primary
        .parent()
        .map(|p| p.join(".twig-worktrees"))
        .unwrap_or_else(|| crate::config::paths::state_dir().join("worktrees"));
    std::fs::create_dir_all(&wt_parent).map_err(|e| format!("create worktree root: {e}"))?;
    let folder_slug = slug.replace('/', "-");
    let dst_wt = wt_parent.join(format!("{repo_name}-{folder_slug}"));

    if !dst_wt.exists() {
        let add_new = Command::new("git")
            .arg("-C")
            .arg(current_wt)
            .args(["worktree", "add", "-b", &branch])
            .arg(&dst_wt)
            .output()
            .map_err(|e| format!("git worktree add: {e}"))?;
        if !add_new.status.success() {
            let add_existing = Command::new("git")
                .arg("-C")
                .arg(current_wt)
                .args(["worktree", "add"])
                .arg(&dst_wt)
                .arg(&branch)
                .output()
                .map_err(|e| format!("git worktree add existing: {e}"))?;
            if !add_existing.status.success() {
                return Err(format!(
                    "git worktree add failed: {}",
                    String::from_utf8_lossy(&add_existing.stderr).trim()
                ));
            }
        }
        carry_uncommitted_state(current_wt, &dst_wt);
        let setup_hook = dst_wt.join(".twig/worktree_setup.sh");
        if setup_hook.is_file() {
            let mut cmd = Command::new("sh");
            cmd.arg(&setup_hook).current_dir(&dst_wt);
            let _ = run_process(&mut cmd, &[], Duration::from_secs(60), 65_536);
        }
    }

    let canon_dst = dst_wt.canonicalize().unwrap_or(dst_wt);
    Ok(json!({
        "action": "switched",
        "worktree": canon_dst.display().to_string(),
        "branch": branch,
        "summary": format!("live handoff → worktree {} (branch `{branch}`)", canon_dst.display()),
    }))
}

fn detect_default_base_branch(repo: &Path) -> String {
    if let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if let Some(b) = s.strip_prefix("refs/remotes/origin/") {
                if !b.is_empty() {
                    return b.to_owned();
                }
            }
        }
    }
    for candidate in ["main", "master", "develop"] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--verify", candidate])
            .output()
            .is_ok_and(|o| o.status.success());
        if ok {
            return candidate.to_owned();
        }
    }
    "main".to_owned()
}

/// Synthesize a conventional commit message matching recent repo history and create a git commit (`/commit`).
pub(super) fn run_smart_commit(
    repo: &Path,
    llm: &dyn Llm,
    hint: Option<&str>,
) -> Result<Value, String> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("git status: {e}"))?;
    if !status.status.success() {
        return Err(String::from_utf8_lossy(&status.stderr).trim().to_owned());
    }
    let porcelain = String::from_utf8_lossy(&status.stdout).into_owned();
    if porcelain.trim().is_empty() {
        return Err("working tree is clean — nothing to commit".into());
    }

    let pre_staged = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--cached", "--name-only"])
        .output()
        .ok()
        .is_some_and(|o| o.status.success() && !o.stdout.is_empty());
    if !pre_staged {
        let add = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["add", "-A"])
            .output()
            .map_err(|e| format!("git add -A: {e}"))?;
        if !add.status.success() {
            return Err(format!(
                "git add -A failed: {}",
                String::from_utf8_lossy(&add.stderr).trim()
            ));
        }
    }

    let diff_out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--cached"])
        .output()
        .map_err(|e| format!("git diff --cached: {e}"))?;
    let staged_diff = String::from_utf8_lossy(&diff_out.stdout).into_owned();
    if staged_diff.trim().is_empty() {
        return Err("no staged changes to commit".into());
    }
    let stat_out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--cached", "--shortstat"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let recent_log = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-n", "5", "--oneline"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();

    let clipped_diff: String = staged_diff.chars().take(24_000).collect();
    let user_hint = hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|h| format!("\nAuthor guidance: {h}\n"))
        .unwrap_or_default();
    let model = llm.roles().get("scout").to_owned();
    let reply = llm.complete(
        &model,
        "Write a concise, accurate Git commit message matching the repository's recent commit style. Output ONLY the raw commit message text (no markdown fences, no commentary): a <=72-char imperative subject line, optionally followed by a blank line and 1-4 short bullet points if multi-part.",
        &[json!({
            "role": "user",
            "content": format!(
                "Recent commits:\n{recent_log}\n{user_hint}\nStaged diff ({stat_out}):\n{clipped_diff}"
            )
        })],
        None,
    )?;

    let cleaned_msg = strip_ai_coauthor_trailers(
        reply
            .text
            .trim()
            .trim_start_matches("```text")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim(),
    );
    let message = if cleaned_msg.is_empty() {
        "chore: update working tree changes"
    } else {
        cleaned_msg.as_str()
    };

    let commit = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .output()
        .map_err(|e| format!("git commit: {e}"))?;
    if !commit.status.success() {
        return Err(format!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr).trim()
        ));
    }
    let sha = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "HEAD".into());
    let subject = message.lines().next().unwrap_or(message);
    Ok(json!({
        "sha": sha,
        "subject": subject,
        "message": message,
        "stat": stat_out,
        "summary": format!("committed {sha} · {subject} ({stat_out})"),
    }))
}

/// One-click T3-style PR flow (`/pr [--draft] [--base BRANCH] [hint]`):
/// commits dirty changes (stripping AI trailers), pushes branch, generates PR title/body, and opens PR via `gh`.
pub(super) fn ship_pull_request(
    repo: &Path,
    llm: &dyn Llm,
    raw_arg: Option<&str>,
) -> Result<Value, String> {
    let mut draft = false;
    let mut base: Option<String> = None;
    let mut hint_words = Vec::new();
    let tokens: Vec<&str> = raw_arg.unwrap_or("").split_whitespace().collect();
    let mut i = 0usize;
    while i < tokens.len() {
        match tokens[i] {
            "--draft" | "-d" => draft = true,
            "--base" | "-B" => {
                i += 1;
                if let Some(b) = tokens.get(i) {
                    base = Some((*b).to_owned());
                }
            }
            w => hint_words.push(w),
        }
        i += 1;
    }
    let hint = (!hint_words.is_empty()).then(|| hint_words.join(" "));

    let dirty = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("git status: {e}"))?;
    let mut auto_committed: Option<String> = None;
    if !String::from_utf8_lossy(&dirty.stdout).trim().is_empty() {
        let c = run_smart_commit(repo, llm, hint.as_deref())?;
        auto_committed = c["summary"].as_str().map(str::to_owned);
    }

    let branch_out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map_err(|e| format!("git branch: {e}"))?;
    let branch = String::from_utf8_lossy(&branch_out.stdout).trim().to_owned();
    let base_branch = base.unwrap_or_else(|| detect_default_base_branch(repo));
    if branch == base_branch || branch == "HEAD" {
        return Err(format!(
            "current branch is `{branch}` — switch to a feature worktree (`/worktree <name>`) before `/pr`"
        ));
    }

    let push = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["push", "-u", "origin", "HEAD"])
        .output()
        .map_err(|e| format!("git push: {e}"))?;
    if !push.status.success() {
        return Err(format!(
            "git push failed: {}",
            String::from_utf8_lossy(&push.stderr).trim()
        ));
    }

    if let Ok(existing) = Command::new("gh")
        .args(["pr", "view", "--json", "url"])
        .current_dir(repo)
        .output()
    {
        if existing.status.success() {
            if let Ok(v) = serde_json::from_slice::<Value>(&existing.stdout) {
                if let Some(url) = v["url"].as_str().filter(|s| !s.is_empty()) {
                    return Ok(json!({
                        "url": url,
                        "branch": branch,
                        "existed": true,
                        "committed": auto_committed,
                        "summary": format!("pushed `{branch}` → existing PR {url}"),
                    }));
                }
            }
        }
    }

    let log_out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "--oneline", &format!("{base_branch}..HEAD")])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let diff_out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", &format!("{base_branch}...HEAD")])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let clipped_diff: String = diff_out.chars().take(24_000).collect();
    let model = llm.roles().get("scout").to_owned();
    let reply = llm.complete(
        &model,
        "Write a clean GitHub Pull Request title and Markdown body. First line MUST be a concise <=72-char imperative title (no `#` prefix, no quotes). Follow with a blank line, `## Summary` (2-4 bullets), and `## Verification` (1-3 bullets). Never include AI co-author trailers or badges.",
        &[json!({
            "role": "user",
            "content": format!(
                "Branch: {branch} -> {base_branch}\nHint: {}\nCommits:\n{log_out}\n\nDiff:\n{clipped_diff}",
                hint.as_deref().unwrap_or("(none)")
            )
        })],
        None,
    )?;

    let cleaned = strip_ai_coauthor_trailers(reply.text.trim());
    let mut lines = cleaned.lines();
    let title = lines
        .next()
        .map(|t| t.trim().trim_start_matches('#').trim())
        .filter(|t| !t.is_empty())
        .unwrap_or(&branch)
        .to_owned();
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_owned();

    let mut gh_args = vec![
        "pr".to_owned(),
        "create".to_owned(),
        "--base".to_owned(),
        base_branch,
        "--head".to_owned(),
        branch.clone(),
        "--title".to_owned(),
        title.clone(),
        "--body".to_owned(),
        if body.is_empty() { log_out } else { body },
    ];
    if draft {
        gh_args.push("--draft".to_owned());
    }
    let pr_out = Command::new("gh")
        .args(&gh_args)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("gh pr create: {e}"))?;
    if !pr_out.status.success() {
        return Err(format!(
            "gh pr create failed: {}",
            String::from_utf8_lossy(&pr_out.stderr).trim()
        ));
    }
    let url = String::from_utf8_lossy(&pr_out.stdout).trim().to_owned();
    Ok(json!({
        "url": url,
        "title": title,
        "branch": branch,
        "draft": draft,
        "committed": auto_committed,
        "summary": format!("opened {}PR {url} · {title}", if draft { "draft " } else { "" }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ai_coauthors_while_preserving_human_coauthors_and_hands_off_worktree() {
        let msg = "feat: add cache\n\n- speed up lookup\n\nCo-Authored-By: Alice <alice@example.com>\nCo-Authored-By: Claude <noreply@anthropic.com>\n🤖 Generated with [Claude Code]";
        let cleaned = strip_ai_coauthor_trailers(msg);
        assert!(cleaned.contains("Alice <alice@example.com>"));
        assert!(!cleaned.contains("Claude"));
        assert!(!cleaned.contains("Generated with"));

        let base = std::env::temp_dir().join(format!("twig-wt-handoff-{}", std::process::id()));
        let repo = base.join("demo");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git").arg("init").current_dir(&repo).output().unwrap();
        Command::new("git")
            .args(["config", "user.email", "t@t.io"])
            .current_dir(&repo)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(&repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&repo).output().unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&repo)
            .output()
            .unwrap();
        // Dirty uncommitted edit should carry over into the new worktree!
        std::fs::write(repo.join("a.txt"), "v2-dirty\n").unwrap();

        let res = handoff_worktree(&repo, Some("feat-fast")).unwrap();
        assert_eq!(res["action"], "switched");
        assert_eq!(res["branch"], "twig/feat-fast");
        let new_wt = PathBuf::from(res["worktree"].as_str().unwrap());
        assert_eq!(std::fs::read_to_string(new_wt.join("a.txt")).unwrap(), "v2-dirty\n");

        let back = handoff_worktree(&new_wt, Some("back")).unwrap();
        assert_eq!(back["action"], "switched");
        let _ = std::fs::remove_dir_all(&base);
    }
}
