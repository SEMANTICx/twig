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
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn inspect_session_detail(sid: &str, state: &Arc<WebState>) -> Result<Value, String> {
    if !paths::valid_session(sid) {
        return Err("invalid session id".into());
    }
    let path = paths::state_dir()
        .join("sessions")
        .join(format!("{sid}.jsonl"));
    if !path.exists() {
        return Err("session not found".into());
    }
    let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let mut worktree = state.default_repo.display().to_string();
    let mut branch = String::new();
    let mut model = String::new();
    let mut messages = Vec::new();
    let mut pending_map = serde_json::Map::new();

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match row["kind"].as_str() {
            Some("session.meta") => {
                if let Some(w) = row["worktree"].as_str().filter(|s| !s.is_empty()) {
                    worktree = w.to_owned();
                }
                if let Some(b) = row["branch"].as_str().filter(|s| !s.is_empty()) {
                    branch = b.to_owned();
                }
                if let Some(m) = row["model"].as_str().filter(|s| !s.is_empty()) {
                    model = m.to_owned();
                }
            }
            Some("messages.checkpoint") => {
                if let Some(arr) = row["messages"].as_array() {
                    messages = arr.clone();
                }
            }
            Some("message") => messages.push(row["message"].clone()),
            Some("approval.requested") => {
                if let Some(id) = row["id"].as_str() {
                    pending_map.insert(id.to_owned(), row);
                }
            }
            Some("approval.resolved") | Some("tool.result") => {
                if let Some(id) = row["id"].as_str() {
                    pending_map.remove(id);
                }
            }
            _ => {}
        }
    }

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
    let running = state
        .running_sids
        .lock()
        .map(|g| g.contains(sid))
        .unwrap_or(false);

    Ok(json!({
        "sid": sid,
        "worktree": worktree,
        "branch": branch,
        "model": model,
        "running": running,
        "messages": messages,
        "pending": pending_map.into_values().collect::<Vec<_>>(),
        "diff": diff,
    }))
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
:root{--bg:#161311;--surf:#211d1a;--hi:#2c2622;--txt:#f0ebe3;--muted:#9a8f85;--acc:#d97757;--sage:#7ea87e;--amber:#e0a958;--err:#d96565}
*{box-sizing:border-box;margin:0;padding:0}
body{font:14px/1.55 ui-sans-serif,system-ui,-apple-system,sans-serif;background:var(--bg);color:var(--txt);display:flex;height:100dvh;overflow:hidden}
aside{width:290px;background:var(--surf);border-right:1px solid #332c27;display:flex;flex-direction:column;flex-shrink:0}
header{padding:12px 14px;border-bottom:1px solid #332c27;display:flex;justify-content:space-between;align-items:center;font-weight:700;color:var(--acc)}
.slist{overflow-y:auto;flex:1}
.sitem{padding:10px 14px;border-bottom:1px solid #28231f;cursor:pointer}
.sitem:hover,.sitem.active{background:var(--hi)}
.badge{font-size:11px;font-weight:700;padding:1px 6px;border-radius:4px;margin-right:6px}
.b-approval,.b-running{background:#3d2f1b;color:var(--amber)}
.b-dirty{background:#3a231c;color:var(--acc)}
.b-settled{background:#1f2f21;color:var(--sage)}
main{flex:1;display:flex;flex-direction:column;min-width:0}
.topbar{padding:10px 16px;border-bottom:1px solid #332c27;display:flex;gap:10px;align-items:center;flex-wrap:wrap;background:var(--surf)}
.tabs button,.btn{background:var(--hi);color:var(--txt);border:1px solid #3c342e;padding:5px 11px;border-radius:6px;cursor:pointer;font-size:12px}
.tabs button.active,.btn.primary{background:var(--acc);color:#161311;border-color:var(--acc);font-weight:700}
.pane{flex:1;overflow-y:auto;padding:16px;display:flex;flex-direction:column;gap:12px}
.msg{padding:10px 14px;border-radius:8px;background:var(--surf);white-space:pre-wrap;word-break:break-word}
.msg.assistant{white-space:normal}
.msg.assistant p,.msg.assistant ul,.msg.assistant ol,.msg.assistant pre,.msg.assistant table,.msg.assistant blockquote{margin:6px 0}
.msg.assistant ul,.msg.assistant ol{padding-left:20px}
.msg.assistant code{font:12.5px/1.4 ui-monospace,monospace;color:#f5b799;background:#2a231e;padding:1px 5px;border-radius:4px}
.msg.assistant pre{background:#120f0d;border:1px solid #332c27;padding:10px 12px;border-radius:6px;overflow-x:auto;white-space:pre}
.msg.assistant pre code{background:none;padding:0;color:var(--txt)}
.msg.assistant table{border-collapse:collapse;width:100%;font-size:13px}
.msg.assistant th,.msg.assistant td{border:1px solid #3c342e;padding:5px 9px;text-align:left}
.msg.assistant th{background:var(--hi);color:var(--acc)}
.msg.assistant .math-blk{padding:6px 12px;margin:6px 0;border-left:2px solid var(--amber);color:var(--amber);font-family:ui-serif,Georgia,serif;font-size:15px}
.msg.assistant .math-in{color:var(--amber);font-family:ui-serif,Georgia,serif}
.msg.assistant .svg-card{background:#120f0d;border:1px solid #332c27;border-radius:6px;padding:10px;margin:6px 0;overflow-x:auto}
.msg.assistant .svg-card svg{max-width:100%;height:auto;display:block}
.msg.assistant .code-hdr{display:flex;justify-content:space-between;align-items:center;font-size:11px;color:var(--muted);margin-bottom:4px}
.msg.user{border-left:3px solid var(--acc);background:var(--hi)}
.msg.tool{font:12px/1.45 ui-monospace,monospace;color:var(--muted);padding:6px 12px}
.approval{background:#362917;border:1px solid var(--amber);padding:12px;border-radius:8px;display:flex;justify-content:space-between;align-items:center;gap:10px}
pre.diff{font:12px/1.5 ui-monospace,monospace;white-space:pre-wrap;word-break:break-all}
.d-add{color:var(--sage)}.d-del{color:var(--err)}.d-hunk{color:var(--amber)}
.composer{padding:12px 16px;border-top:1px solid #332c27;display:flex;gap:8px;background:var(--surf)}
textarea{flex:1;background:var(--bg);color:var(--txt);border:1px solid #3c342e;border-radius:6px;padding:8px 10px;font:inherit;resize:none;height:44px}
@media(max-width:720px){body{flex-direction:column}aside{width:100%;height:190px;border-right:0;border-bottom:1px solid #332c27}}
</style>
</head>
<body>
<aside>
  <header><span>twig cockpit</span><button class="btn" onclick="newThread()">+ New</button></header>
  <div class="slist" id="slist"></div>
</aside>
<main>
  <div class="topbar">
    <strong id="stitle">Select or start a thread</strong>
    <span id="smeta" style="color:var(--muted);font-size:12px;flex:1"></span>
    <div class="tabs">
      <button id="t-chat" class="active" onclick="setTab('chat')">Thread</button>
      <button id="t-diff" onclick="setTab('diff')">Diff</button>
    </div>
    <button class="btn" onclick="commitNow()">Commit</button>
  </div>
  <div class="pane" id="pane"></div>
  <div class="composer">
    <textarea id="inp" placeholder="Ask twig… (Enter to send, Shift+Enter newline)"></textarea>
    <button class="btn primary" onclick="sendPrompt()">Send</button>
  </div>
</main>
<script>
const tok=new URLSearchParams(location.search).get('token')||localStorage.getItem('twig_tok')||'';
if(tok)localStorage.setItem('twig_tok',tok);
let curSid='',curWt='',tab='chat',detail=null;
const hdrs=()=>tok?{'Content-Type':'application/json','Authorization':'Bearer '+tok}:{'Content-Type':'application/json'};
async function api(p,o={}){const r=await fetch(p,{...o,headers:hdrs()});return r.json();}
function esc(s){return (s||'').replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]));}
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
    .replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g,'<a href="$2" target="_blank" rel="noopener" style="color:var(--acc)">$1</a>');
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
        const safe=buf.join('\n').replace(/<script[\s\S]*?<\/script>/gi,'').replace(/\son\w+\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)/gi,'').replace(/javascript:/gi,'');
        out+=`<div class="svg-card">${safe}</div>`;
      }
      else out+=`<pre><div class="code-hdr"><span>${esc(lang||'code')}</span><button class="btn" style="padding:1px 7px;font-size:11px" onclick="navigator.clipboard.writeText(this.parentElement.nextElementSibling.textContent)">Copy</button></div><code>${esc(buf.join('\n'))}</code></pre>`;
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
    if(hm){const lv=hm[1].length+1;out+=`<h${lv} style="color:var(--acc);margin:8px 0 4px">${fmtIn(hm[2])}</h${lv}>`;i++;continue;}
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
    return `<div class="sitem ${s.sid===curSid?'active':''}" onclick="openSid('${s.sid}')">
      <div><span class="badge ${cls}">${tag}</span><strong>${esc(s.label)}</strong></div>
      <div style="font-size:11px;color:var(--muted);margin-top:3px">${esc(s.branch||'main')} · t${s.turns} · ${esc(s.age)}</div>
    </div>`;
  }).join('');
  if(!curSid&&d.sessions.length)openSid(d.sessions[0].sid);
}
async function openSid(sid){curSid=sid;detail=await api('/api/session/'+sid);curWt=detail.worktree||curWt;render();}
function newThread(){curSid='';detail={messages:[],pending:[],diff:'',branch:'new',worktree:curWt};render();}
function setTab(t){tab=t;document.getElementById('t-chat').className=t==='chat'?'active':'';document.getElementById('t-diff').className=t==='diff'?'active':'';render();}
function render(){
  if(!detail)return;
  document.getElementById('stitle').textContent=curSid?curSid.slice(0,12):'New Thread';
  document.getElementById('smeta').textContent=`${detail.branch||''} · ${detail.model||''} ${detail.running?'· ✻ running…':''}`;
  const p=document.getElementById('pane');
  if(tab==='diff'){
    const lines=(detail.diff||'(working tree clean)').split('\n').map(l=>{
      const c=l.startsWith('+')?'d-add':l.startsWith('-')?'d-del':l.startsWith('@@')?'d-hunk':'';
      return `<span class="${c}">${esc(l)}</span>`;
    }).join('\n');
    p.innerHTML=`<pre class="diff">${lines}</pre>`;return;
  }
  let html=(detail.pending||[]).map(a=>`<div class="approval"><div><strong>Approve ${esc(a.name)}?</strong><div style="font-size:12px;color:var(--muted)">${esc(JSON.stringify(a.args))}</div></div><div><button class="btn primary" onclick="approve('${a.id}',true)">Allow</button> <button class="btn" onclick="approve('${a.id}',false)">Deny</button></div></div>`).join('');
  html+=(detail.messages||[]).filter(m=>m.role!=='system').map(m=>{
    const txt=typeof m.content==='string'?m.content:JSON.stringify(m.content);
    if(m.role==='tool')return `<div class="msg tool">↳ ${esc(txt.slice(0,400))}</div>`;
    return `<div class="msg ${m.role}">${m.role==='assistant'?fmtMd(txt):esc(txt)}</div>`;
  }).join('');
  p.innerHTML=html||'<div style="color:var(--muted)">Send a prompt below to begin.</div>';
}
async function sendPrompt(){
  const inp=document.getElementById('inp');const text=inp.value.trim();if(!text)return;inp.value='';
  const r=await api('/api/send',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,prompt:text})});
  if(r.sid){curSid=r.sid;setTimeout(()=>openSid(curSid),300);}
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

