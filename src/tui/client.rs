//! Host I/O boundary for `twig chat`:
//! - `ServeClient` subprocess JSON-RPC transport for `twig serve`
//! - `TerminalGuard` RAII raw-mode / alternate-screen / mouse / bracketed-paste manager
//! - Host helpers: `$EDITOR` bridge, OSC 52 + `wl-copy` clipboard, git branch, `--plain` fallback

use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

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

/// Query available models from magpie (`GET /v1/models`) plus detected local CLI runners (`cli:*`) for `/model`.
pub fn fetch_magpie_models() -> Result<Vec<String>, String> {
    let cli_runners = crate::llm::cli::detected_runners();
    match crate::llm::Provider::new().list_models() {
        Ok(mut models) => {
            models.extend(cli_runners);
            Ok(models)
        }
        Err(_e) if !cli_runners.is_empty() => Ok(cli_runners),
        Err(e) => Err(e),
    }
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

const PROJECT_JSON_TEMPLATE: &str = "{\n  \"notes\": \"Optional: set 'check_on_change' (runs only when files change), 'post_edit_check' (runs after edit/write/apply_patch), or 'check' (runs every turn).\"\n}\n";

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

/// Append a quick `# <note>` memory bullet to either project `.twig/preferences.md` or global `~/.config/twig/preferences.md`.
pub fn append_quick_memory(repo: &str, note: &str, global: bool) -> Result<String, String> {
    let clean = note.trim().trim_start_matches('-').trim();
    if clean.is_empty() {
        return Err("memory note is empty".into());
    }
    let (target, label) = if global {
        let dir = paths::config_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("create global config dir: {e}"))?;
        let p = dir.join("preferences.md");
        if !p.exists() {
            std::fs::write(&p, "# Global Preferences\n\n")
                .map_err(|e| format!("write global preferences.md: {e}"))?;
        }
        let disp = shorten_home(&p.display().to_string());
        (p, disp)
    } else {
        let (p, _) = ensure_memory_file(repo)?;
        (p, ".twig/preferences.md".to_owned())
    };
    let mut existing = std::fs::read_to_string(&target).unwrap_or_default();
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push_str(&format!("- {clean}\n"));
    std::fs::write(&target, existing).map_err(|e| format!("save memory: {e}"))?;
    Ok(label)
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

fn has_valid_image_magic(mime: &str, bytes: &[u8]) -> bool {
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xFF\xD8\xFF"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        _ => false,
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
    if !has_valid_image_magic(mime, &bytes) {
        return Err(format!(
            "invalid {mime} header in {}",
            path.display()
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

/// Auto-detect `@path/to/image.png` mentions inside a prompt and load existing workspace image files.
/// Confines auto-extracted `@...` paths to `repo` so pasted logs cannot silently exfiltrate outside files.
pub fn extract_inline_images(repo: &str, text: &str) -> Vec<(String, String)> {
    let Ok(canon_repo) = Path::new(repo).canonicalize() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        let Some(candidate) = token.strip_prefix('@') else {
            continue;
        };
        let cleaned = candidate.trim_end_matches([',', '.', ';', ')', ']']);
        let pb = Path::new(cleaned);
        if pb.is_absolute() || image_mime_for_path(pb).is_none() {
            continue;
        }
        let joined = canon_repo.join(pb);
        let Ok(canon_file) = joined.canonicalize() else {
            continue;
        };
        if !canon_file.starts_with(&canon_repo) {
            continue;
        }
        if let Ok(img) = load_image_data_url(repo, cleaned) {
            out.push(img);
        }
    }
    out
}

const MAX_MEMORY_CHARS: usize = 2_400;

/// Format an `mcp.list` RPC reply cleanly for the transcript (tools, prompts/skills, and resources).
pub fn format_mcp_report(result: &Value) -> String {
    let servers = result["servers"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if servers.is_empty() {
        return "no MCP servers configured (add `mcpServers` to `.twig/mcp.json`, or run `/mcp init-rea` to scaffold optional REA MCP)".into();
    }
    let mut lines = Vec::new();
    for s in servers {
        let name = s["server"].as_str().unwrap_or("?");
        if s["disabled"] == true {
            lines.push(format!(
                "- **{name}** — `disabled` (zero overhead; run `/mcp enable {name}` to turn on)"
            ));
        } else if s["ok"] == true {
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

/// Read `<repo>/.twig/preferences.md` for `/memory show` with bounded-budget indicator.
pub fn read_memory_preview(repo: &str) -> Result<Option<String>, String> {
    let path = Path::new(repo).join(".twig/preferences.md");
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let chars = body.chars().count();
    let warn = if chars > MAX_MEMORY_CHARS {
        " — OVER 2,400-CHAR CAP, run `/evolve` or `/memory` to consolidate!"
    } else {
        ""
    };
    Ok(Some(format!(
        "*Memory budget: {chars} / {MAX_MEMORY_CHARS} chars{warn}*\n\n{body}"
    )))
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

/// Execute `/rci [status|harvest|distill]` and format `(is_distill_success, notice, optional_markdown)`.
pub fn format_rci_slash(repo: &str, sub: &str) -> Result<(bool, String, Option<String>), String> {
    let repo_path = Path::new(repo);
    let sessions_dir = paths::state_dir().join("sessions");
    match sub {
        "harvest" => {
            let rep = crate::eval::rci::harvest_sessions(&sessions_dir)?;
            let notice = format!(
                "RCI label-free harvest: {} verified collective session(s) out of {} valid ledgers",
                rep.verified_collective_sessions, rep.chain_valid_files
            );
            let body = serde_json::to_string_pretty(&rep.role_credits)
                .ok()
                .map(|p| format!("```json\n{p}\n```"));
            Ok((false, notice, body))
        }
        "distill" | "distill --online" => {
            let online = sub.contains("--online");
            let (rep, files) =
                crate::eval::rci::distill_role_notes(repo_path, &sessions_dir, online)?;
            let msg = format!(
                "RCI distilled {} role protocol cards ({} verified sessions harvested)",
                files.len(),
                rep.verified_collective_sessions
            );
            let bullets = files
                .into_iter()
                .map(|f| format!("- `{f}`"))
                .collect::<Vec<_>>()
                .join("\n");
            Ok((true, msg, Some(bullets)))
        }
        _ => Ok((false, crate::runtime::blackboard::status_report(repo_path), None)),
    }
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

fn spawn_detached_opener(target: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(target);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", target]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(target);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn detect_file_opener_scheme() -> &'static str {
    if let Ok(v) = std::env::var("TWIG_FILE_OPENER") {
        match v.trim().to_ascii_lowercase().as_str() {
            "cursor" => return "cursor",
            "vscode" | "code" => return "vscode",
            "windsurf" => return "windsurf",
            "zed" => return "zed",
            "none" | "copy" => return "copy",
            "system" | "xdg" => return "system",
            _ => {}
        }
    }
    if std::env::var_os("CURSOR_TRACE_ID").is_some() {
        return "cursor";
    }
    if std::env::var_os("VSCODE_IPC_HOOK_CLI").is_some() {
        return "vscode";
    }
    if std::env::var_os("ZED_TERM").is_some() {
        return "zed";
    }
    match std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "cursor" => "cursor",
        "vscode" => "vscode",
        "windsurf" => "windsurf",
        "zed" => "zed",
        _ => "system",
    }
}

/// Open a clicked or `/open` target (`https://...`, `file:///path#L42`, or `src/foo.rs:42[:col]`)
/// using the OS browser or Codex-style `file_opener` (`cursor://`, `vscode://`, `zed://`, or system).
pub fn open_target_in_os(repo: &str, raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_matches(['"', '\'', '<', '>']);
    if trimmed.is_empty() {
        return Err("empty link target".into());
    }
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("mailto:")
    {
        if std::env::var_os("SSH_CONNECTION").is_some() {
            copy_to_clipboard(trimmed);
        }
        return match spawn_detached_opener(trimmed) {
            Ok(()) => Ok(format!("Opened {trimmed}")),
            Err(_) => {
                copy_to_clipboard(trimmed);
                Ok(format!("Copied URL to clipboard: {trimmed}"))
            }
        };
    }

    let local_spec = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    let (raw_path, range) = parse_path_and_line_range(local_spec);
    let candidate = Path::new(raw_path);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        Path::new(repo).join(candidate)
    };
    let canon = resolved
        .canonicalize()
        .map_err(|_| format!("target not found: {trimmed}"))?;
    let abs = canon.display().to_string();
    let line = range.map(|(s, _)| s);
    let scheme = detect_file_opener_scheme();
    if scheme == "copy" {
        let loc = line.map(|l| format!("{abs}:{l}")).unwrap_or(abs);
        copy_to_clipboard(&loc);
        return Ok(format!("Copied {loc}"));
    }
    let uri_or_path = match scheme {
        "cursor" | "vscode" | "windsurf" | "zed" => {
            let slash_abs = if abs.starts_with('/') {
                abs.clone()
            } else {
                format!("/{abs}")
            };
            match line {
                Some(l) => format!("{scheme}://file{slash_abs}:{l}:1"),
                None => format!("{scheme}://file{slash_abs}"),
            }
        }
        _ => abs.clone(),
    };
    let label = match line {
        Some(l) => format!("{}:{l}", shorten_home(&abs)),
        None => shorten_home(&abs),
    };
    match spawn_detached_opener(&uri_or_path) {
        Ok(()) => Ok(format!("Opened {label}")),
        Err(_) => {
            copy_to_clipboard(&label);
            Ok(format!("Copied path to clipboard: {label}"))
        }
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
    Review {
        outcome: Result<(String, Option<String>), String>,
        auto_fix: bool,
    },
    Walkthrough(Result<String, String>),
    DualPlan(Result<String, String>),
    Doctor(Result<(String, bool), String>),
    Models(Result<Vec<String>, String>),
    GitStat(Option<(usize, usize)>),
    SubagentEvent(Value),
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

    pub fn spawn_review(&self, repo: String, auto_fix: bool) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let outcome = crate::runtime::workflows::run_independent_review(&repo);
            let _ = tx.send(LocalMsg::Review { outcome, auto_fix });
        });
    }

    pub fn spawn_walkthrough(&self, repo: String) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(LocalMsg::Walkthrough(
                crate::runtime::workflows::run_walkthrough(&repo),
            ));
        });
    }

    pub fn spawn_dual_plan(&self, repo: String, goal: String) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(LocalMsg::DualPlan(
                crate::runtime::workflows::run_dual_plan(&repo, &goal),
            ));
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

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_subagent_turn(
        &self,
        repo: String,
        parent_sid: String,
        ui_id: String,
        child_sid: String,
        role: String,
        model: Option<String>,
        effort: Option<String>,
        input: String,
    ) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let role_enum = crate::runtime::subagent::Role::from_str_or_scout(&role);
            let cfg = match crate::runtime::subagent::SubagentConfig::for_repo(&repo) {
                Ok(mut c) => {
                    c.concurrency = 2;
                    c.max_output_bytes = 64_000;
                    c
                }
                Err(e) => {
                    let _ = tx.send(LocalMsg::SubagentEvent(json!({
                        "ev": "subagent.completed",
                        "id": ui_id,
                        "sid": child_sid,
                        "role": role,
                        "ok": false,
                        "elapsed_secs": 0,
                        "error": e.to_string(),
                    })));
                    return;
                }
            };
            let pool_res = crate::runtime::subagent::SubagentPool::new_for_parent(
                cfg,
                (!parent_sid.is_empty()).then_some(parent_sid),
            );
            let pool = match pool_res {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(LocalMsg::SubagentEvent(json!({
                        "ev": "subagent.completed",
                        "id": ui_id,
                        "sid": child_sid,
                        "role": role,
                        "ok": false,
                        "elapsed_secs": 0,
                        "error": e.to_string(),
                    })));
                    return;
                }
            };
            let tx_ev = tx.clone();
            let ui_ev_id = ui_id.clone();
            let ev_role = role.clone();
            let event_cb = move |mut ev: Value| {
                if let Some(obj) = ev.as_object_mut() {
                    obj.insert("id".into(), Value::String(ui_ev_id.clone()));
                    obj.insert("role".into(), Value::String(ev_role.clone()));
                }
                let _ = tx_ev.send(LocalMsg::SubagentEvent(ev));
            };
            let parent_artifacts = paths::state_dir().join("sessions").join("artifacts");
            let run = pool.run_resumable_live(
                role_enum,
                &input,
                model.as_deref(),
                effort.as_deref(),
                Some(&child_sid),
                Some(&parent_artifacts),
                None,
                Some(&event_cb),
            );
            let elapsed_secs = started.elapsed().as_secs();
            match run {
                Ok(r) => {
                    if model.as_deref().is_some_and(crate::llm::cli::is_cli_model) {
                        let text = r
                            .result
                            .as_ref()
                            .and_then(|v| v["text"].as_str())
                            .unwrap_or(&r.output);
                        if !text.trim().is_empty() {
                            let _ = tx.send(LocalMsg::SubagentEvent(json!({
                                "ev": "subagent.stream",
                                "id": ui_id,
                                "role": role,
                                "child": {"ev": "model.delta", "text": text},
                            })));
                        }
                    }
                    let _ = tx.send(LocalMsg::SubagentEvent(json!({
                        "ev": "subagent.completed",
                        "id": ui_id,
                        "sid": r.sid,
                        "model": model,
                        "role": role,
                        "ok": r.exit_code == Some(0) && !r.timed_out,
                        "elapsed_secs": elapsed_secs,
                        "patch_artifact": r.patch_artifact,
                        "diff_stat": r.diff_stat,
                    })));
                }
                Err(e) => {
                    let _ = tx.send(LocalMsg::SubagentEvent(json!({
                        "ev": "subagent.completed",
                        "id": ui_id,
                        "sid": child_sid,
                        "model": model,
                        "role": role,
                        "ok": false,
                        "elapsed_secs": elapsed_secs,
                        "error": e.to_string(),
                    })));
                }
            }
        });
    }
}

/// Format turn RPC parameters including dynamic `@diff`/`@staged`/`@last` context
/// expansions and any pending or inline `@image` attachments.
pub fn build_turn_params(
    repo: &str,
    mut text: String,
    mut images: Vec<(String, String)>,
    last_shell: Option<&(String, String, Option<i32>)>,
) -> (Value, Vec<String>) {
    images.extend(extract_inline_images(repo, &text));
    let mut context_blocks = Vec::new();
    let has_tok = |want: &str| {
        text.split_whitespace()
            .any(|t| t.trim_end_matches([',', '.', ';', ')', ']']) == want)
    };
    if has_tok("@diff") {
        if let Ok(d) = Command::new("git").args(["-C", repo, "diff"]).output() {
            let body: String = String::from_utf8_lossy(&d.stdout).chars().take(24_000).collect();
            if !body.trim().is_empty() {
                context_blocks.push(format!("<context source=\"@diff\">\n{body}\n</context>"));
            }
        }
    }
    if has_tok("@staged") {
        if let Ok(d) = Command::new("git")
            .args(["-C", repo, "diff", "--cached"])
            .output()
        {
            let body: String = String::from_utf8_lossy(&d.stdout).chars().take(24_000).collect();
            if !body.trim().is_empty() {
                context_blocks.push(format!("<context source=\"@staged\">\n{body}\n</context>"));
            }
        }
    }
    if has_tok("@last") {
        if let Some((cmd, out, code)) = last_shell {
            let body: String = out.chars().take(16_000).collect();
            context_blocks.push(format!(
                "<context source=\"@last\" command={cmd:?} exit_code={code:?}>\n{body}\n</context>"
            ));
        }
    }
    context_blocks.extend(extract_inline_file_slices(repo, &text));
    if !context_blocks.is_empty() {
        text = format!("{text}\n\n{}", context_blocks.join("\n\n"));
    }
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

/// Parse `@path/to/file.rs#L10-50` or `@path/to/file.rs:10-50` inline file-slice mentions
/// safely confined within `repo` so line-range references inject exact numbered excerpts!
fn extract_inline_file_slices(repo: &str, text: &str) -> Vec<String> {
    let Ok(canon_repo) = Path::new(repo).canonicalize() else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for token in text.split_whitespace() {
        let Some(raw_ref) = token.strip_prefix('@') else {
            continue;
        };
        let cleaned = raw_ref.trim_end_matches([',', '.', ';', ')', ']']);
        if matches!(cleaned, "diff" | "staged" | "last" | "") {
            continue;
        }
        let (rel_path, range_opt) = parse_path_and_line_range(cleaned);
        // Only auto-inline explicit line slices (`#L10-30` / `:10-30`) so plain `@file` keeps lightweight path semantics unless sliced.
        let Some((start_l, end_l)) = range_opt else {
            continue;
        };
        if !seen.insert(cleaned.to_owned()) {
            continue;
        }
        let pb = Path::new(rel_path);
        if pb.is_absolute() || image_mime_for_path(pb).is_some() {
            continue;
        }
        let Ok(canon_file) = canon_repo.join(pb).canonicalize() else {
            continue;
        };
        if !canon_file.starts_with(&canon_repo) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&canon_file) else {
            continue;
        };
        let all_lines: Vec<&str> = body.lines().collect();
        if all_lines.is_empty() {
            continue;
        }
        let lo = start_l.max(1).min(all_lines.len());
        let hi = end_l.max(lo).min(all_lines.len()).min(lo + 299);
        let numbered: String = all_lines[lo - 1..hi]
            .iter()
            .enumerate()
            .map(|(off, l)| format!("{:>4} | {l}", lo + off))
            .collect::<Vec<_>>()
            .join("\n");
        let clipped: String = numbered.chars().take(16_000).collect();
        blocks.push(format!(
            "<context source=\"@{rel_path}#L{lo}-{hi}\">\n{clipped}\n</context>"
        ));
    }
    blocks
}

fn parse_path_and_line_range(spec: &str) -> (&str, Option<(usize, usize)>) {
    let (path, tail) = if let Some((p, t)) = spec.split_once("#L") {
        (p, t)
    } else if let Some((p, t)) = spec.split_once("#l") {
        (p, t)
    } else if let Some((p, t)) = spec.rsplit_once(':') {
        if t.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            (p, t)
        } else {
            return (spec, None);
        }
    } else {
        return (spec, None);
    };
    let tail = tail.trim_start_matches(['L', 'l']);
    if let Some((a, b)) = tail.split_once('-') {
        let b_clean = b.trim_start_matches(['L', 'l']);
        if let (Ok(s), Ok(e)) = (a.parse::<usize>(), b_clean.parse::<usize>()) {
            return (path, Some((s, e)));
        }
    } else if let Ok(single) = tail.parse::<usize>() {
        return (path, Some((single, single)));
    }
    (spec, None)
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
    rx: Option<Receiver<ServerMsg>>,
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
            rx: Some(rx),
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
        self.rx.as_ref()?.try_recv().ok()
    }

    pub fn shutdown(mut self, cancel_active: bool) {
        if cancel_active {
            let _ = self.send("cancel", json!({}));
        }
        drop(self.stdin.take());
        let _ = self.child.wait();
    }

    pub fn run_plain(mut self) -> Result<(), String> {
        let rx = self.rx.take().ok_or("missing server receiver")?;
        let reader = std::thread::spawn(move || {
            let terminal = std::io::stdout().is_terminal();
            for msg in rx {
                let mut out = std::io::stdout().lock();
                match msg {
                    ServerMsg::Event(v) => match v["ev"].as_str() {
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
                        _ => {}
                    },
                    ServerMsg::Error { message, .. } => {
                        let _ = writeln!(out, "\nError: {message}");
                        if terminal {
                            let _ = write!(out, "\n> ");
                        }
                    }
                    ServerMsg::Reply { result, .. } => {
                        if result["done"].is_boolean() {
                            let _ = writeln!(
                                out,
                                "\n[{}]",
                                result["reason"].as_str().unwrap_or("done")
                            );
                        }
                        if terminal {
                            let _ = write!(out, "\n> ");
                        }
                    }
                    ServerMsg::Stderr(line) => {
                        let _ = writeln!(std::io::stderr(), "{line}");
                    }
                    ServerMsg::Exited => break,
                }
                let _ = out.flush();
            }
        });
        println!("Twig · magpie\nEnter a task. During a run use /steer TEXT, /cancel, /approve ID, /deny ID. /quit exits.");
        for line in std::io::stdin().lock().lines() {
            let text = line.map_err(|e| e.to_string())?;
            if text.trim().is_empty() {
                continue;
            }
            if text == "/quit" {
                break;
            } else if text == "/cancel" {
                self.send("cancel", json!({}))?;
            } else if let Some(t) = text.strip_prefix("/steer ") {
                self.send("steer", json!({"input": t}))?;
            } else if let Some(t) = text.strip_prefix("/approve ") {
                self.send("approval.resolve", json!({"id": t.trim(), "allow": true}))?;
            } else if let Some(t) = text.strip_prefix("/deny ") {
                self.send("approval.resolve", json!({"id": t.trim(), "allow": false}))?;
            } else {
                self.send("turn", json!({"input": text}))?;
            }
        }
        self.shutdown(std::io::stdin().is_terminal());
        let _ = reader.join();
        Ok(())
    }
}

/// Plain line-delimited fallback for piped / non-TTY sessions or `--plain`, built on `ServeClient`.
pub fn chat_plain(repo: &str, resume: Option<&str>) -> Result<(), String> {
    ServeClient::spawn(repo, resume)?.run_plain()
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

        let shell_snap = (
            "cargo test".to_owned(),
            "panicked at foo.rs:9".to_owned(),
            Some(101),
        );
        let (params, _) = build_turn_params(
            ".",
            "fix @last please".into(),
            Vec::new(),
            Some(&shell_snap),
        );
        let expanded = params["input"].as_str().unwrap();
        assert!(expanded.contains("source=\"@last\"") && expanded.contains("panicked at foo.rs:9"));

        let img_base = std::env::temp_dir().join(format!("twig-img-confine-{}", std::process::id()));
        let repo_dir = img_base.join("repo");
        let outside_dir = img_base.join("outside");
        std::fs::create_dir_all(&repo_dir).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();
        let valid_png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        std::fs::write(repo_dir.join("ok.png"), valid_png).unwrap();
        std::fs::write(repo_dir.join("fake.png"), b"-----BEGIN PRIVATE KEY-----").unwrap();
        std::fs::write(outside_dir.join("escape.png"), valid_png).unwrap();

        assert!(load_image_data_url(repo_dir.to_str().unwrap(), "fake.png").is_err());
        let extracted = extract_inline_images(
            repo_dir.to_str().unwrap(),
            "inspect @ok.png @fake.png @../outside/escape.png",
        );
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].0, "ok.png");
        let _ = std::fs::remove_dir_all(&img_base);
    }
}
