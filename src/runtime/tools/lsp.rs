//! Zero-dependency on-demand stdio LSP 3.17 client (`Content-Length` framing)
//! for compiler-grade `documentSymbol` and `workspace/symbol` resolution when
//! `outline(..., lsp=true)` is requested. Falls back cleanly if no language
//! server binary is installed or if workspace indexing exceeds bounded timeout.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const LSP_TIMEOUT: Duration = Duration::from_secs(8);

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next_id: u64,
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn detect_server(root: &Path, target: &Path) -> Option<(&'static str, &'static [&'static str], &'static str)> {
    let ext = target
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let candidates: &[(&str, &[&str], &str)] = match ext.as_str() {
        "rs" => &[("rust-analyzer", &[], "rust")],
        "go" => &[("gopls", &["serve"], "go")],
        "py" => &[("pyright-langserver", &["--stdio"], "python")],
        "ts" | "tsx" | "js" | "jsx" | "mjs" => {
            &[("typescript-language-server", &["--stdio"], "typescript")]
        }
        "c" | "h" | "cpp" | "hpp" => &[("clangd", &["--log=error"], "cpp")],
        _ => {
            if root.join("Cargo.toml").is_file() {
                &[("rust-analyzer", &[], "rust")]
            } else if root.join("go.mod").is_file() {
                &[("gopls", &["serve"], "go")]
            } else if root.join("tsconfig.json").is_file() {
                &[("typescript-language-server", &["--stdio"], "typescript")]
            } else if root.join("pyproject.toml").is_file() {
                &[("pyright-langserver", &["--stdio"], "python")]
            } else {
                return None;
            }
        }
    };
    for &(bin, args, lang) in candidates {
        let available = Command::new(bin)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if available {
            return Some((bin, args, lang));
        }
    }
    None
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy().replace('\\', "/"))
}

fn symbol_kind_label(kind: u64) -> &'static str {
    match kind {
        2 | 3 => "mod",
        5 => "class",
        6 => "method",
        10 => "enum",
        11 => "interface",
        12 => "fn",
        13 => "var",
        14 => "const",
        23 => "struct",
        26 => "type",
        _ => "sym",
    }
}

impl LspClient {
    fn spawn(root: &Path, bin: &str, args: &[&str]) -> Result<Self, String> {
        let mut child = Command::new(bin)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn {bin}: {e}"))?;
        let stdin = child.stdin.take().ok_or("lsp missing stdin")?;
        let stdout = child.stdout.take().ok_or("lsp missing stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut content_len: Option<usize> = None;
                let mut header = String::new();
                loop {
                    header.clear();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {
                            let trimmed = header.trim();
                            if trimmed.is_empty() {
                                break;
                            }
                            if let Some(rest) = trimmed
                                .strip_prefix("Content-Length:")
                                .or_else(|| trimmed.strip_prefix("content-length:"))
                            {
                                content_len = rest.trim().parse::<usize>().ok();
                            }
                        }
                    }
                }
                let Some(len) = content_len.filter(|&n| n > 0 && n <= 8 * 1024 * 1024) else {
                    continue;
                };
                let mut buf = vec![0u8; len];
                if reader.read_exact(&mut buf).is_err() {
                    return;
                }
                if let Ok(val) = serde_json::from_slice::<Value>(&buf) {
                    if tx.send(val).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            rx,
            next_id: 0,
        })
    }

    fn send_frame(&mut self, msg: &Value) -> Result<(), String> {
        let body = msg.to_string();
        write!(
            self.stdin,
            "Content-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .map_err(|e| format!("lsp write: {e}"))?;
        self.stdin.flush().map_err(|e| format!("lsp flush: {e}"))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send_frame(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send_frame(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + timeout;
        loop {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return Err(format!("LSP `{method}` timed out after {}s", timeout.as_secs()));
            }
            match self.rx.recv_timeout(rem.min(Duration::from_millis(100))) {
                Ok(v) => {
                    if v.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    if let Some(err) = v.get("error") {
                        return Err(err["message"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| err.to_string()));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("LSP server closed stdout".into());
                }
            }
        }
    }
}

fn flatten_doc_symbols(items: &[Value], prefix: &str, out: &mut Vec<String>) {
    for item in items {
        if out.len() >= 200 {
            break;
        }
        let name = item["name"].as_str().unwrap_or("?");
        let kind = symbol_kind_label(item["kind"].as_u64().unwrap_or(0));
        let line = item
            .pointer("/selectionRange/start/line")
            .or_else(|| item.pointer("/range/start/line"))
            .or_else(|| item.pointer("/location/range/start/line"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + 1;
        let qualified = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}::{name}")
        };
        out.push(format!("{line:>4}: [{kind}] {qualified}"));
        if let Some(children) = item.get("children").and_then(Value::as_array) {
            flatten_doc_symbols(children, &qualified, out);
        }
    }
}

pub(super) fn query_lsp(root: &Path, base: &Path, symbol: Option<&str>) -> Result<String, String> {
    let (bin, args, lang_id) =
        detect_server(root, base).ok_or_else(|| "no matching local LSP binary found".to_owned())?;
    let mut client = LspClient::spawn(root, bin, args)?;
    let root_uri = file_uri(root);
    let _ = client.request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {"documentSymbol": {"hierarchicalDocumentSymbolSupport": true}},
                "workspace": {"symbol": {}}
            }
        }),
        LSP_TIMEOUT,
    )?;
    client.notify("initialized", json!({}))?;

    let out = if let Some(q) = symbol.map(str::trim).filter(|s| !s.is_empty()) {
        let res = client.request("workspace/symbol", json!({"query": q}), LSP_TIMEOUT)?;
        let arr = res.as_array().cloned().unwrap_or_default();
        if arr.is_empty() {
            return Err(format!("{bin} returned 0 workspace symbols for {q:?}"));
        }
        let mut lines = Vec::new();
        for item in arr.iter().take(80) {
            let name = item["name"].as_str().unwrap_or("?");
            let kind = symbol_kind_label(item["kind"].as_u64().unwrap_or(0));
            let container = item["containerName"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|c| format!(" (in {c})"))
                .unwrap_or_default();
            let uri = item
                .pointer("/location/uri")
                .and_then(Value::as_str)
                .unwrap_or("");
            let rel = uri
                .strip_prefix(&format!("{}/", file_uri(root)))
                .unwrap_or(uri);
            let line = item
                .pointer("/location/range/start/line")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1;
            lines.push(format!("[{kind}] {rel}:{line}: {name}{container}"));
        }
        format!("### LSP Workspace Symbols via `{bin}`\n{}", lines.join("\n"))
    } else if base.is_file() {
        let text = std::fs::read_to_string(base).map_err(|e| e.to_string())?;
        let uri = file_uri(base);
        client.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": lang_id,
                    "version": 1,
                    "text": text
                }
            }),
        )?;
        let res = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri}}),
            LSP_TIMEOUT,
        )?;
        let arr = res.as_array().cloned().unwrap_or_default();
        let mut lines = Vec::new();
        flatten_doc_symbols(&arr, "", &mut lines);
        if lines.is_empty() {
            return Err(format!("{bin} returned 0 document symbols"));
        }
        format!("### LSP Document Symbols via `{bin}`\n{}", lines.join("\n"))
    } else {
        return Err("LSP directory mode requires `symbol` query or a single file `path`".into());
    };

    let _ = client.request("shutdown", Value::Null, Duration::from_millis(500));
    Ok(out)
}
