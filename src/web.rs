//! Built-in zero-dependency Web & Mobile Cockpit (`twig web`) synthesizing the strongest
//! workflows from T3 Code, `bb` (`getbb.app`), Orca, Happy Coder, and Vibe Kanban:
//! - 3-pane quantitative `4px`-grid UI (`Inbox` + `Thread / File-Indexed Diff` + `Environment Inspector`)
//! - Isolated Git worktree creation on new threads, model/effort/role selectors, image paste/drop,
//!   live `Stop` (`/api/cancel`), subagent thread tree navigation, and opt-in browser notifications
//! - Hardened against localhost cross-origin CSRF (`Content-Type: application/json` + `Origin` check),
//!   SVG XSS (`<img src="data:image/svg+xml;base64,...">`), and non-loopback unauthorized access (`Bearer`).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::automation::{Automation, AutomationConfig, ShippingPermissions, SourceKind, WorkItem};
use crate::config::{paths, HarnessConfig};
use crate::exec::{run_process, shell_command};
use crate::llm::{cli, ConfigOp, Llm, Provider};
use crate::runtime::guidance::SkillStore;
use crate::runtime::mcp::McpRegistry;
use crate::runtime::policy::{Policy, Tier};
use crate::runtime::session::{archive_session, scan_sessions};
use crate::runtime::workflows::build_goal_prompt;
use crate::runtime::Daemon;

const COCKPIT_HTML: &str = include_str!("web_ui.html");

struct ActiveRun {
    pid: u32,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

struct WebState {
    default_repo: PathBuf,
    token: Option<String>,
    runs: Mutex<HashMap<String, ActiveRun>>,
    sse_clients: Mutex<Vec<SyncSender<String>>>,
    automation_running: AtomicBool,
}

/// Start the `twig web` HTTP cockpit server (`twig web [--port 3426] [--bind 127.0.0.1] [--repo .] [--token T]`).
pub fn serve_web(args: &[String]) -> Result<(), String> {
    let mut bind_host = String::from("127.0.0.1");
    let mut port: u16 = 3426;
    let mut repo = String::from(".");
    let mut explicit_token: Option<String> = None;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--port" | "-p" => {
                i += 1;
                port = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or("invalid --port")?;
            }
            "--bind" | "-b" => {
                i += 1;
                bind_host = args.get(i).cloned().ok_or("--bind requires host")?;
            }
            "--repo" | "-C" => {
                i += 1;
                repo = args.get(i).cloned().ok_or("--repo requires path")?;
            }
            "--token" => {
                i += 1;
                explicit_token = Some(args.get(i).cloned().ok_or("--token requires value")?);
            }
            _ => {}
        }
        i += 1;
    }

    let addr: SocketAddr = format!("{bind_host}:{port}")
        .parse()
        .map_err(|e| format!("invalid bind address {bind_host}:{port}: {e}"))?;
    let is_loopback = match addr.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    };
    let token = if let Some(t) = explicit_token {
        Some(t)
    } else if !is_loopback {
        let seed = format!(
            "{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        Some(blake3::hash(seed.as_bytes()).to_hex()[..24].to_string())
    } else {
        None
    };

    let listener = TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
    let default_repo = Path::new(&repo)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&repo));

    println!("twig web cockpit listening on http://{addr}");
    if let Some(ref tok) = token {
        println!("auth token (required for non-loopback access): {tok}");
        println!("mobile/remote URL: http://{addr}/?token={tok}");
    }

    let state = Arc::new(WebState {
        default_repo,
        token,
        runs: Mutex::new(HashMap::new()),
        sse_clients: Mutex::new(Vec::new()),
        automation_running: AtomicBool::new(false),
    });

    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        let st = state.clone();
        std::thread::spawn(move || {
            let _ = handle_conn(stream, &st);
        });
    }
    Ok(())
}

fn broadcast_sse(state: &WebState, ev: &Value) {
    let Ok(mut clients) = state.sse_clients.lock() else {
        return;
    };
    if clients.is_empty() {
        return;
    }
    let payload = ev.to_string();
    clients.retain(|tx| !matches!(tx.try_send(payload.clone()), Err(TrySendError::Disconnected(_))));
}

fn serve_sse_stream(mut stream: TcpStream, state: &Arc<WebState>) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n: connected\n\n"
    )?;
    stream.flush()?;
    let (tx, rx) = mpsc::sync_channel::<String>(256);
    if let Ok(mut clients) = state.sse_clients.lock() {
        clients.push(tx);
    }
    loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(msg) => {
                if write!(stream, "data: {msg}\n\n").is_err() || stream.flush().is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if write!(stream, ": ping\n\n").is_err() || stream.flush().is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    if origin.is_empty() {
        return true;
    }
    let Some((_, after_scheme)) = origin.split_once("://") else {
        return false;
    };
    let origin_host = after_scheme.split('/').next().unwrap_or("");
    !host.is_empty() && origin_host.eq_ignore_ascii_case(host)
}

fn handle_conn(mut stream: TcpStream, state: &Arc<WebState>) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut req_line = String::new();
    if reader.read_line(&mut req_line)? == 0 {
        return Ok(());
    }
    let parts: Vec<&str> = req_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }
    let method = parts[0];
    let raw_target = parts[1];
    let (path, query) = match raw_target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_target, ""),
    };

    let mut content_len = 0usize;
    let mut auth_header = String::new();
    let mut content_type = String::new();
    let mut origin_header = String::new();
    let mut host_header = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let val = v.trim();
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_len = val.parse::<usize>().unwrap_or(0).min(8_388_608),
                "authorization" => auth_header = val.to_owned(),
                "content-type" => content_type = val.to_ascii_lowercase(),
                "origin" => origin_header = val.to_owned(),
                "host" => host_header = val.to_owned(),
                _ => {}
            }
        }
    }

    let mut body_bytes = vec![0u8; content_len];
    if content_len > 0 {
        reader.read_exact(&mut body_bytes)?;
    }

    if path == "/" || path == "/index.html" {
        return write_http(&mut stream, 200, "text/html; charset=utf-8", COCKPIT_HTML.as_bytes());
    }

    if let Some(ref expected) = state.token {
        let want = format!("Bearer {expected}");
        let query_ok = method == "GET"
            && path == "/api/events"
            && query
                .split('&')
                .find_map(|kv| kv.strip_prefix("token="))
                .map(url_decode_simple)
                .as_deref()
                == Some(expected.as_str());
        if auth_header != want && !query_ok {
            let err = json!({"error": "unauthorized — pass Bearer token"}).to_string();
            return write_http(&mut stream, 401, "application/json", err.as_bytes());
        }
    }

    if method == "GET" && path == "/api/events" {
        return serve_sse_stream(stream, state);
    }

    // Block cross-origin browser CSRF against loopback `POST` endpoints (e.g. `<form enctype="text/plain">`).
    if method == "POST" {
        if !content_type.starts_with("application/json") {
            let err = json!({"error": "Content-Type: application/json required"}).to_string();
            return write_http(&mut stream, 400, "application/json", err.as_bytes());
        }
        if !origin_matches_host(&origin_header, &host_header) {
            let err = json!({"error": "cross-origin POST rejected"}).to_string();
            return write_http(&mut stream, 403, "application/json", err.as_bytes());
        }
    }

    let resp_json: Result<Value, String> = match (method, path) {
        ("GET", "/api/meta") => Ok(collect_meta(state)),
        ("GET", "/api/files") => Ok(list_worktree_files(query, &state.default_repo)),
        ("GET", "/api/settings") => collect_settings(query, &state.default_repo),
        ("GET", "/api/automations") => Ok(collect_automations(query, state)),
        ("GET", "/api/sessions") => {
            let all = query.split('&').any(|kv| kv == "all=1");
            let running: std::collections::HashSet<String> = state
                .runs
                .lock()
                .map(|g| g.keys().cloned().collect())
                .unwrap_or_default();
            let filter = (!all).then_some(state.default_repo.as_path());
            let mut items = scan_sessions(filter, 50);
            if items.is_empty() && !all {
                items = scan_sessions(None, 50);
            }
            for s in &mut items {
                if running.contains(&s.sid) {
                    s.status = "running".to_owned();
                }
            }
            Ok(json!({
                "default_repo": state.default_repo.display().to_string(),
                "sessions": items,
            }))
        }
        ("GET", p) if p.starts_with("/api/session/") => {
            let sid = p.trim_start_matches("/api/session/").trim();
            inspect_session_detail(sid, state)
        }
        ("POST", "/api/send") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| dispatch_background_turn(&req, state)),
        ("POST", "/api/steer") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| steer_running_turn(&req, state)),
        ("POST", "/api/cancel") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| cancel_running_turn(&req, state)),
        ("POST", "/api/archive") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| {
                let sid = req["sid"].as_str().ok_or("sid required")?;
                let dst = archive_session(sid)?;
                Ok(json!({"archived": true, "path": dst}))
            }),
        ("POST", "/api/approve") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| resolve_session_approval(&req, &state.default_repo)),
        ("POST", "/api/commit") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| commit_session_worktree(&req, &state.default_repo)),
        ("POST", "/api/rpc") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| dispatch_session_rpc(&req, &state.default_repo)),
        ("POST", "/api/shell") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| run_quick_shell(&req, &state.default_repo)),
        ("POST", "/api/settings") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| apply_settings(&req, &state.default_repo)),
        ("POST", "/api/automations") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| apply_automations(&req, state)),
        _ => Err(format!("not found: {method} {path}")),
    };

    match resp_json {
        Ok(v) => write_http(&mut stream, 200, "application/json", v.to_string().as_bytes()),
        Err(e) => write_http(
            &mut stream,
            400,
            "application/json",
            json!({"error": e}).to_string().as_bytes(),
        ),
    }
}

fn write_http(
    stream: &mut TcpStream,
    code: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let status = match code {
        200 => "200 OK",
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        403 => "403 Forbidden",
        _ => "404 Not Found",
    };
    let csp = if content_type.starts_with("text/html") {
        "Content-Security-Policy: default-src 'self'; img-src 'self' data:; frame-src 'self' data: blob:; style-src 'unsafe-inline'; script-src 'unsafe-inline'; object-src 'none'; base-uri 'none'\r\n"
    } else {
        ""
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{csp}Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn collect_meta(state: &Arc<WebState>) -> Value {
    let provider = Provider::new();
    let active_model = provider.roles().get("code").to_owned();
    let mut models = cli::detected_runners();
    if let Ok(gw) = provider.list_models() {
        for m in gw {
            if !models.contains(&m) {
                models.push(m);
            }
        }
    }
    if !models.contains(&active_model) {
        models.insert(0, active_model.clone());
    }
    let branch = Command::new("git")
        .arg("-C")
        .arg(&state.default_repo)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let skills: Vec<Value> = SkillStore::default_for(Some(&state.default_repo))
        .catalog_for_autocomplete()
        .into_iter()
        .take(80)
        .map(|(name, scope, desc)| json!({"name": name, "scope": scope, "desc": desc}))
        .collect();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "default_repo": state.default_repo.display().to_string(),
        "branch": branch,
        "active_model": active_model,
        "effort": provider.effort(),
        "models": models,
        "skills": skills,
    })
}

fn url_decode_simple(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn list_worktree_files(query: &str, default_repo: &Path) -> Value {
    let wt = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("wt="))
        .map(url_decode_simple)
        .filter(|s| !s.is_empty() && Path::new(s).is_dir())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_repo.to_path_buf());
    let files: Vec<String> = Command::new("git")
        .arg("-C")
        .arg(&wt)
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty() && !crate::runtime::policy::is_sensitive_secret_path(l))
                .take(800)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    json!({ "files": files })
}

fn inspect_session_detail(sid: &str, state: &Arc<WebState>) -> Result<Value, String> {
    let in_flight = state
        .runs
        .lock()
        .map(|g| g.contains_key(sid))
        .unwrap_or(false);
    crate::runtime::session::inspect_session_detail(sid, &state.default_repo, in_flight)
}

fn provision_isolated_worktree(base_repo: &Path, raw_slug: &str) -> Result<PathBuf, String> {
    let slug: String = raw_slug
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(32)
        .collect();
    if slug.is_empty() {
        return Err("invalid worktree slug".into());
    }
    let repo_name = base_repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let wt_root = base_repo
        .parent()
        .map(|p| p.join(".twig-worktrees"))
        .unwrap_or_else(|| paths::state_dir().join("worktrees"));
    std::fs::create_dir_all(&wt_root).map_err(|e| format!("create worktree dir: {e}"))?;
    let dst = wt_root.join(format!("{repo_name}-{slug}"));
    if !dst.exists() {
        let branch = format!("twig/{slug}");
        let add = Command::new("git")
            .arg("-C")
            .arg(base_repo)
            .args(["worktree", "add", "-b", &branch])
            .arg(&dst)
            .output()
            .map_err(|e| format!("git worktree add: {e}"))?;
        if !add.status.success() {
            let fallback = Command::new("git")
                .arg("-C")
                .arg(base_repo)
                .args(["worktree", "add"])
                .arg(&dst)
                .arg(&branch)
                .output()
                .map_err(|e| format!("git worktree add existing: {e}"))?;
            if !fallback.status.success() {
                return Err(format!(
                    "git worktree add failed: {}",
                    String::from_utf8_lossy(&fallback.stderr).trim()
                ));
            }
        }
    }
    Ok(dst.canonicalize().unwrap_or(dst))
}

fn decode_b64(input: &str) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let a = val(*chunk.first()?)?;
        let b = val(*chunk.get(1)?)?;
        out.push((a << 2) | (b >> 4));
        if let Some(&c_raw) = chunk.get(2) {
            let c = val(c_raw)?;
            out.push((b << 4) | (c >> 2));
            if let Some(&d_raw) = chunk.get(3) {
                let d = val(d_raw)?;
                out.push((c << 6) | d);
            }
        }
    }
    Some(out)
}

fn stage_data_images(sid: &str, imgs: &[Value]) -> Vec<String> {
    let dir = paths::state_dir().join("web-uploads");
    let _ = std::fs::create_dir_all(&dir);
    let mut refs = Vec::new();
    for (idx, item) in imgs.iter().take(4).enumerate() {
        let Some(url) = item["url"].as_str() else { continue };
        let Some((hdr, b64)) = url.split_once(";base64,") else { continue };
        let ext = if hdr.contains("jpeg") || hdr.contains("jpg") {
            "jpg"
        } else if hdr.contains("webp") {
            "webp"
        } else if hdr.contains("gif") {
            "gif"
        } else {
            "png"
        };
        let Some(bytes) = decode_b64(b64) else { continue };
        if bytes.is_empty() || bytes.len() > 8 * 1024 * 1024 {
            continue;
        }
        let path = dir.join(format!("{sid}-{idx}-{}.{ext}", std::process::id()));
        if std::fs::write(&path, bytes).is_ok() {
            refs.push(format!("@{}", path.display()));
        }
    }
    refs
}

fn dispatch_background_turn(req: &Value, state: &Arc<WebState>) -> Result<Value, String> {
    let raw_prompt = req["prompt"].as_str().map(str::trim).unwrap_or("");
    let imgs = req["images"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if raw_prompt.is_empty() && imgs.is_empty() {
        return Err("prompt required".into());
    }
    let sid = req["sid"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(paths::session_id);
    if !paths::valid_session(&sid) {
        return Err("invalid session id".into());
    }
    let mut prompt = if let Some(goal) = raw_prompt.strip_prefix("/goal ") {
        build_goal_prompt(goal.trim())
    } else if raw_prompt.is_empty() {
        "Inspect the attached image(s).".to_owned()
    } else {
        raw_prompt.to_owned()
    };
    let model = req["model"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let effective_cli = model
        .as_deref()
        .is_some_and(|m| m.starts_with("cli:"))
        || (model.is_none() && Provider::new().roles().get("code").starts_with("cli:"));
    if !imgs.is_empty() && effective_cli {
        let staged = stage_data_images(&sid, &imgs);
        if !staged.is_empty() {
            prompt = format!("{} {prompt}", staged.join(" "));
        }
    }
    let mut repo = req["worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| state.default_repo.display().to_string());
    if let Some(slug) = req["new_worktree_slug"].as_str().filter(|s| !s.trim().is_empty()) {
        let wt = provision_isolated_worktree(Path::new(&repo), slug)?;
        repo = wt.display().to_string();
    }
    let role = req["role"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let effort = req["effort"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;

    {
        let g = state.runs.lock().map_err(|e| e.to_string())?;
        if g.contains_key(&sid) {
            return Err("session is already running a turn".into());
        }
    }

    let runs_dir = paths::state_dir().join("sessions");
    let mut cmd = Command::new(exe);
    cmd.args([
        "serve",
        &repo,
        &runs_dir.display().to_string(),
        &sid,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn twig serve: {e}"))?;
    let pid = child.id();
    let mut stdin_pipe = child.stdin.take().ok_or("missing child stdin")?;
    let stdout_pipe = child.stdout.take().ok_or("missing child stdout")?;

    if let Some(ref m) = model {
        let _ = writeln!(
            stdin_pipe,
            "{}",
            json!({"id": "cfg-m", "method": "model.set", "params": {"model": m}})
        );
    }
    if let Some(ref e) = effort {
        let _ = writeln!(
            stdin_pipe,
            "{}",
            json!({"id": "cfg-e", "method": "effort.set", "params": {"effort": e}})
        );
    }
    if let Some(ref r) = role {
        let is_ro = matches!(r.as_str(), "plan" | "advisor" | "scout" | "reviewer");
        let _ = writeln!(
            stdin_pipe,
            "{}",
            json!({"id": "cfg-p", "method": "mode.set", "params": {"plan": is_ro}})
        );
    }
    let turn_req = json!({
        "id": "turn-1",
        "method": "turn",
        "params": {
            "input": prompt,
            "role": role.as_deref().unwrap_or("code"),
            "images": imgs,
        }
    });
    writeln!(stdin_pipe, "{turn_req}").map_err(|e| format!("write turn rpc: {e}"))?;
    let _ = stdin_pipe.flush();

    let stdin_arc = Arc::new(Mutex::new(Some(stdin_pipe)));
    if let Ok(mut g) = state.runs.lock() {
        g.insert(
            sid.clone(),
            ActiveRun {
                pid,
                stdin: stdin_arc.clone(),
            },
        );
    }
    broadcast_sse(state, &json!({"ev": "session.started", "sid": sid}));

    let st = state.clone();
    let sid_bg = sid.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout_pipe).lines().map_while(Result::ok) {
            let Ok(mut v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("ev").is_some() {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("sid".into(), json!(sid_bg));
                }
                broadcast_sse(&st, &v);
            }
            if v.get("id") == Some(&json!("turn-1")) {
                if let Ok(mut guard) = stdin_arc.lock() {
                    *guard = None;
                }
                broadcast_sse(
                    &st,
                    &json!({
                        "ev": "session.idle",
                        "sid": sid_bg,
                        "error": v.get("error").cloned().unwrap_or(Value::Null),
                    }),
                );
                break;
            }
        }
        if let Ok(mut guard) = stdin_arc.lock() {
            *guard = None;
        }
        let _ = child.wait();
        if let Ok(mut g) = st.runs.lock() {
            g.remove(&sid_bg);
        }
        broadcast_sse(&st, &json!({"ev": "session.settled", "sid": sid_bg}));
    });

    Ok(json!({"sid": sid, "worktree": repo, "queued": true}))
}

fn steer_running_turn(req: &Value, state: &Arc<WebState>) -> Result<Value, String> {
    let sid = req["sid"].as_str().ok_or("sid required")?;
    let text = req["input"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("steer input required")?;
    let stdin_arc = {
        let g = state.runs.lock().map_err(|e| e.to_string())?;
        g.get(sid).map(|r| r.stdin.clone())
    };
    let Some(stdin_arc) = stdin_arc else {
        return Ok(json!({"queued": false, "idle": true}));
    };
    let mut guard = stdin_arc.lock().map_err(|e| e.to_string())?;
    let Some(ref mut pipe) = *guard else {
        return Ok(json!({"queued": false, "idle": true}));
    };
    writeln!(
        pipe,
        "{}",
        json!({"id": "steer", "method": "steer", "params": {"input": text}})
    )
    .map_err(|e| format!("steer write: {e}"))?;
    let _ = pipe.flush();
    drop(guard);
    broadcast_sse(state, &json!({"ev": "steering.queued", "sid": sid, "text": text}));
    Ok(json!({"queued": true}))
}

fn cancel_running_turn(req: &Value, state: &Arc<WebState>) -> Result<Value, String> {
    let sid = req["sid"].as_str().ok_or("sid required")?;
    let run_opt = state
        .runs
        .lock()
        .map_err(|e| e.to_string())?
        .remove(sid);
    let Some(run) = run_opt else {
        return Ok(json!({"stopped": false}));
    };
    if let Ok(mut guard) = run.stdin.lock() {
        if let Some(ref mut pipe) = *guard {
            let _ = writeln!(pipe, "{}", json!({"id": "cancel", "method": "cancel", "params": {}}));
            let _ = pipe.flush();
        }
        *guard = None;
    }
    let pid = run.pid;
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-(pid as i32), libc::SIGINT);
        let _ = libc::kill(pid as i32, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output();
    }
    broadcast_sse(state, &json!({"ev": "session.settled", "sid": sid}));
    Ok(json!({"stopped": true, "pid": pid}))
}

fn resolve_session_approval(req: &Value, default_repo: &Path) -> Result<Value, String> {
    let sid = req["sid"].as_str().ok_or("sid required")?;
    let call_id = req["id"].as_str().ok_or("approval id required")?;
    let allow = req["allow"].as_bool().unwrap_or(false);
    let wt = req["worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_repo.to_path_buf());
    let (cfg, _) = HarnessConfig::resolve()?;
    let provider = Provider::new();
    let runs = paths::state_dir().join("sessions");
    let mut daemon = Daemon::new(&provider, &wt, &runs, sid, &cfg)?;
    daemon.dispatch(
        "approval.resolve",
        &json!({"id": call_id, "allow": allow}),
        &mut |_| {},
    )
}

fn commit_session_worktree(req: &Value, default_repo: &Path) -> Result<Value, String> {
    let sid = req["sid"].as_str().ok_or("sid required")?;
    let hint = req["hint"].as_str();
    let wt = req["worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_repo.to_path_buf());
    let (cfg, _) = HarnessConfig::resolve()?;
    let provider = Provider::new();
    let runs = paths::state_dir().join("sessions");
    let mut daemon = Daemon::new(&provider, &wt, &runs, sid, &cfg)?;
    daemon.dispatch("git.commit", &json!({"hint": hint}), &mut |_| {})
}

fn dispatch_session_rpc(req: &Value, default_repo: &Path) -> Result<Value, String> {
    let method = req["method"].as_str().ok_or("method required")?;
    if !matches!(
        method,
        "session.rename"
            | "session.btw"
            | "session.fork"
            | "session.compact"
            | "session.rewind"
            | "session.apply"
            | "git.pr"
            | "git.worktree"
    ) {
        return Err(format!("unsupported web rpc method {method:?}"));
    }
    let sid = req["sid"].as_str().filter(|s| !s.is_empty()).ok_or("sid required")?;
    let wt = req["worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_repo.to_path_buf());
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let (cfg, _) = HarnessConfig::resolve()?;
    let provider = Provider::new();
    let runs = paths::state_dir().join("sessions");
    let mut daemon = Daemon::new(&provider, &wt, &runs, sid, &cfg)?;
    daemon.dispatch(method, &params, &mut |_| {})
}

fn run_quick_shell(req: &Value, default_repo: &Path) -> Result<Value, String> {
    let command = req["command"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("command required")?;
    let tier = Policy::new().classify(command);
    if tier == Tier::Deny {
        return Err(
            "refused by policy: destructive, secret-reading, or control-plane command requires interactive TUI approval"
                .into(),
        );
    }
    let wt = req["worktree"]
        .as_str()
        .filter(|s| !s.is_empty() && Path::new(s).is_dir())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_repo.to_path_buf());
    let mut cmd = shell_command(command);
    cmd.current_dir(&wt);
    let out = run_process(&mut cmd, &[], Duration::from_secs(20), 65_536)
        .map_err(|e| format!("shell exec: {e}"))?;
    Ok(json!({
        "exit_code": out.exit_code.unwrap_or(-1),
        "timed_out": out.timed_out,
        "tier": tier.as_str(),
        "stdout": String::from_utf8_lossy(&out.stdout),
        "stderr": String::from_utf8_lossy(&out.stderr),
    }))
}

fn read_mcp_entries(wt: &Path) -> Vec<Value> {
    let mut merged: std::collections::BTreeMap<String, (Value, &'static str)> =
        std::collections::BTreeMap::new();
    for (p, scope) in [
        (paths::config_dir().join("mcp.json"), "global"),
        (wt.join(".twig/mcp.json"), "project"),
    ] {
        let Ok(raw) = std::fs::read_to_string(p) else { continue };
        let Ok(val) = serde_json::from_str::<Value>(&raw) else { continue };
        if let Some(obj) = val.get("mcpServers").and_then(Value::as_object) {
            for (k, v) in obj {
                merged.insert(k.clone(), (v.clone(), scope));
            }
        }
    }
    merged
        .into_iter()
        .map(|(name, (v, scope))| {
            let args: Vec<String> = v
                .get("args")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            json!({
                "name": name,
                "command": v.get("command").and_then(Value::as_str).unwrap_or(""),
                "args": args,
                "disabled": v.get("disabled").and_then(Value::as_bool).unwrap_or(false),
                "scope": scope,
            })
        })
        .collect()
}

fn collect_settings(query: &str, default_repo: &Path) -> Result<Value, String> {
    let wt = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("wt="))
        .map(url_decode_simple)
        .filter(|s| !s.is_empty() && Path::new(s).is_dir())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_repo.to_path_buf());
    let provider = Provider::new();
    let roles_rep = Provider::apply_config(ConfigOp::RolesReport)?;
    let (harness, harness_src) = HarnessConfig::resolve()?;
    let store = SkillStore::default_for(Some(&wt));
    let (active_cnt, vault_cnt) = store.counts();
    let (skills_ok, _) = store.doctor_report();
    let skills: Vec<Value> = store
        .catalog_for_autocomplete()
        .into_iter()
        .map(|(name, scope, desc)| json!({"name": name, "scope": scope, "desc": desc}))
        .collect();

    let proj_cfg_raw = std::fs::read_to_string(wt.join(".twig/project.json")).unwrap_or_default();
    let proj_cfg: Value = serde_json::from_str(&proj_cfg_raw).unwrap_or_else(|_| json!({}));
    let check_cmd = proj_cfg
        .get("check")
        .and_then(|c| c.as_str().or_else(|| c.get("command").and_then(Value::as_str)))
        .unwrap_or("")
        .to_owned();
    let post_edit_cmd = proj_cfg
        .get("post_edit_check")
        .and_then(|c| c.as_str().or_else(|| c.get("command").and_then(Value::as_str)))
        .unwrap_or("")
        .to_owned();

    let global_mem =
        std::fs::read_to_string(paths::config_dir().join("preferences.md")).unwrap_or_default();
    let proj_mem = std::fs::read_to_string(wt.join(".twig/preferences.md")).unwrap_or_default();

    Ok(json!({
        "gateway_base": provider.base(),
        "cli_runners": cli::detected_runners(),
        "roles": roles_rep.get("roles").cloned().unwrap_or_else(|| json!([])),
        "harness": harness,
        "harness_source": harness_src,
        "hooks": {
            "check": check_cmd,
            "post_edit_check": post_edit_cmd,
        },
        "skills": {
            "active_count": active_cnt,
            "vault_count": vault_cnt,
            "healthy": skills_ok,
            "items": skills,
        },
        "mcp": read_mcp_entries(&wt),
        "memory": {
            "global": global_mem,
            "project": proj_mem,
        }
    }))
}

fn apply_settings(req: &Value, default_repo: &Path) -> Result<Value, String> {
    let section = req["section"].as_str().ok_or("section required")?;
    let wt = req["worktree"]
        .as_str()
        .filter(|s| !s.is_empty() && Path::new(s).is_dir())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_repo.to_path_buf());

    match section {
        "roles" => {
            if req["action"].as_str() == Some("reset") {
                return Provider::apply_config(ConfigOp::ResetRoles);
            }
            if let Some(items) = req["items"].as_array() {
                for it in items {
                    let Some(role) = it["role"].as_str() else { continue };
                    Provider::apply_config(ConfigOp::SetRole {
                        role,
                        model: it["model"].as_str(),
                        effort: it["effort"].as_str(),
                    })?;
                }
                return Provider::apply_config(ConfigOp::RolesReport);
            }
            let role = req["role"].as_str().ok_or("role required")?;
            Provider::apply_config(ConfigOp::SetRole {
                role,
                model: req.get("model").and_then(Value::as_str),
                effort: req.get("effort").and_then(Value::as_str),
            })
        }
        "harness" => {
            let (cur, _) = HarnessConfig::resolve()?;
            let next = cur.merged(req.get("patch").unwrap_or(&json!({})))?;
            let cfg_path = paths::config_path();
            if let Some(p) = cfg_path.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            next.save(&cfg_path).map_err(|e| e.to_string())?;

            if let Some(hooks) = req.get("hooks").and_then(Value::as_object) {
                let pj_path = wt.join(".twig/project.json");
                let mut pj: Value = std::fs::read_to_string(&pj_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_else(|| json!({}));
                if let Some(obj) = pj.as_object_mut() {
                    for k in ["check", "post_edit_check"] {
                        if let Some(v) = hooks.get(k).and_then(Value::as_str) {
                            let t = v.trim();
                            if t.is_empty() {
                                obj.remove(k);
                            } else {
                                obj.insert(k.to_owned(), Value::String(t.to_owned()));
                            }
                        }
                    }
                }
                if let Some(parent) = pj_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let body = serde_json::to_string_pretty(&pj).map_err(|e| e.to_string())?;
                std::fs::write(&pj_path, format!("{body}\n")).map_err(|e| e.to_string())?;
            }
            Ok(json!({"saved": true}))
        }
        "skill" => {
            let name = req["name"].as_str().ok_or("skill name required")?;
            let enable = req["enable"].as_bool().unwrap_or(true);
            let project = req["project"].as_bool().unwrap_or(false);
            let store = SkillStore::default_for(Some(&wt));
            let msg = if enable {
                store.enable(name, project)?
            } else {
                store.disable(name, project)?
            };
            Ok(json!({"ok": true, "summary": msg}))
        }
        "mcp" => {
            let action = req["action"].as_str().unwrap_or("enable");
            if matches!(action, "enable" | "disable" | "init_rea") {
                let msg = McpRegistry::configure_project_file(&wt, req)?;
                return Ok(json!({"ok": true, "summary": msg}));
            }
            let path = wt.join(".twig/mcp.json");
            let mut root: Value = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| json!({"mcpServers": {}}));
            if !root.get("mcpServers").is_some_and(Value::is_object) {
                root["mcpServers"] = json!({});
            }
            let servers = root["mcpServers"].as_object_mut().ok_or("invalid mcp.json")?;
            if action == "add" {
                let name = req["server"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("server name required")?;
                let cmd = req["command"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("command required")?;
                let args: Vec<String> = req["args"]
                    .as_str()
                    .unwrap_or("")
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect();
                servers.insert(name.to_owned(), json!({"command": cmd, "args": args}));
            } else if action == "remove" {
                let name = req["server"].as_str().ok_or("server name required")?;
                servers.remove(name);
            } else {
                return Err(format!("unknown mcp action {action:?}"));
            }
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?))
                .map_err(|e| e.to_string())?;
            Ok(json!({"ok": true, "summary": "Saved .twig/mcp.json"}))
        }
        "memory" => {
            if let Some(g) = req.get("global").and_then(Value::as_str) {
                let gp = paths::config_dir().join("preferences.md");
                if let Some(p) = gp.parent() {
                    std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                std::fs::write(&gp, g).map_err(|e| e.to_string())?;
            }
            if let Some(pr) = req.get("project").and_then(Value::as_str) {
                let pp = wt.join(".twig/preferences.md");
                if let Some(p) = pp.parent() {
                    std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                std::fs::write(&pp, pr).map_err(|e| e.to_string())?;
            }
            Ok(json!({"saved": true}))
        }
        other => Err(format!("unknown settings section {other:?}")),
    }
}

fn infer_github_repo(wt: &Path) -> String {
    let raw = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let tail = raw
        .split("github.com")
        .nth(1)
        .unwrap_or("")
        .trim_start_matches([':', '/'])
        .trim_end_matches(".git");
    if tail.split('/').filter(|s| !s.is_empty()).count() == 2 {
        tail.to_owned()
    } else {
        String::new()
    }
}

fn resolve_automation_cfg_path(wt: &Path) -> PathBuf {
    let dot_path = wt.join(".twig/automation.json");
    if dot_path.exists() {
        dot_path
    } else {
        let root_path = wt.join("automation.json");
        if root_path.exists() {
            root_path
        } else {
            dot_path
        }
    }
}

fn collect_automations(query: &str, state: &Arc<WebState>) -> Value {
    let wt = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("wt="))
        .map(url_decode_simple)
        .filter(|s| !s.is_empty() && Path::new(s).is_dir())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.default_repo.clone());
    let cfg_path = resolve_automation_cfg_path(&wt);
    let suggested_repo = infer_github_repo(&wt);
    let running = state.automation_running.load(Ordering::Relaxed);
    let Ok(raw) = std::fs::read_to_string(&cfg_path) else {
        return json!({
            "configured": false,
            "suggested_repo": suggested_repo,
            "running": running,
            "config_path": cfg_path.display().to_string(),
            "items": [],
        });
    };
    let mut cfg: AutomationConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "configured": false,
                "suggested_repo": suggested_repo,
                "running": running,
                "error": e.to_string(),
                "items": [],
            });
        }
    };
    if cfg.repository_path == Path::new(".") {
        cfg.repository_path = wt.clone();
    }
    if cfg.state_dir.is_relative() {
        cfg.state_dir = wt.join(&cfg.state_dir);
    }
    let items = Automation::open(cfg.clone())
        .and_then(|a| a.queue_snapshot())
        .unwrap_or_default();
    json!({
        "configured": true,
        "suggested_repo": suggested_repo,
        "running": running,
        "config_path": cfg_path.display().to_string(),
        "config": cfg,
        "items": items,
    })
}

fn apply_automations(req: &Value, state: &Arc<WebState>) -> Result<Value, String> {
    let action = req["action"].as_str().ok_or("action required")?;
    let wt = req["worktree"]
        .as_str()
        .filter(|s| !s.is_empty() && Path::new(s).is_dir())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.default_repo.clone());
    let cfg_path = resolve_automation_cfg_path(&wt);

    match action {
        "save_config" => {
            let create_pr = req["create_pr"].as_bool().unwrap_or(false);
            let cfg = AutomationConfig {
                repository: req["repository"]
                    .as_str()
                    .map(str::trim)
                    .unwrap_or("")
                    .to_owned(),
                repository_path: wt.clone(),
                label: req["label"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("twig")
                    .to_owned(),
                state_dir: paths::state_dir().join("automation-queue"),
                poll_interval_secs: req["poll_interval_secs"].as_u64().unwrap_or(300).max(30),
                include_pull_requests: req["include_pull_requests"].as_bool().unwrap_or(false),
                check_command: req["check_command"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("cargo test --quiet")
                    .to_owned(),
                check_timeout_secs: 600,
                lease_secs: 3600,
                permissions: ShippingPermissions {
                    replies: false,
                    push: create_pr,
                    create_pr,
                    merge: false,
                    release: false,
                },
                reply: None,
                create_pr,
                merge: false,
                release_tag: None,
                ..AutomationConfig::default()
            };
            cfg.validate()?;
            let _ = Automation::open(cfg.clone())?;
            if let Some(p) = cfg_path.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            let body = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
            std::fs::write(&cfg_path, format!("{body}\n")).map_err(|e| e.to_string())?;
            Ok(json!({"saved": true}))
        }
        "enqueue" => {
            let raw = std::fs::read_to_string(&cfg_path)
                .map_err(|_| "configure automation first".to_owned())?;
            let cfg: AutomationConfig = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            let number = req["number"].as_u64().filter(|&n| n > 0).unwrap_or_else(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    % 100_000
            });
            let title = req["title"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("task title required")?;
            let body = req["body"].as_str().unwrap_or("");
            let kind = if req["kind"].as_str() == Some("pr") {
                SourceKind::PullRequest
            } else {
                SourceKind::Issue
            };
            let url = req["url"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("https://github.com/{}/issues/{number}", cfg.repository));
            let item = WorkItem::new(&cfg.repository, kind, number, title, body, url);
            let mut auto = Automation::open(cfg)?;
            let added = auto.ingest(vec![item])?;
            broadcast_sse(state, &json!({"ev": "automation.updated"}));
            Ok(json!({"added": added}))
        }
        "poll_once" | "run_local" => {
            if state
                .automation_running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Err("automation poll/run is already active".into());
            }
            let raw = std::fs::read_to_string(&cfg_path)
                .map_err(|_| {
                    state.automation_running.store(false, Ordering::SeqCst);
                    "configure automation first".to_owned()
                })?;
            let cfg: AutomationConfig = serde_json::from_str(&raw).map_err(|e| {
                state.automation_running.store(false, Ordering::SeqCst);
                e.to_string()
            })?;
            let exe = std::env::current_exe().map_err(|e| {
                state.automation_running.store(false, Ordering::SeqCst);
                e.to_string()
            })?;
            let local_only = action == "run_local";
            let st = state.clone();
            std::thread::spawn(move || {
                let runs = paths::state_dir().join("automation-sessions");
                let timeout = Duration::from_secs(cfg.check_timeout_secs.min(3600));
                let mut exec = crate::automation::TwigExecutor::new(
                    exe,
                    runs,
                    cfg.check_command.clone(),
                    timeout,
                    timeout,
                );
                let res = Automation::open(cfg).and_then(|mut a| {
                    if local_only {
                        a.run_once(&mut exec)
                    } else {
                        a.poll_once(&mut exec)
                    }
                });
                st.automation_running.store(false, Ordering::SeqCst);
                let err = res.err();
                broadcast_sse(&st, &json!({"ev": "automation.updated", "error": err}));
            });
            Ok(json!({"started": true}))
        }
        "resolve_shipping" => {
            let id = req["id"].as_str().ok_or("item id required")?;
            let ship_act = req["shipping_action"].as_str().unwrap_or("create_pr");
            let completed = req["completed"].as_bool().unwrap_or(true);
            let raw = std::fs::read_to_string(&cfg_path).map_err(|e| e.to_string())?;
            let cfg: AutomationConfig = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            Automation::open(cfg)?.resolve_shipping(id, ship_act, completed)?;
            broadcast_sse(state, &json!({"ev": "automation.updated"}));
            Ok(json!({"resolved": true}))
        }
        other => Err(format!("unknown automation action {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_cockpit_enforces_bearer_token_csrf_origin_and_serves_ui() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(WebState {
            default_repo: PathBuf::from("."),
            token: Some("secret-tok".into()),
            runs: Mutex::new(HashMap::new()),
            sse_clients: Mutex::new(Vec::new()),
            automation_running: AtomicBool::new(false),
        });

        let server = std::thread::spawn(move || {
            for _ in 0..6 {
                if let Ok((stream, _)) = listener.accept() {
                    let _ = handle_conn(stream, &state);
                }
            }
        });

        // 1. GET / serves cockpit HTML with CSP and 3-pane features
        let mut c1 = TcpStream::connect(addr).unwrap();
        write!(c1, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut r1 = String::new();
        c1.read_to_string(&mut r1).unwrap();
        assert!(r1.contains("200 OK") && r1.contains("<title>twig</title>"));
        assert!(r1.contains("Content-Security-Policy:"));
        assert!(!COCKPIT_HTML.contains("approve('${a.id}'"));
        assert!(!COCKPIT_HTML.contains("openSid('${s.sid}'"));
        assert!(COCKPIT_HTML.contains("data:image/svg+xml;base64,"));
        assert!(COCKPIT_HTML.contains("Open in ") && COCKPIT_HTML.contains("What should we build"));

        // 2. GET /api/sessions without token is rejected with 401
        let mut c2 = TcpStream::connect(addr).unwrap();
        write!(c2, "GET /api/sessions HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut r2 = String::new();
        c2.read_to_string(&mut r2).unwrap();
        assert!(r2.contains("401 Unauthorized"));

        // 3. GET /api/sessions with valid Bearer token succeeds with 200
        let mut c3 = TcpStream::connect(addr).unwrap();
        write!(
            c3,
            "GET /api/sessions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer secret-tok\r\n\r\n"
        )
        .unwrap();
        let mut r3 = String::new();
        c3.read_to_string(&mut r3).unwrap();
        assert!(r3.contains("200 OK") && r3.contains("\"sessions\""));

        // 4. POST with text/plain (form CSRF) is rejected with 400
        let mut c4 = TcpStream::connect(addr).unwrap();
        c4.write_all(
            b"POST /api/cancel HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer secret-tok\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{}",
        )
        .unwrap();
        let mut r4 = String::new();
        c4.read_to_string(&mut r4).unwrap();
        assert!(r4.contains("400 Bad Request"));

        // 5. POST from foreign Origin is rejected with 403 Forbidden
        let mut c5 = TcpStream::connect(addr).unwrap();
        c5.write_all(
            b"POST /api/cancel HTTP/1.1\r\nHost: 127.0.0.1:3426\r\nOrigin: https://evil.example\r\nAuthorization: Bearer secret-tok\r\nContent-Type: application/json\r\nContent-Length: 12\r\n\r\n{\"sid\":\"s1\"}",
        )
        .unwrap();
        let mut r5 = String::new();
        c5.read_to_string(&mut r5).unwrap();
        assert!(r5.contains("403 Forbidden"));

        // 6. GET /api/events without token is rejected with 401
        let mut c6 = TcpStream::connect(addr).unwrap();
        write!(c6, "GET /api/events HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut r6 = String::new();
        c6.read_to_string(&mut r6).unwrap();
        assert!(r6.contains("401 Unauthorized"));

        let _ = server.join();
    }
}
