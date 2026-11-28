//! Built-in zero-dependency Web & Mobile Cockpit (`twig web`) inspired by T3 Code:
//! - Serves a responsive warm-terracotta dark SPA over stdlib `TcpListener`
//! - Inbox-style session sidebar (`▲ wait`, `● diff`, `✓ ok`), unified colored Git diff tab,
//!   1-click approval resolution, smart commit, worktree creation, and background prompt dispatch
//! - Loopback (`127.0.0.1`) by default; non-loopback binds (e.g. Tailscale/LAN) automatically
//!   require `Authorization: Bearer <token>` on every API call.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::config::{paths, HarnessConfig};
use crate::llm::Provider;
use crate::runtime::session::scan_sessions;
use crate::runtime::Daemon;

struct WebState {
    default_repo: PathBuf,
    token: Option<String>,
    running_sids: Mutex<HashSet<String>>,
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
        running_sids: Mutex::new(HashSet::new()),
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
    let path = raw_target.split('?').next().unwrap_or(raw_target);

    let mut content_len = 0usize;
    let mut auth_header = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k_low = k.trim().to_ascii_lowercase();
            if k_low == "content-length" {
                content_len = v.trim().parse::<usize>().unwrap_or(0).min(1_048_576);
            } else if k_low == "authorization" {
                auth_header = v.trim().to_owned();
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

    let resp_json: Result<Value, String> = match (method, path) {
        ("GET", "/api/sessions") => {
            let running = state
                .running_sids
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            let mut items = scan_sessions(None, 40);
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
        ("POST", "/api/approve") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| resolve_session_approval(&req, &state.default_repo)),
        ("POST", "/api/commit") => serde_json::from_slice::<Value>(&body_bytes)
            .map_err(|e| e.to_string())
            .and_then(|req| commit_session_worktree(&req, &state.default_repo)),
        _ => Err(format!("not found: {method} {path}")),
    };

    match resp_json {
        Ok(v) => write_http(
            &mut stream,
            200,
            "application/json",
            v.to_string().as_bytes(),
        ),
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

fn inspect_session_detail(sid: &str, state: &Arc<WebState>) -> Result<Value, String> {
    let in_flight = state
        .running_sids
        .lock()
        .map(|g| g.contains(sid))
        .unwrap_or(false);
    crate::runtime::session::inspect_session_detail(sid, &state.default_repo, in_flight)
}

fn dispatch_background_turn(req: &Value, state: &Arc<WebState>) -> Result<Value, String> {
    let prompt = req["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("prompt required")?
        .to_owned();
    let sid = req["sid"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(paths::session_id);
    if !paths::valid_session(&sid) {
        return Err("invalid session id".into());
    }
    let repo = req["worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| state.default_repo.display().to_string());
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;

    if let Ok(mut g) = state.running_sids.lock() {
        if g.contains(&sid) {
            return Err("session is already running a turn".into());
        }
        g.insert(sid.clone());
    }

    let st = state.clone();
    let sid_bg = sid.clone();
    std::thread::spawn(move || {
        let _ = Command::new(exe)
            .args(["exec", "--repo", &repo, "--resume", &sid_bg, "--json", "--", &prompt])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Ok(mut g) = st.running_sids.lock() {
            g.remove(&sid_bg);
        }
    });

    Ok(json!({"sid": sid, "queued": true}))
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
    let wt = req["worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_repo.to_path_buf());
    let (cfg, _) = HarnessConfig::resolve()?;
    let provider = Provider::new();
    let runs = paths::state_dir().join("sessions");
    let mut daemon = Daemon::new(&provider, &wt, &runs, sid, &cfg)?;
    daemon.dispatch("git.commit", &json!({}), &mut |_| {})
}

const COCKPIT_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>twig cockpit</title>
<style>
:root{color-scheme:dark;--bg:#141312;--surf:#1c1a18;--hi:#262320;--line:#2c2825;--txt:#f0ede8;--muted:#8e877f;--acc:#d97757;--sage:#7ea87e;--amber:#e0a958;--err:#d96565}
*{box-sizing:border-box;margin:0;padding:0}
body{font:400 14px/24px 'Geist','Inter',ui-sans-serif,system-ui,-apple-system,sans-serif;background:var(--bg);color:var(--txt);display:flex;height:100dvh;overflow:hidden;-webkit-font-smoothing:antialiased}
aside{width:288px;background:var(--surf);border-right:1px solid var(--line);display:flex;flex-direction:column;flex-shrink:0}
header{min-height:56px;padding:12px 16px;border-bottom:1px solid var(--line);display:flex;justify-content:space-between;align-items:center}
.brand{font-size:16px;font-weight:600;line-height:24px;letter-spacing:-0.01em;color:var(--txt);display:inline-flex;align-items:center;gap:8px}
.brand-dot{width:8px;height:8px;border-radius:8px;background:var(--acc)}
.slist{overflow-y:auto;flex:1;padding:8px}
.sitem{padding:12px;border-radius:8px;cursor:pointer;transition:background 120ms ease;margin-bottom:4px}
.sitem:hover{background:var(--hi)}
.sitem.active{background:var(--hi)}
.srow{display:flex;align-items:center;gap:8px;min-width:0}
.slabel{font-size:14px;font-weight:500;line-height:20px;color:var(--txt);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.ssub{font-size:12px;line-height:16px;color:var(--muted);margin-top:4px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.badge{font-size:12px;line-height:16px;font-weight:500;padding:0 8px;height:20px;display:inline-flex;align-items:center;border-radius:8px;flex-shrink:0}
.b-approval,.b-running{background:#332818;color:var(--amber)}
.b-dirty{background:#32201a;color:var(--acc)}
.b-settled{background:#1c291e;color:var(--sage)}
main{flex:1;display:flex;flex-direction:column;min-width:0;min-height:0;background:var(--bg)}
.topbar{min-height:56px;padding:12px 24px;border-bottom:1px solid var(--line);display:flex;gap:12px;align-items:center;flex-wrap:wrap}
#stitle{font-size:18px;font-weight:600;line-height:24px;letter-spacing:-0.01em;color:var(--txt)}
#smeta{color:var(--muted);font-size:12px;line-height:16px;flex:1;min-width:120px}
.tabs{display:inline-flex;gap:4px;padding:4px;background:var(--surf);border-radius:12px}
.tabs button{background:transparent;color:var(--muted);border:0;height:28px;padding:0 12px;border-radius:8px;cursor:pointer;font:500 12px/16px inherit;transition:color 120ms ease,background 120ms ease}
.tabs button:hover{color:var(--txt)}
.tabs button.active{background:var(--hi);color:var(--txt)}
.btn{background:var(--surf);color:var(--txt);border:1px solid var(--line);height:32px;padding:0 12px;border-radius:8px;cursor:pointer;font:500 12px/16px inherit;display:inline-flex;align-items:center;gap:8px;transition:background 120ms ease,color 120ms ease,border-color 120ms ease,opacity 140ms ease}
.btn:hover{background:var(--hi)}
.btn.ghost{background:transparent;border-color:transparent;color:var(--muted)}
.btn.ghost:hover,.btn.ghost:focus-visible{background:var(--surf);border-color:var(--line);color:var(--txt)}
.btn.primary{background:var(--acc);color:#141312;border-color:var(--acc);font-weight:600;height:44px;padding:0 16px;border-radius:12px}
.btn.primary:hover{filter:brightness(1.06)}
.ico{width:24px;height:24px;display:inline-flex;align-items:center;justify-content:center;flex-shrink:0}
.ico svg{width:18px;height:18px;stroke:currentColor;stroke-width:1.5;fill:none;stroke-linecap:round;stroke-linejoin:round}
.pane{flex:1;min-height:0;overflow-y:auto;padding:24px 32px;display:flex;flex-direction:column;gap:20px}
.pane>*{width:100%;max-width:768px;margin-left:auto;margin-right:auto}
.msg{word-break:break-word}
.msg.user{background:var(--surf);padding:12px 16px;border-radius:12px;white-space:pre-wrap;color:var(--txt)}
.msg.assistant{padding:4px 0;white-space:normal}
.msg.assistant p,.msg.assistant ul,.msg.assistant ol,.msg.assistant pre,.msg.assistant table,.msg.assistant blockquote{margin:8px 0}
.msg.assistant h2,.msg.assistant h3,.msg.assistant h4,.msg.assistant h5{font-size:16px;font-weight:600;line-height:24px;color:var(--txt);margin:16px 0 8px}
.msg.assistant ul,.msg.assistant ol{padding-left:20px}
.msg.assistant code{font:13px/20px 'Geist Mono',ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--txt);background:var(--surf);padding:0 4px;border-radius:4px}
.msg.assistant pre{background:var(--surf);padding:16px;border-radius:12px;overflow-x:auto;white-space:pre}
.msg.assistant pre code{background:none;padding:0;color:var(--txt)}
.msg.assistant .code-hdr{display:flex;justify-content:space-between;align-items:center;min-height:24px;font-size:12px;line-height:16px;color:var(--muted);margin-bottom:8px}
.copy-btn{opacity:0;height:24px;padding:0 8px;border-radius:8px;font-size:12px}
.msg.assistant pre:hover .copy-btn,.msg.assistant pre:focus-within .copy-btn{opacity:1}
.msg.assistant table{border-collapse:collapse;width:100%;font-size:13px;line-height:20px;background:var(--surf);border-radius:12px;overflow:hidden}
.msg.assistant th,.msg.assistant td{border-bottom:1px solid var(--line);padding:8px 12px;text-align:left}
.msg.assistant th{font-weight:600;color:var(--txt);background:var(--hi)}
.msg.assistant .math-blk{padding:8px 16px;margin:8px 0;border-left:2px solid var(--line);color:var(--txt);font-family:ui-serif,Georgia,serif;font-size:16px;line-height:24px}
.msg.assistant .math-in{color:var(--txt);font-family:ui-serif,Georgia,serif}
.msg.assistant .svg-card{background:var(--surf);border-radius:12px;padding:16px;margin:12px 0;overflow-x:auto}
.msg.assistant .svg-card img{max-width:100%;height:auto;display:block}
.msg.tool{font:12px/20px 'Geist Mono',ui-monospace,monospace;color:var(--muted);padding:0 4px;white-space:pre-wrap}
.approval{background:var(--surf);border-left:2px solid var(--amber);padding:16px;border-radius:12px;display:flex;justify-content:space-between;align-items:center;gap:16px;flex-wrap:wrap}
.approval-actions{display:flex;gap:8px;align-items:center}
.approval-actions .btn.primary{height:32px;padding:0 12px}
pre.diff{font:12px/20px 'Geist Mono',ui-monospace,monospace;white-space:pre-wrap;word-break:break-all;background:var(--surf);padding:16px;border-radius:12px}
.d-add{color:var(--sage)}.d-del{color:var(--err)}.d-hunk{color:var(--amber)}
.empty{color:var(--muted);font-size:14px;line-height:24px;padding:24px 0}
.composer{padding:16px 24px;border-top:1px solid var(--line);display:flex;gap:12px;align-items:flex-end;background:var(--bg)}
.composer-inner{width:100%;max-width:768px;margin:0 auto;display:flex;gap:12px;align-items:center}
textarea{flex:1;background:var(--surf);color:var(--txt);border:1px solid var(--line);border-radius:12px;padding:8px 12px;font:inherit;line-height:24px;resize:none;height:44px;outline:none;transition:border-color 120ms ease}
textarea:focus{border-color:var(--muted)}
@media(hover:none){.copy-btn{opacity:1}}
@media(max-width:720px){body{flex-direction:column}aside{width:100%;height:192px;border-right:0;border-bottom:1px solid var(--line)}.topbar{padding:12px 16px}.pane{padding:16px;gap:16px}.composer{padding:12px 16px}}
</style>
</head>
<body>
<aside>
  <header>
    <span class="brand"><span class="brand-dot"></span>twig cockpit</span>
    <button class="btn" onclick="newThread()"><span class="ico"><svg viewBox="0 0 24 24"><path d="M12 5v14M5 12h14"/></svg></span>New</button>
  </header>
  <div class="slist" id="slist"></div>
</aside>
<main>
  <div class="topbar">
    <strong id="stitle">New thread</strong>
    <span id="smeta"></span>
    <div class="tabs">
      <button id="t-chat" class="active" onclick="setTab('chat')">Thread</button>
      <button id="t-diff" onclick="setTab('diff')">Diff</button>
    </div>
    <button class="btn ghost" onclick="commitNow()"><span class="ico"><svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3"/><path d="M3 12h6m6 0h6"/></svg></span>Commit</button>
  </div>
  <div class="pane" id="pane"></div>
  <div class="composer">
    <div class="composer-inner">
      <textarea id="inp" placeholder="Ask twig…"></textarea>
      <button class="btn primary" onclick="sendPrompt()"><span class="ico"><svg viewBox="0 0 24 24"><path d="M5 12h14M13 6l6 6-6 6"/></svg></span>Send</button>
    </div>
  </div>
</main>
<script>
const tok=new URLSearchParams(location.search).get('token')||localStorage.getItem('twig_tok')||'';
if(tok)localStorage.setItem('twig_tok',tok);
let curSid='',curWt='',tab='chat',detail=null,lastHtml='',forceTail=true,draftNew=false;
const hdrs=()=>tok?{'Content-Type':'application/json','Authorization':'Bearer '+tok}:{'Content-Type':'application/json'};
async function api(p,o={}){const r=await fetch(p,{...o,headers:hdrs()});return r.json();}
function esc(s){return String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
function svgDataUri(raw){try{const b=new TextEncoder().encode(raw);let s='';for(let i=0;i<b.length;i++)s+=String.fromCharCode(b[i]);return 'data:image/svg+xml;base64,'+btoa(s);}catch(_){return '';}}
const SYM={'\\alpha':'α','\\beta':'β','\\gamma':'γ','\\delta':'δ','\\theta':'θ','\\lambda':'λ','\\mu':'μ','\\pi':'π','\\sigma':'σ','\\omega':'ω','\\Delta':'Δ','\\Sigma':'∑','\\sum':'∑','\\prod':'∏','\\int':'∫','\\infty':'∞','\\pm':'±','\\times':'×','\\cdot':'·','\\leq':'≤','\\le':'≤','\\geq':'≥','\\ge':'≥','\\neq':'≠','\\ne':'≠','\\approx':'≈','\\to':'→','\\rightarrow':'→','\\Rightarrow':'⇒','\\in':'∈','\\forall':'∀','\\exists':'∃','\\partial':'∂','\\nabla':'∇'};
function fmtTex(s){
  let o=esc(s).replace(/\\frac\{([^{}]+)\}\{([^{}]+)\}/g,'($1)/($2)').replace(/\\sqrt\{([^{}]+)\}/g,'√($1)').replace(/\\text\{([^{}]+)\}/g,'$1');
  for(const[k,v]of Object.entries(SYM))o=o.split(k).join(v);
  return o.replace(/\^\{([^{}]+)\}/g,'<sup>$1</sup>').replace(/\^([0-9a-zA-Z])/g,'<sup>$1</sup>').replace(/_\{([^{}]+)\}/g,'<sub>$1</sub>').replace(/_([0-9a-zA-Z])/g,'<sub>$1</sub>');
}
function fmtIn(s){
  const codes=[];
  let o=s.replace(/`([^`\n]+)`/g,(_,c)=>{codes.push(`<code>${esc(c)}</code>`);return `\x00C${codes.length-1}\x00`;});
  o=esc(o)
    .replace(/\\\((.+?)\\\)/g,(_,m)=>`<span class="math-in">${fmtTex(m)}</span>`)
    .replace(/(^|[^\\$])\$([^\s$][^$\n]*?[^\s$]|[^\s$])\$/g,(all,p,m)=>/^[0-9.,]+$/.test(m)?all:`${p}<span class="math-in">${fmtTex(m)}</span>`)
    .replace(/\*\*(.+?)\*\*/g,'<strong>$1</strong>')
    .replace(/\[([^\]]+)\]\((https?:\/\/[^\s)"']+)\)/g,(_,l,u)=>`<a href="${u}" target="_blank" rel="noopener noreferrer" style="color:var(--acc)">${l}</a>`);
  return o.replace(/\x00C(\d+)\x00/g,(_,i)=>codes[+i]);
}
function fmtMd(src){
  const lines=(src||'').replace(/\r\n/g,'\n').split('\n');
  let out='',i=0;
  while(i<lines.length){
    const ln=lines[i],tr=ln.trim();
    if(tr.startsWith('```')){
      const lang=tr.slice(3).trim().toLowerCase();i++;let buf=[];
      while(i<lines.length&&!lines[i].trim().startsWith('```')){buf.push(lines[i]);i++;}
      i++;
      if(lang==='math'||lang==='latex'||lang==='tex')out+=`<div class="math-blk">${fmtTex(buf.join('\n'))}</div>`;
      else if(lang==='svg'||(buf.join('').trim().startsWith('<svg'))){
        const uri=svgDataUri(buf.join('\n'));
        out+=uri?`<div class="svg-card"><img src="${uri}" alt="diagram"></div>`:`<pre><code>${esc(buf.join('\n'))}</code></pre>`;
      }
      else out+=`<pre><div class="code-hdr"><span>${esc(lang||'code')}</span><button class="btn copy-btn" onclick="navigator.clipboard.writeText(this.parentElement.nextElementSibling.textContent)">Copy</button></div><code>${esc(buf.join('\n'))}</code></pre>`;
      continue;
    }
    if(tr.startsWith('$$')||tr.startsWith('\\[')){
      const cl=tr.startsWith('$$')?'$$':'\\]';
      if(tr.length>2&&tr.endsWith(cl)){out+=`<div class="math-blk">${fmtTex(tr.slice(2,-2))}</div>`;i++;continue;}
      let buf=[tr.slice(2)];i++;
      while(i<lines.length&&!lines[i].trim().endsWith(cl)){buf.push(lines[i]);i++;}
      if(i<lines.length){buf.push(lines[i].trim().slice(0,-2));i++;}
      out+=`<div class="math-blk">${fmtTex(buf.join(' '))}</div>`;continue;
    }
    if(tr.includes('|')&&i+1<lines.length&&/^\|?[\s:-]+\|[\s|:-]*$/.test(lines[i+1].trim())){
      const cells=r=>r.trim().replace(/^\||\|$/g,'').split('|').map(c=>c.trim());
      const hdr=cells(ln);i+=2;let rows='';
      while(i<lines.length&&lines[i].trim().includes('|')){rows+=`<tr>${cells(lines[i]).map(c=>`<td>${fmtIn(c)}</td>`).join('')}</tr>`;i++;}
      out+=`<table><thead><tr>${hdr.map(c=>`<th>${fmtIn(c)}</th>`).join('')}</tr></thead><tbody>${rows}</tbody></table>`;continue;
    }
    if(!tr){i++;continue;}
    const hm=tr.match(/^(#{1,4})\s+(.+)$/);
    if(hm){const lv=hm[1].length+1;out+=`<h${lv}>${fmtIn(hm[2])}</h${lv}>`;i++;continue;}
    if(/^[-*]\s+/.test(tr)){
      let items='';while(i<lines.length&&/^[-*]\s+/.test(lines[i].trim())){items+=`<li>${fmtIn(lines[i].trim().replace(/^[-*]\s+/,''))}</li>`;i++;}
      out+=`<ul>${items}</ul>`;continue;
    }
    out+=`<p>${fmtIn(ln)}</p>`;i++;
  }
  return out;
}
async function loadList(){
  const d=await api('/api/sessions');
  if(!d.sessions)return;
  if(!curWt&&d.default_repo)curWt=d.default_repo;
  const el=document.getElementById('slist');
  el.innerHTML=d.sessions.map(s=>{
    const cls='b-'+(s.status||'settled');
    const tag=s.status==='approval'?'wait':s.status==='running'?'run':s.status==='dirty'?'diff':'ok';
    return `<div class="sitem ${s.sid===curSid?'active':''}" data-sid="${esc(s.sid)}" onclick="openSid(this.dataset.sid)">
      <div class="srow"><span class="badge ${esc(cls)}">${tag}</span><span class="slabel">${esc(s.label)}</span></div>
      <div class="ssub">${esc(s.branch||'main')} · t${s.turns} · ${esc(s.age)}</div>
    </div>`;
  }).join('');
  if(!curSid&&!draftNew&&d.sessions.length)openSid(d.sessions[0].sid);
}
async function openSid(sid,resetScroll=false){draftNew=false;if(sid!==curSid)forceTail=true;if(resetScroll)forceTail=true;curSid=sid;detail=await api('/api/session/'+sid);curWt=detail.worktree||curWt;render();}
function newThread(){draftNew=true;curSid='';forceTail=true;lastHtml='';detail={messages:[],pending:[],diff:'',branch:'new',worktree:curWt};setTab('chat');}
function setTab(t){tab=t;lastHtml='';document.getElementById('t-chat').className=t==='chat'?'active':'';document.getElementById('t-diff').className=t==='diff'?'active':'';render();}
function render(){
  if(!detail)return;
  document.getElementById('stitle').textContent=curSid?curSid.slice(0,12):'New thread';
  document.getElementById('smeta').textContent=[detail.branch,detail.model,detail.running?'running…':''].filter(Boolean).join(' · ');
  const p=document.getElementById('pane');
  const nearBottom=forceTail||(p.scrollHeight-p.scrollTop-p.clientHeight<56);
  if(tab==='diff'){
    const lines=(detail.diff||'(working tree clean)').split('\n').map(l=>{
      const c=l.startsWith('+')?'d-add':l.startsWith('-')?'d-del':l.startsWith('@@')?'d-hunk':'';
      return `<span class="${c}">${esc(l)}</span>`;
    }).join('\n');
    const next=`<pre class="diff">${lines}</pre>`;
    if(next!==lastHtml){const prevTop=p.scrollTop;p.innerHTML=next;lastHtml=next;p.scrollTop=prevTop;}
    return;
  }
  let html=(detail.pending||[]).map(a=>`<div class="approval"><div><strong>Approve ${esc(a.name)}?</strong><div class="ssub">${esc(JSON.stringify(a.args))}</div></div><div class="approval-actions"><button class="btn primary" data-id="${esc(a.id)}" onclick="approve(this.dataset.id,true)">Allow</button><button class="btn" data-id="${esc(a.id)}" onclick="approve(this.dataset.id,false)">Deny</button></div></div>`).join('');
  html+=(detail.messages||[]).filter(m=>m.role!=='system').map(m=>{
    const txt=typeof m.content==='string'?m.content:JSON.stringify(m.content);
    const rCls=m.role==='assistant'?'assistant':m.role==='user'?'user':'tool';
    if(m.role==='tool')return `<div class="msg tool">↳ ${esc(txt.slice(0,400))}</div>`;
    return `<div class="msg ${rCls}">${m.role==='assistant'?fmtMd(txt):esc(txt)}</div>`;
  }).join('');
  const finalHtml=html||'<div class="empty">No messages yet.</div>';
  if(finalHtml!==lastHtml){
    const prevTop=p.scrollTop;
    p.innerHTML=finalHtml;
    lastHtml=finalHtml;
    p.scrollTop=nearBottom?p.scrollHeight:prevTop;
  }else if(forceTail){
    p.scrollTop=p.scrollHeight;
  }
  forceTail=false;
}
async function sendPrompt(){
  const inp=document.getElementById('inp');const text=inp.value.trim();if(!text)return;inp.value='';
  forceTail=true;
  const r=await api('/api/send',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,prompt:text})});
  if(r.sid){curSid=r.sid;setTimeout(()=>openSid(curSid,true),300);}
}
async function approve(id,allow){await api('/api/approve',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,id,allow})});openSid(curSid);}
async function commitNow(){if(!curSid)return;const r=await api('/api/commit',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt})});alert(r.summary||r.error||'done');openSid(curSid);}
document.getElementById('inp').addEventListener('keydown',e=>{if(e.key==='Enter'&&!e.shiftKey){e.preventDefault();sendPrompt();}});
loadList();setInterval(()=>{loadList();if(curSid)openSid(curSid);},2500);
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_cockpit_enforces_bearer_token_when_configured_and_serves_ui() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(WebState {
            default_repo: PathBuf::from("."),
            token: Some("secret-tok".into()),
            running_sids: Mutex::new(HashSet::new()),
        });

        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                if let Ok((stream, _)) = listener.accept() {
                    let _ = handle_conn(stream, &state);
                }
            }
        });

        // 1. GET / serves cockpit HTML without auth header
        let mut c1 = TcpStream::connect(addr).unwrap();
        write!(c1, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut r1 = String::new();
        c1.read_to_string(&mut r1).unwrap();
        assert!(r1.contains("200 OK") && r1.contains("twig cockpit"));
        assert!(r1.contains("Content-Security-Policy:"));
        assert!(!COCKPIT_HTML.contains("approve('${a.id}'"));
        assert!(!COCKPIT_HTML.contains("openSid('${s.sid}'"));
        assert!(COCKPIT_HTML.contains("data:image/svg+xml;base64,"));
        assert!(COCKPIT_HTML.contains("'Geist'") && COCKPIT_HTML.contains(".copy-btn{opacity:0"));

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

        let _ = server.join();
    }
}

