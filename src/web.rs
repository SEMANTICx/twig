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
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::config::{paths, HarnessConfig};
use crate::exec::{run_process, shell_command};
use crate::llm::{cli, Llm, Provider};
use crate::runtime::guidance::SkillStore;
use crate::runtime::policy::{Policy, Tier};
use crate::runtime::session::{archive_session, scan_sessions};
use crate::runtime::workflows::build_goal_prompt;
use crate::runtime::Daemon;

const COCKPIT_HTML: &str = include_str!("web_ui.html");

struct WebState {
    default_repo: PathBuf,
    token: Option<String>,
    running_pids: Mutex<HashMap<String, u32>>,
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
        running_pids: Mutex::new(HashMap::new()),
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
        if auth_header != want {
            let err = json!({"error": "unauthorized — pass Bearer token"}).to_string();
            return write_http(&mut stream, 401, "application/json", err.as_bytes());
        }
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
        ("GET", "/api/sessions") => {
            let all = query.split('&').any(|kv| kv == "all=1");
            let running: std::collections::HashSet<String> = state
                .running_pids
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
        "Content-Security-Policy: default-src 'self'; img-src 'self' data:; style-src 'unsafe-inline'; script-src 'unsafe-inline'; object-src 'none'; base-uri 'none'\r\n"
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
        .running_pids
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
    let raw_prompt = req["prompt"]
        .as_str()
        .map(str::trim)
        .unwrap_or("");
    let has_images = req["images"].as_array().is_some_and(|a| !a.is_empty());
    if raw_prompt.is_empty() && !has_images {
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
    if let Some(imgs) = req["images"].as_array() {
        let staged = stage_data_images(&sid, imgs);
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
    let model = req["model"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let effort = req["effort"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;

    {
        let g = state.running_pids.lock().map_err(|e| e.to_string())?;
        if g.contains_key(&sid) {
            return Err("session is already running a turn".into());
        }
    }

    let mut cmd = Command::new(exe);
    cmd.args(["exec", "--repo", &repo, "--resume", &sid, "--json"]);
    if let Some(r) = role.as_deref() {
        cmd.args(["--role", r]);
    }
    if let Some(m) = model.as_deref() {
        cmd.args(["--model", m]);
    }
    if let Some(e) = effort.as_deref() {
        cmd.args(["--effort", e]);
    }
    cmd.args(["--", &prompt])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn twig exec: {e}"))?;
    let pid = child.id();
    if let Ok(mut g) = state.running_pids.lock() {
        g.insert(sid.clone(), pid);
    }

    let st = state.clone();
    let sid_bg = sid.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        if let Ok(mut g) = st.running_pids.lock() {
            g.remove(&sid_bg);
        }
    });

    Ok(json!({"sid": sid, "worktree": repo, "queued": true}))
}

fn cancel_running_turn(req: &Value, state: &Arc<WebState>) -> Result<Value, String> {
    let sid = req["sid"].as_str().ok_or("sid required")?;
    let pid_opt = state
        .running_pids
        .lock()
        .map_err(|e| e.to_string())?
        .remove(sid);
    let Some(pid) = pid_opt else {
        return Ok(json!({"stopped": false}));
    };
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
        "session.rename" | "session.btw" | "session.fork" | "session.compact"
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
            running_pids: Mutex::new(HashMap::new()),
        });

        let server = std::thread::spawn(move || {
            for _ in 0..5 {
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

        let _ = server.join();
    }
}
