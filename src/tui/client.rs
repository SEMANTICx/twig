//! Host I/O boundary for `twig chat`:
//! - `ServeClient` subprocess JSON-RPC transport for `twig serve`
//! - `TerminalGuard` RAII raw-mode / alternate-screen / mouse / bracketed-paste manager
//! - Host helpers: `$EDITOR` bridge, OSC 52 + `wl-copy` clipboard, git branch, `--plain` fallback

use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use serde_json::{json, Value};

use crate::config::paths;

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|e| format!("terminal raw mode: {e}"))?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
        .map_err(|e| format!("enter alternate screen: {e}"))?;
        Ok(Self)
    }

    pub fn set_mouse(enabled: bool) {
        let mut stdout = io::stdout();
        if enabled {
            let _ = execute!(stdout, EnableMouseCapture);
        } else {
            let _ = execute!(stdout, DisableMouseCapture);
        }
    }

    pub fn suspend() {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }

    pub fn resume(mouse_enabled: bool) -> Result<(), String> {
        enable_raw_mode().map_err(|e| format!("resume raw mode: {e}"))?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
            .map_err(|e| format!("resume alternate screen: {e}"))?;
        if mouse_enabled {
            let _ = execute!(stdout, EnableMouseCapture);
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::suspend();
    }
}

/// Kitty native desktop notification (OSC 99): ignored when focused, delivered to WM when unfocused.
pub fn notify_osc99(title: &str, body: &str) {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(160)
            .collect()
    };
    let mut stdout = io::stdout();
    let _ = write!(
        stdout,
        "\x1b]99;i=twig:d=0:p=title;{}\x1b\\\x1b]99;i=twig:d=1:p=body;{}\x1b\\",
        clean(title),
        clean(body)
    );
    let _ = stdout.flush();
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub sid: String,
    pub label: String,
    pub turns: usize,
    pub age: String,
}

/// Scan `~/.local/state/twig/sessions/*.jsonl` newest-first for `/resume`.
pub fn list_sessions() -> Vec<SessionSummary> {
    let dir = paths::state_dir().join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<(SystemTime, std::path::PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .map(|e| {
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (mtime, e.path())
        })
        .collect();
    files.sort_by_key(|a| std::cmp::Reverse(a.0));

    let now = SystemTime::now();
    let mut out = Vec::new();
    for (mtime, path) in files.into_iter().take(25) {
        let Some(sid) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut title: Option<String> = None;
        let mut first_user: Option<String> = None;
        let mut turns = 0usize;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(row) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match row["kind"].as_str() {
                Some("session.renamed") => {
                    if let Some(t) = row["title"].as_str() {
                        title = Some(t.to_owned());
                    }
                }
                Some("message") => match row["message"]["role"].as_str() {
                    Some("user") if first_user.is_none() => {
                        if let Some(c) = row["message"]["content"].as_str() {
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
        out.push(SessionSummary {
            sid,
            label,
            turns,
            age,
        });
    }
    out
}

/// Run a local quick shell command (`!cmd`) directly in `repo` without spending LLM tokens.
pub fn run_local_shell(repo: &str, cmd: &str) -> (String, Option<i32>, Duration) {
    let started = Instant::now();
    match crate::exec::shell_command(cmd)
        .current_dir(repo)
        .output()
    {
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&err);
            }
            (combined, out.status.code(), started.elapsed())
        }
        Err(e) => (format!("failed to run command: {e}"), Some(1), started.elapsed()),
    }
}

/// Collect working-tree diff (or staged diff if unstaged is empty) for `/diff` and `/review`.
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
    let diff_path = std::env::temp_dir().join(format!("twig-review-{}.diff", std::process::id()));
    std::fs::write(&diff_path, &diff).map_err(|e| e.to_string())?;
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

/// Query available models from magpie (`GET /v1/models`) for the interactive `/model` picker.
pub fn fetch_magpie_models() -> Result<Vec<String>, String> {
    crate::llm::Provider::new().list_models()
}

/// Run `twig doctor` in `repo` and return `(combined_output, succeeded)`.
pub fn run_doctor(repo: &str) -> Result<(String, bool), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = Command::new(exe)
        .arg("doctor")
        .current_dir(repo)
        .output()
        .map_err(|e| format!("spawn twig doctor: {e}"))?;
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&err);
    }
    Ok((combined.trim_end().to_owned(), out.status.success()))
}

const PREFERENCES_TEMPLATE: &str = "\
# Project Preferences (auto-loaded by twig on every turn)

- Follow existing architecture, naming, and error-handling conventions.
- Run the project's test and lint suite before declaring a task done.
- Keep edits minimal, focused, and well-tested.
";

const PROJECT_JSON_TEMPLATE: &str = "{\n  \"notes\": \"Optional: add a 'check' command for automatic acceptance verification.\"\n}\n";

/// Ensure `<repo>/.twig/preferences.md` exists; returns `(path, created)`.
pub fn ensure_memory_file(repo: &str) -> Result<(PathBuf, bool), String> {
    let twig_dir = Path::new(repo).join(".twig");
    std::fs::create_dir_all(&twig_dir).map_err(|e| format!("create .twig: {e}"))?;
    let path = twig_dir.join("preferences.md");
    if path.exists() {
        return Ok((path, false));
    }
    std::fs::write(&path, PREFERENCES_TEMPLATE)
        .map_err(|e| format!("write preferences.md: {e}"))?;
    Ok((path, true))
}

/// Initialize `.twig/preferences.md` and `.twig/project.json` without overwriting existing files.
pub fn init_project_files(repo: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let twig_dir = Path::new(repo).join(".twig");
    std::fs::create_dir_all(&twig_dir).map_err(|e| format!("create .twig: {e}"))?;
    let mut created = Vec::new();
    let mut existing = Vec::new();
    for (name, template) in [
        ("preferences.md", PREFERENCES_TEMPLATE),
        ("project.json", PROJECT_JSON_TEMPLATE),
    ] {
        let path = twig_dir.join(name);
        let rel = format!(".twig/{name}");
        if path.exists() {
            existing.push(rel);
        } else {
            std::fs::write(&path, template).map_err(|e| format!("write {rel}: {e}"))?;
            created.push(rel);
        }
    }
    Ok((created, existing))
}

/// Archive `<sid>.jsonl` into `<state_dir>/sessions/archive/` and clean any stale lock file.
pub fn archive_session_file(sid: &str) -> Result<String, String> {
    archive_session_in_dir(&paths::state_dir().join("sessions"), sid)
}

fn archive_session_in_dir(sessions_dir: &Path, sid: &str) -> Result<String, String> {
    let trimmed = sid.trim();
    if trimmed.is_empty() {
        return Err("no active session id to archive".into());
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
    if lock.exists() {
        let _ = std::fs::remove_file(lock);
    }
    Ok(dst.display().to_string())
}

/// Write exported transcript Markdown to a default or user-specified path under `repo`.
pub fn write_export_file(
    repo: &str,
    sid: &str,
    custom: Option<&str>,
    content: &str,
) -> Result<String, String> {
    let target = match custom {
        Some(p) => {
            let pb = PathBuf::from(p);
            if pb.is_absolute() {
                pb
            } else {
                Path::new(repo).join(pb)
            }
        }
        None => {
            let short: String = sid.chars().take(10).collect();
            let fname = if short.is_empty() {
                "twig-export.md".to_owned()
            } else {
                format!("twig-export-{short}.md")
            };
            Path::new(repo).join(fname)
        }
    };
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&target, content).map_err(|e| format!("export: {e}"))?;
    Ok(shorten_home(&target.display().to_string()))
}

/// Format a `session.verify` RPC reply into `(chain_ok, concise_report)`.
pub fn format_verify_report(result: &Value) -> (bool, String) {
    let chain_ok = result["chain"]["ok"] == true;
    let chain_summary = if chain_ok {
        format!(
            "ok ({} entries)",
            result["chain"]["entries"].as_u64().unwrap_or(0)
        )
    } else {
        format!(
            "FAILED at #{} ({})",
            result["chain"]["bad_index"].as_u64().unwrap_or(0),
            result["chain"]["why"].as_str().unwrap_or("corrupt")
        )
    };
    let samples = result["guard"]["samples"].as_u64().unwrap_or(0);
    let stall = result["guard"]["stall"].as_u64().unwrap_or(0);
    let digest: String = result["guard"]["digest"]
        .as_str()
        .unwrap_or("—")
        .chars()
        .take(10)
        .collect();
    let hits = result["policy"]["hits"].as_u64().unwrap_or(0);
    let misses = result["policy"]["misses"].as_u64().unwrap_or(0);
    let rules = result["policy"]["size"].as_u64().unwrap_or(0);
    let mode = if result["read_only"] == true {
        "read-only"
    } else {
        "edit"
    };
    (
        chain_ok,
        format!(
            "blake3 chain: {chain_summary} · guard: {samples} samples, stall {stall}, digest {digest} · policy: {hits}h/{misses}m ({rules} cached) · mode: {mode}"
        ),
    )
}

const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

fn image_mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// Read an image file and encode it as an OpenAI-compatible `data:image/...;base64,...` URL.
pub fn load_image_data_url(repo: &str, raw_path: &str) -> Result<(String, String), String> {
    let trimmed = raw_path.trim().trim_matches(['"', '\'']);
    if trimmed.is_empty() {
        return Err("empty image path".into());
    }
    let pb = PathBuf::from(trimmed);
    let path = if pb.is_absolute() {
        pb
    } else {
        Path::new(repo).join(pb)
    };
    let mime = image_mime_for_path(&path)
        .ok_or_else(|| format!("unsupported image extension: {}", path.display()))?;
    let bytes = std::fs::read(&path).map_err(|e| format!("read image {}: {e}", path.display()))?;
    if bytes.is_empty() {
        return Err("image file is empty".into());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image too large ({}MB > 10MB limit)",
            bytes.len() / (1024 * 1024)
        ));
    }
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| trimmed.to_owned());
    Ok((label, format!("data:{mime};base64,{}", encode_base64(&bytes))))
}

/// Grab an image directly from the system clipboard (`wl-paste` on Linux, `pngpaste` on macOS, PowerShell on Windows).
pub fn paste_clipboard_image() -> Result<Option<(String, String)>, String> {
    #[cfg(target_os = "macos")]
    {
        let tmp = std::env::temp_dir().join(format!("twig-clip-{}.png", std::process::id()));
        let ok = Command::new("pngpaste")
            .arg(&tmp)
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            let _ = std::fs::remove_file(&tmp);
            return Ok(None);
        }
        let bytes = std::fs::read(&tmp).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp);
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Ok(None);
        }
        return Ok(Some((
            format!("clipboard.png ({}KB)", bytes.len() / 1024),
            format!("data:image/png;base64,{}", encode_base64(&bytes)),
        )));
    }
    #[cfg(windows)]
    {
        let tmp = std::env::temp_dir().join(format!("twig-clip-{}.png", std::process::id()));
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; $i = [System.Windows.Forms.Clipboard]::GetImage(); if ($i -ne $null) {{ $i.Save('{}') }}",
            tmp.display()
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output();
        let bytes = std::fs::read(&tmp).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp);
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Ok(None);
        }
        return Ok(Some((
            format!("clipboard.png ({}KB)", bytes.len() / 1024),
            format!("data:image/png;base64,{}", encode_base64(&bytes)),
        )));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let list = Command::new("wl-paste")
            .arg("--list-types")
            .output()
            .map_err(|e| format!("wl-paste unavailable: {e}"))?;
        if !list.status.success() {
            return Ok(None);
        }
        let types = String::from_utf8_lossy(&list.stdout);
        let mime = if types.lines().any(|l| l.trim() == "image/png") {
            "image/png"
        } else if types.lines().any(|l| l.trim() == "image/jpeg") {
            "image/jpeg"
        } else {
            return Ok(None);
        };
        let data = Command::new("wl-paste")
            .args(["--no-newline", "--type", mime])
            .output()
            .map_err(|e| format!("wl-paste image: {e}"))?;
        if !data.status.success() || data.stdout.is_empty() {
            return Ok(None);
        }
        if data.stdout.len() > MAX_IMAGE_BYTES {
            return Err("clipboard image exceeds 10MB limit".into());
        }
        let ext = if mime == "image/png" { "png" } else { "jpg" };
        Ok(Some((
            format!("clipboard.{ext} ({}KB)", data.stdout.len() / 1024),
            format!("data:{mime};base64,{}", encode_base64(&data.stdout)),
        )))
    }
}

/// Auto-detect `@path/to/image.png` mentions inside a prompt and load existing image files.
pub fn extract_inline_images(repo: &str, text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        let Some(candidate) = token.strip_prefix('@') else {
            continue;
        };
        let cleaned = candidate.trim_end_matches([',', '.', ';', ')', ']']);
        if image_mime_for_path(Path::new(cleaned)).is_some() {
            if let Ok(img) = load_image_data_url(repo, cleaned) {
                out.push(img);
            }
        }
    }
    out
}

/// Prompt sent by `/evolve` for human-reviewed session self-improvement.
pub const EVOLVE_PROMPT: &str = "\
Reflect on this session's friction, repeated errors, tool papercuts, or repo-specific conventions you discovered. \
Propose ONE concise, high-leverage improvement to `.twig/preferences.md` or a project skill under `.twig/skills/<name>/SKILL.md` \
(keep skills <500 lines with crisp triggers and verify with `twig skill doctor` once approved). \
Do NOT edit Rust source code or locked harness files, and do NOT modify files yet—present the proposed change as a clear unified diff with a brief rationale so I can review and approve applying it.";

/// Format an `mcp.list` RPC reply cleanly for the transcript (tools, prompts/skills, and resources).
pub fn format_mcp_report(result: &Value) -> String {
    let servers = result["servers"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if servers.is_empty() {
        return "no MCP servers configured (add `mcpServers` to `.twig/mcp.json` or `~/.config/twig/mcp.json`)".into();
    }
    let mut lines = Vec::new();
    for s in servers {
        let name = s["server"].as_str().unwrap_or("?");
        if s["ok"] == true {
            let tools: Vec<&str> = s["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t["name"].as_str())
                .collect();
            let prompts: Vec<&str> = s["prompts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p["name"].as_str())
                .collect();
            let resources: Vec<&str> = s["resources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r["name"].as_str().or_else(|| r["uri"].as_str()))
                .collect();
            let mut parts = Vec::new();
            if tools.is_empty() {
                parts.push("0 tools".to_owned());
            } else {
                parts.push(format!("{} tools: {}", tools.len(), tools.join(", ")));
            }
            if !prompts.is_empty() {
                parts.push(format!("{} prompts: {}", prompts.len(), prompts.join(", ")));
            }
            if !resources.is_empty() {
                parts.push(format!(
                    "{} resources: {}",
                    resources.len(),
                    resources.join(", ")
                ));
            }
            lines.push(format!("- **{name}** ({})", parts.join(" · ")));
        } else {
            let err = s["error"].as_str().unwrap_or("error");
            lines.push(format!("- **{name}** — error: `{err}`"));
        }
    }
    lines.join("\n")
}

/// Format a `tasks.list` RPC reply cleanly for `/tasks`.
pub fn format_tasks_report(result: &Value) -> String {
    let tasks = result["tasks"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if tasks.is_empty() {
        return "no background tasks started in this session".into();
    }
    let mut lines = Vec::new();
    for t in tasks {
        let id = t["id"].as_str().unwrap_or("?");
        let status = t["status"].as_str().unwrap_or("?");
        let secs = t["elapsed_secs"].as_u64().unwrap_or(0);
        let cmd = t["command"].as_str().unwrap_or("");
        let exit = t["exit_code"]
            .as_i64()
            .map(|c| format!(" · exit {c}"))
            .unwrap_or_default();
        lines.push(format!("- **{id}** `{status}` ({secs}s{exit}) — `{cmd}`"));
    }
    lines.join("\n")
}

/// Format effective `HarnessConfig` summary for `/config`.
pub fn format_config_report() -> Result<String, String> {
    let (cfg, src) = crate::config::HarnessConfig::resolve()?;
    Ok(format!(
        "config:{src} · max_turns:{} · stall_turns:{} · context_max:{}KB · timeout:{}s · isolation:{}",
        cfg.max_turns,
        cfg.stall_turns,
        cfg.context_max_bytes / 1024,
        cfg.bash_timeout_secs,
        cfg.isolation
    ))
}

/// Read `<repo>/.twig/preferences.md` for `/memory show`.
pub fn read_memory_preview(repo: &str) -> Result<Option<String>, String> {
    let path = Path::new(repo).join(".twig/preferences.md");
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    Ok(Some(body))
}

/// Format `.twig/` initialization results for `/init`.
pub fn format_init_report(created: &[String], existing: &[String]) -> String {
    let mut parts = Vec::new();
    if !created.is_empty() {
        parts.push(format!("created {}", created.join(", ")));
    }
    if !existing.is_empty() {
        parts.push(format!("kept existing {}", existing.join(", ")));
    }
    format!("twig init: {}", parts.join(" · "))
}

pub fn detect_git_branch(repo: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", repo, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!name.is_empty()).then_some(name)
}

/// Parse `git diff --shortstat` output into `(insertions, deletions)`.
pub fn parse_git_shortstat(text: &str) -> Option<(usize, usize)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut ins = 0usize;
    let mut del = 0usize;
    for part in trimmed.split(',') {
        let p = part.trim();
        let n = p
            .split_whitespace()
            .next()
            .and_then(|tok| tok.parse::<usize>().ok());
        if let Some(count) = n {
            if p.contains("insertion") {
                ins = count;
            } else if p.contains("deletion") {
                del = count;
            }
        }
    }
    (ins > 0 || del > 0).then_some((ins, del))
}

/// Compute live working-tree `+N -M` line diff stat against `HEAD` (falls back to unstaged diff).
pub fn detect_git_diff_stat(repo: &str) -> Option<(usize, usize)> {
    let head = Command::new("git")
        .args(["-C", repo, "diff", "HEAD", "--shortstat"])
        .output()
        .ok();
    if let Some(out) = head {
        if out.status.success() {
            return parse_git_shortstat(&String::from_utf8_lossy(&out.stdout));
        }
    }
    let plain = Command::new("git")
        .args(["-C", repo, "diff", "--shortstat"])
        .output()
        .ok()?;
    plain
        .status
        .success()
        .then(|| parse_git_shortstat(&String::from_utf8_lossy(&plain.stdout)))
        .flatten()
}

const MAX_INPUT_HISTORY: usize = 200;

fn history_file_path() -> PathBuf {
    paths::state_dir().join("history.txt")
}

/// Load persistent cross-session prompt history (JSON-encoded one entry per line).
pub fn load_input_history() -> Vec<String> {
    load_input_history_from(&history_file_path())
}

fn load_input_history_from(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let item = serde_json::from_str::<String>(trimmed).unwrap_or_else(|_| trimmed.to_owned());
        if !item.trim().is_empty() {
            out.push(item);
        }
    }
    if out.len() > MAX_INPUT_HISTORY {
        out.drain(..out.len() - MAX_INPUT_HISTORY);
    }
    out
}

/// Append a submitted prompt to persistent cross-session history (deduped tail, capped at 200).
pub fn append_input_history(entry: &str) {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return;
    }
    append_input_history_at(&history_file_path(), trimmed);
}

fn append_input_history_at(path: &Path, entry: &str) {
    let mut items = load_input_history_from(path);
    items.retain(|x| x != entry);
    items.push(entry.to_owned());
    if items.len() > MAX_INPUT_HISTORY {
        items.drain(..items.len() - MAX_INPUT_HISTORY);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body: String = items
        .iter()
        .filter_map(|s| serde_json::to_string(s).ok())
        .map(|j| format!("{j}\n"))
        .collect();
    let _ = std::fs::write(path, body);
}

pub fn shorten_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_owned()
}

fn run_editor_on_path(path: &Path) -> Result<(), String> {
    let default_ed = if cfg!(windows) { "notepad" } else { "vi" };
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| default_ed.into());
    let parts = shlex::split(&editor).unwrap_or_else(|| vec![editor]);
    let (bin, args) = parts
        .split_first()
        .ok_or_else(|| "empty $EDITOR".to_owned())?;
    let status = Command::new(bin)
        .args(args)
        .arg(path)
        .status()
        .map_err(|e| format!("spawn {bin}: {e}"))?;
    if !status.success() {
        return Err(format!("{bin} exited with {status}"));
    }
    Ok(())
}

pub fn edit_in_external_editor(initial: &str) -> Result<String, String> {
    let path = std::env::temp_dir().join(format!("twig-prompt-{}.md", std::process::id()));
    std::fs::write(&path, initial).map_err(|e| e.to_string())?;
    let run_res = run_editor_on_path(&path);
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string());
    let _ = std::fs::remove_file(&path);
    run_res?;
    content
}

/// Open an existing repository file (e.g. `.twig/preferences.md`) directly in `$EDITOR`.
pub fn edit_file_in_external_editor(path: &Path) -> Result<usize, String> {
    run_editor_on_path(path)?;
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    Ok(content.lines().count())
}

/// Copy text to system clipboard via OSC 52 + native fallback (`wl-copy`/`pbcopy`/`clip`).
pub fn copy_to_clipboard(text: &str) {
    let b64 = encode_base64(text.as_bytes());
    let mut stdout = io::stdout();
    let _ = write!(stdout, "\x1b]52;c;{b64}\x07");
    let _ = stdout.flush();
    let clip_bin = if cfg!(target_os = "macos") {
        "pbcopy"
    } else if cfg!(windows) {
        "clip"
    } else {
        "wl-copy"
    };
    if let Ok(mut child) = Command::new(clip_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Asynchronous host-side job results delivered back to the TUI event loop.
pub enum LocalMsg {
    Shell {
        id: String,
        output: String,
        exit_code: Option<i32>,
        elapsed: Duration,
    },
    Review(Result<String, String>),
    Doctor(Result<(String, bool), String>),
    Models(Result<Vec<String>, String>),
    GitStat(Option<(usize, usize)>),
}

/// Non-blocking worker pool for host-side commands (`!cmd`, `/doctor`, `/review`, `/model`, git stat).
pub struct LocalWorkers {
    tx: mpsc::Sender<LocalMsg>,
    rx: Receiver<LocalMsg>,
}

impl LocalWorkers {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx }
    }

    pub fn try_recv(&self) -> Option<LocalMsg> {
        self.rx.try_recv().ok()
    }

    pub fn spawn_models(&self) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(LocalMsg::Models(fetch_magpie_models()));
        });
    }

    pub fn spawn_doctor(&self, repo: String) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(LocalMsg::Doctor(run_doctor(&repo)));
        });
    }

    pub fn spawn_review(&self, repo: String) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(LocalMsg::Review(run_independent_review(&repo)));
        });
    }

    pub fn spawn_shell(&self, repo: String, id: String, cmd: String) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let (output, exit_code, elapsed) = run_local_shell(&repo, &cmd);
            let _ = tx.send(LocalMsg::Shell {
                id,
                output,
                exit_code,
                elapsed,
            });
        });
    }

    pub fn spawn_git_stat(&self, repo: String) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(LocalMsg::GitStat(detect_git_diff_stat(&repo)));
        });
    }
}

/// Format turn RPC parameters including any pending or inline `@image` attachments.
pub fn build_turn_params(
    repo: &str,
    text: String,
    mut images: Vec<(String, String)>,
) -> (Value, Vec<String>) {
    images.extend(extract_inline_images(repo, &text));
    if images.is_empty() {
        return (json!({"input": text}), Vec::new());
    }
    let labels: Vec<String> = images.iter().map(|(l, _)| l.clone()).collect();
    let payload: Vec<Value> = images
        .into_iter()
        .map(|(label, url)| json!({"label": label, "url": url}))
        .collect();
    (json!({"input": text, "images": payload}), labels)
}

#[derive(Debug)]
pub enum ServerMsg {
    Event(Value),
    Reply { id: u64, result: Value },
    Error { id: Option<u64>, message: String },
    Stderr(String),
    Exited,
}

pub struct ServeClient {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<ServerMsg>,
    next_id: u64,
}

impl ServeClient {
    pub fn spawn(repo: &str, resume: Option<&str>) -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut command = Command::new(exe);
        command
            .arg("serve")
            .arg(repo)
            .arg(paths::state_dir().join("sessions"));
        if let Some(id) = resume {
            command.arg(id);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn twig serve: {e}"))?;
        let stdin = child.stdin.take().ok_or("missing server stdin")?;
        let stdout = child.stdout.take().ok_or("missing server stdout")?;
        let stderr = child.stderr.take().ok_or("missing server stderr")?;
        let (tx, rx) = mpsc::channel();

        let tx_out = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let msg = if v.get("ev").is_some() {
                    ServerMsg::Event(v)
                } else if let Some(err) = v.get("error") {
                    let message = err
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| err.to_string());
                    ServerMsg::Error {
                        id: v.get("id").and_then(Value::as_u64),
                        message,
                    }
                } else {
                    ServerMsg::Reply {
                        id: v.get("id").and_then(Value::as_u64).unwrap_or(0),
                        result: v.get("result").cloned().unwrap_or(Value::Null),
                    }
                };
                if tx_out.send(msg).is_err() {
                    break;
                }
            }
            let _ = tx_out.send(ServerMsg::Exited);
        });

        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !line.trim().is_empty() && tx.send(ServerMsg::Stderr(line)).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            child,
            stdin: Some(stdin),
            rx,
            next_id: 0,
        })
    }

    pub fn send(&mut self, method: &str, params: Value) -> Result<u64, String> {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({"id": id, "method": method, "params": params});
        let stdin = self.stdin.as_mut().ok_or("server stdin closed")?;
        writeln!(stdin, "{req}").map_err(|e| e.to_string())?;
        stdin.flush().map_err(|e| e.to_string())?;
        Ok(id)
    }

    pub fn try_recv(&self) -> Option<ServerMsg> {
        self.rx.try_recv().ok()
    }

    pub fn shutdown(mut self, cancel_active: bool) {
        if cancel_active {
            let _ = self.send("cancel", json!({}));
        }
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// Plain line-delimited fallback for piped / non-TTY sessions or `--plain`.
pub fn chat_plain(repo: &str, resume: Option<&str>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(exe);
    command
        .arg("serve")
        .arg(repo)
        .arg(paths::state_dir().join("sessions"));
    if let Some(id) = resume {
        command.arg(id);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("missing server stdout")?;
    let input = Arc::new(Mutex::new(
        child.stdin.take().ok_or("missing server stdin")?,
    ));
    let reader = std::thread::spawn(move || {
        let terminal = std::io::stdout().is_terminal();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let mut out = std::io::stdout().lock();
            match v["ev"].as_str() {
                Some("model.delta") => {
                    let _ = write!(out, "{}", v["text"].as_str().unwrap_or(""));
                }
                Some("tool.started") => {
                    let name = v["tool"].as_str().unwrap_or("tool");
                    let _ = writeln!(out, "\n[{name} {}]", v["id"].as_str().unwrap_or(""));
                }
                Some("tool.output") => {
                    let _ = write!(out, "{}", v["text"].as_str().unwrap_or(""));
                }
                Some("approval.requested") => {
                    let _ = writeln!(
                        out,
                        "\nApproval {}: {}\n/approve ID or /deny ID",
                        v["id"], v["args"]
                    );
                }
                Some("turn.completed") => {
                    if v["reason"] == "final" {
                        let _ = writeln!(out);
                    }
                }
                Some("context.compacted") => {
                    let _ = writeln!(out, "\n[context compacted]");
                }
                Some(_) => {}
                None => {
                    if let Some(e) = v.get("error") {
                        let _ = writeln!(out, "\nError: {e}");
                    } else if v["result"]["done"].is_boolean() {
                        let _ = writeln!(
                            out,
                            "\n[{}]",
                            v["result"]["reason"].as_str().unwrap_or("done")
                        );
                    }
                    if terminal {
                        let _ = write!(out, "\n> ");
                    }
                }
            }
            let _ = out.flush();
        }
    });
    println!("Twig · magpie\nEnter a task. During a run use /steer TEXT, /cancel, /approve ID, /deny ID. /quit exits.");
    let mut id = 0;
    for line in std::io::stdin().lock().lines() {
        let text = line.map_err(|e| e.to_string())?;
        if text.trim().is_empty() {
            continue;
        }
        id += 1;
        let mut request = if text == "/quit" {
            break;
        } else if text == "/cancel" {
            json!({"method": "cancel"})
        } else if let Some(t) = text.strip_prefix("/steer ") {
            json!({"method": "steer", "params": {"input": t}})
        } else if let Some(t) = text.strip_prefix("/approve ") {
            json!({"method": "approval.resolve", "params": {"id": t.trim(), "allow": true}})
        } else if let Some(t) = text.strip_prefix("/deny ") {
            json!({"method": "approval.resolve", "params": {"id": t.trim(), "allow": false}})
        } else {
            json!({"method": "turn", "params": {"input": text}})
        };
        request["id"] = json!(id);
        let mut writer = input.lock().map_err(|_| "input lock")?;
        writeln!(writer, "{request}").map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
    }
    if std::io::stdin().is_terminal() {
        let mut w = input.lock().unwrap();
        let _ = writeln!(w, "{{\"method\":\"cancel\"}}");
    }
    drop(input);
    let _ = child.wait();
    let _ = reader.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encoder_matches_rfc4648() {
        assert_eq!(encode_base64(b"hello"), "aGVsbG8=");
        assert_eq!(encode_base64(b"twig"), "dHdpZw==");
    }

    #[test]
    fn init_project_files_creates_templates_without_overwriting() {
        let dir = std::env::temp_dir().join(format!("twig-init-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (created, existing) = init_project_files(dir.to_str().unwrap()).unwrap();
        assert_eq!(created, vec![".twig/preferences.md", ".twig/project.json"]);
        assert!(existing.is_empty());

        std::fs::write(dir.join(".twig/preferences.md"), "custom prefs\n").unwrap();
        let (created2, existing2) = init_project_files(dir.to_str().unwrap()).unwrap();
        assert!(created2.is_empty());
        assert_eq!(existing2, vec![".twig/preferences.md", ".twig/project.json"]);
        assert_eq!(
            std::fs::read_to_string(dir.join(".twig/preferences.md")).unwrap(),
            "custom prefs\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_session_moves_jsonl_and_cleans_lock() {
        let dir = std::env::temp_dir().join(format!("twig-archive-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s1.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.join("s1.lock"), "1").unwrap();

        let dst = archive_session_in_dir(&dir, "s1").unwrap();
        assert!(dst.ends_with("archive/s1.jsonl"));
        assert!(!dir.join("s1.jsonl").exists());
        assert!(!dir.join("s1.lock").exists());
        assert!(dir.join("archive/s1.jsonl").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_shortstat_tasks_mcp_and_persistent_history_helpers_work() {
        assert_eq!(
            parse_git_shortstat(" 3 files changed, 42 insertions(+), 8 deletions(-)\n"),
            Some((42, 8))
        );
        assert_eq!(parse_git_shortstat(" 1 file changed, 5 insertions(+)"), Some((5, 0)));
        assert_eq!(parse_git_shortstat(""), None);

        let mcp = format_mcp_report(&json!({
            "servers": [{
                "server": "s1",
                "ok": true,
                "tools": [{"name": "search"}],
                "prompts": [{"name": "audit-skill"}],
                "resources": [{"uri": "mem://doc", "name": "Doc"}]
            }]
        }));
        assert!(mcp.contains("1 tools: search"));
        assert!(mcp.contains("1 prompts: audit-skill"));
        assert!(mcp.contains("1 resources: Doc"));

        let tasks = format_tasks_report(&json!({
            "tasks": [{"id": "bg-1", "status": "running", "elapsed_secs": 4, "command": "cargo test"}]
        }));
        assert!(tasks.contains("bg-1") && tasks.contains("running") && tasks.contains("cargo test"));

        let hist_path = std::env::temp_dir().join(format!("twig-hist-test-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&hist_path);
        append_input_history_at(&hist_path, "first\nmultiline");
        append_input_history_at(&hist_path, "second");
        append_input_history_at(&hist_path, "first\nmultiline");
        assert_eq!(
            load_input_history_from(&hist_path),
            vec!["second".to_owned(), "first\nmultiline".to_owned()]
        );
        let _ = std::fs::remove_file(&hist_path);
    }
}
