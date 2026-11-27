//! Minimal stdio JSON-RPC 2.0 MCP (Model Context Protocol) client.
//!
//! Discovers `mcpServers` from `<worktree>/.twig/mcp.json` and
//! `~/.config/twig/mcp.json` (project overrides global), spawns servers lazily
//! on first inspection or call, supports tools, prompts (Skills over MCP), and
//! resources, and cleans up child processes on drop.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::paths;
use crate::control::Control;

const MAX_MCP_OUTPUT_BYTES: usize = 64_000;
const DEFAULT_MCP_TIMEOUT: Duration = Duration::from_secs(30);
const INIT_MCP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct McpFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct McpServerConfig {
    command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    disabled: bool,
}

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Result<String, String>>,
    stderr_tail: std::sync::Arc<std::sync::Mutex<String>>,
    next_id: u64,
    tools: Vec<Value>,
    prompts: Vec<Value>,
    resources: Vec<Value>,
}

fn clip_mcp_text(text: String) -> String {
    if text.len() > MAX_MCP_OUTPUT_BYTES {
        let clipped: String = text.chars().take(MAX_MCP_OUTPUT_BYTES).collect();
        format!("{clipped}\n... [mcp output truncated]")
    } else {
        text
    }
}

impl McpProcess {
    fn spawn(
        cfg: &McpServerConfig,
        worktree: &Path,
        init_timeout: Duration,
        control: &Control,
    ) -> Result<Self, String> {
        let mut cmd = Command::new(&cfg.command);
        if worktree.is_dir() {
            cmd.current_dir(worktree);
        }
        cmd.args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn mcp {:?}: {e}", cfg.command))?;
        let stdin = child.stdin.take().ok_or("mcp missing stdin")?;
        let stdout = child.stdout.take().ok_or("mcp missing stdout")?;
        let stderr = child.stderr.take().ok_or("mcp missing stderr")?;
        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let stderr_sink = stderr_tail.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).ok().filter(|&n| n > 0).is_some() {
                if let Ok(mut buf) = stderr_sink.lock() {
                    buf.push_str(&line);
                    if buf.len() > 2_048 {
                        let keep: String = buf
                            .chars()
                            .rev()
                            .take(2_048)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect();
                        *buf = keep;
                    }
                }
                line.clear();
            }
        });
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if tx.send(Ok(line.clone())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("mcp read: {e}")));
                        break;
                    }
                }
            }
        });
        let mut proc = Self {
            child,
            stdin,
            rx,
            stderr_tail,
            next_id: 0,
            tools: Vec::new(),
            prompts: Vec::new(),
            resources: Vec::new(),
        };
        let _ = proc.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "twig", "version": env!("CARGO_PKG_VERSION")}
            }),
            init_timeout,
            control,
        )?;
        proc.notify("notifications/initialized", json!({}))?;
        proc.tools = proc
            .request("tools/list", json!({}), init_timeout, control)
            .ok()
            .and_then(|v| v["tools"].as_array().cloned())
            .unwrap_or_default();
        proc.prompts = proc
            .request("prompts/list", json!({}), init_timeout, control)
            .ok()
            .and_then(|v| v["prompts"].as_array().cloned())
            .unwrap_or_default();
        proc.resources = proc
            .request("resources/list", json!({}), init_timeout, control)
            .ok()
            .and_then(|v| v["resources"].as_array().cloned())
            .unwrap_or_default();
        Ok(proc)
    }

    fn is_exited(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_some()
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        writeln!(self.stdin, "{msg}").map_err(|e| format!("mcp write: {e}"))?;
        self.stdin.flush().map_err(|e| format!("mcp flush: {e}"))
    }

    fn format_err(&self, base: impl Into<String>) -> String {
        let base = base.into();
        let tail = self
            .stderr_tail
            .lock()
            .ok()
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        if tail.is_empty() {
            base
        } else {
            let last_lines: Vec<&str> = tail.lines().rev().take(6).collect();
            let snippet = last_lines.into_iter().rev().collect::<Vec<_>>().join(" | ");
            format!("{base} (stderr: {snippet})")
        }
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
        control: &Control,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").map_err(|e| self.format_err(format!("mcp write: {e}")))?;
        self.stdin
            .flush()
            .map_err(|e| self.format_err(format!("mcp flush: {e}")))?;
        let deadline = Instant::now() + timeout;
        let mut seen = 0usize;
        while seen < 256 {
            if control.cancelled() {
                return Err("mcp request cancelled".into());
            }
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return Err(
                    self.format_err(format!("mcp {method} timed out after {}ms", timeout.as_millis())),
                );
            }
            match self.rx.recv_timeout(rem.min(Duration::from_millis(50))) {
                Ok(Ok(line)) => {
                    seen += 1;
                    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
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
                Ok(Err(e)) => return Err(self.format_err(e)),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(15));
                    return Err(self.format_err("mcp server closed stdout"));
                }
            }
        }
        Err(self.format_err("mcp server did not respond"))
    }

    fn tool_summaries(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "name": t["name"].as_str().unwrap_or(""),
                    "description": t["description"].as_str().unwrap_or("")
                })
            })
            .collect()
    }

    fn prompt_summaries(&self) -> Vec<Value> {
        self.prompts
            .iter()
            .map(|p| {
                json!({
                    "name": p["name"].as_str().unwrap_or(""),
                    "description": p["description"].as_str().unwrap_or(""),
                    "arguments": p["arguments"]
                })
            })
            .collect()
    }

    fn resource_summaries(&self) -> Vec<Value> {
        self.resources
            .iter()
            .map(|r| {
                json!({
                    "uri": r["uri"].as_str().unwrap_or(""),
                    "name": r["name"].as_str().unwrap_or(""),
                    "description": r["description"].as_str().unwrap_or(""),
                    "mimeType": r["mimeType"]
                })
            })
            .collect()
    }

    fn tool_schemas_json(&self) -> String {
        let summary: Vec<Value> = self
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t["name"],
                    "description": t["description"],
                    "inputSchema": t["inputSchema"]
                })
            })
            .collect();
        serde_json::to_string_pretty(&summary).unwrap_or_default()
    }

    fn prompts_json(&self) -> String {
        serde_json::to_string_pretty(&self.prompt_summaries()).unwrap_or_default()
    }

    fn resources_json(&self) -> String {
        serde_json::to_string_pretty(&self.resource_summaries()).unwrap_or_default()
    }

    fn call_tool(
        &mut self,
        tool: &str,
        arguments: Value,
        timeout: Duration,
        control: &Control,
    ) -> Result<String, String> {
        let res = self.request(
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
            timeout,
            control,
        )?;
        let text = if let Some(items) = res["content"].as_array() {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|c| c["text"].as_str().map(str::to_owned))
                .collect();
            if parts.is_empty() {
                res.to_string()
            } else {
                parts.join("\n")
            }
        } else {
            res.to_string()
        };
        Ok(clip_mcp_text(text))
    }

    fn get_prompt(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Duration,
        control: &Control,
    ) -> Result<String, String> {
        if name.trim().is_empty() {
            return Err("get_prompt requires a prompt name".into());
        }
        let res = self.request(
            "prompts/get",
            json!({"name": name, "arguments": arguments}),
            timeout,
            control,
        )?;
        let mut out = String::new();
        if let Some(desc) = res["description"].as_str().filter(|s| !s.is_empty()) {
            out.push_str(desc);
            out.push_str("\n\n");
        }
        if let Some(msgs) = res["messages"].as_array() {
            for m in msgs {
                let role = m["role"].as_str().unwrap_or("user");
                let text = m["content"]["text"]
                    .as_str()
                    .or_else(|| m["content"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| m["content"].to_string());
                out.push_str(&format!("[{role}]\n{text}\n\n"));
            }
        }
        let body = if out.trim().is_empty() {
            res.to_string()
        } else {
            out.trim_end().to_owned()
        };
        Ok(clip_mcp_text(body))
    }

    fn read_resource(
        &mut self,
        uri: &str,
        timeout: Duration,
        control: &Control,
    ) -> Result<String, String> {
        if uri.trim().is_empty() {
            return Err("read_resource requires a resource uri".into());
        }
        let res = self.request("resources/read", json!({"uri": uri}), timeout, control)?;
        let text = if let Some(items) = res["contents"].as_array() {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|c| {
                    c["text"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| c["blob"].as_str().map(|b| format!("[base64 blob, {} bytes]", b.len())))
                })
                .collect();
            if parts.is_empty() {
                res.to_string()
            } else {
                parts.join("\n")
            }
        } else {
            res.to_string()
        };
        Ok(clip_mcp_text(text))
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct McpRegistry {
    worktree: PathBuf,
    configs: BTreeMap<String, McpServerConfig>,
    live: BTreeMap<String, McpProcess>,
    call_timeout: Duration,
    init_timeout: Duration,
}

impl McpRegistry {
    pub fn discover(worktree: &Path) -> Option<Self> {
        let mut configs = BTreeMap::new();
        for path in [
            paths::config_dir().join("mcp.json"),
            worktree.join(".twig/mcp.json"),
        ] {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(parsed) = serde_json::from_str::<McpFile>(&text) {
                for (name, cfg) in parsed.mcp_servers {
                    if !name.trim().is_empty() && !cfg.command.trim().is_empty() {
                        configs.insert(name, cfg);
                    }
                }
            }
        }
        (!configs.is_empty()).then(|| Self {
            worktree: worktree.to_path_buf(),
            configs,
            live: BTreeMap::new(),
            call_timeout: DEFAULT_MCP_TIMEOUT,
            init_timeout: INIT_MCP_TIMEOUT,
        })
    }

    pub fn server_names_summary(&self) -> String {
        self.configs
            .iter()
            .filter(|(_, c)| !c.disabled)
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn ensure_live(&mut self, server: &str, control: &Control) -> Result<&mut McpProcess, String> {
        if let Some(proc) = self.live.get_mut(server) {
            if proc.is_exited() {
                self.live.remove(server);
            }
        }
        if !self.live.contains_key(server) {
            let cfg = self
                .configs
                .get(server)
                .ok_or_else(|| format!("unknown MCP server {server:?}"))?
                .clone();
            if cfg.disabled {
                return Err(format!(
                    "MCP server {server:?} is currently disabled (run `/mcp enable {server}` to activate)"
                ));
            }
            let proc = McpProcess::spawn(&cfg, &self.worktree, self.init_timeout, control)?;
            self.live.insert(server.to_owned(), proc);
        }
        self.live
            .get_mut(server)
            .ok_or_else(|| format!("MCP server {server:?} unavailable"))
    }

    pub fn list_report(&mut self) -> Value {
        let names: Vec<String> = self.configs.keys().cloned().collect();
        let ctrl = Control::default();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            if self.configs.get(&name).is_some_and(|c| c.disabled) {
                out.push(json!({
                    "server": name,
                    "ok": true,
                    "disabled": true,
                    "tools": [],
                    "prompts": [],
                    "resources": []
                }));
                continue;
            }
            match self.ensure_live(&name, &ctrl) {
                Ok(proc) => {
                    let tools = proc.tool_summaries();
                    let prompts = proc.prompt_summaries();
                    let resources = proc.resource_summaries();
                    out.push(json!({
                        "server": name,
                        "ok": true,
                        "disabled": false,
                        "tools": tools,
                        "prompts": prompts,
                        "resources": resources
                    }));
                }
                Err(e) => out.push(json!({"server": name, "ok": false, "error": e})),
            }
        }
        json!({"servers": out})
    }

    /// Persist project-level MCP server toggles (`enable` / `disable` / `init_rea`) in `<worktree>/.twig/mcp.json`.
    pub fn configure_project_file(worktree: &Path, params: &Value) -> Result<String, String> {
        let action = params.get("action").and_then(Value::as_str).unwrap_or("list");
        let path = worktree.join(".twig/mcp.json");
        let mut parsed: McpFile = if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("read .twig/mcp.json: {e}"))?;
            serde_json::from_str(&text).map_err(|e| format!("parse .twig/mcp.json: {e}"))?
        } else {
            McpFile::default()
        };

        let summary = match action {
            "init_rea" => {
                parsed.mcp_servers.entry("rea".into()).or_insert_with(|| McpServerConfig {
                    command: "npx".into(),
                    args: vec!["-y".into(), "rea-agents@latest".into(), "mcp".into()],
                    env: BTreeMap::new(),
                    disabled: true,
                });
                "registered `rea` MCP in .twig/mcp.json (disabled by default — run `/mcp enable rea` when needed)".to_owned()
            }
            "enable" | "disable" => {
                let want_disabled = action == "disable";
                let name = params
                    .get("server")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| format!("`/mcp {action} <server>` requires a server name"))?;
                if let Some(cfg) = parsed.mcp_servers.get_mut(name) {
                    cfg.disabled = want_disabled;
                } else {
                    let global_path = paths::config_dir().join("mcp.json");
                    let global_cfg = std::fs::read_to_string(global_path)
                        .ok()
                        .and_then(|t| serde_json::from_str::<McpFile>(&t).ok())
                        .and_then(|mut g| g.mcp_servers.remove(name));
                    let Some(mut cfg) = global_cfg else {
                        return Err(format!(
                            "unknown MCP server {name:?} (tip: run `/mcp init-rea` to scaffold REA MCP)"
                        ));
                    };
                    cfg.disabled = want_disabled;
                    parsed.mcp_servers.insert(name.to_owned(), cfg);
                }
                format!(
                    "MCP server `{name}` {} (saved to .twig/mcp.json)",
                    if want_disabled { "disabled" } else { "enabled" }
                )
            }
            other => return Err(format!("unknown mcp config action {other:?}")),
        };

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let body = serde_json::to_string_pretty(&parsed).map_err(|e| e.to_string())?;
        std::fs::write(&path, format!("{body}\n")).map_err(|e| e.to_string())?;
        Ok(summary)
    }

    pub fn invoke(
        &mut self,
        server: &str,
        action: &str,
        target: &str,
        arguments: Value,
        control: &Control,
    ) -> Result<String, String> {
        let timeout = self.call_timeout;
        let res = {
            let proc = self.ensure_live(server, control)?;
            match action {
                "tools" => Ok(proc.tool_schemas_json()),
                "prompts" => Ok(proc.prompts_json()),
                "get_prompt" => proc.get_prompt(target, arguments, timeout, control),
                "resources" => Ok(proc.resources_json()),
                "read_resource" => proc.read_resource(target, timeout, control),
                "call" | "" => {
                    if target.trim().is_empty() {
                        Ok(proc.tool_schemas_json())
                    } else {
                        proc.call_tool(target, arguments, timeout, control)
                    }
                }
                other => Err(format!(
                    "unknown mcp action {other:?}; expected call, tools, prompts, get_prompt, resources, or read_resource"
                )),
            }
        };
        // Evict hung or broken servers (Drop kills the child) so future calls respawn cleanly.
        if let Err(ref msg) = res {
            if msg.contains("timed out")
                || msg.contains("closed stdout")
                || msg.contains("cancelled")
                || msg.contains("mcp write")
            {
                self.live.remove(server);
            }
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_project_mcp_json_servers_and_handles_tools_prompts_resources() {
        let dir = std::env::temp_dir().join(format!("twig-mcp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".twig")).unwrap();
        let script = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}\n' ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object"}}]}}\n' ;;
    *'"method":"prompts/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"prompts":[{"name":"review-skill","description":"Review checklist"}]}}\n' ;;
    *'"method":"resources/list"'*)
      printf '{"jsonrpc":"2.0","id":4,"result":{"resources":[{"uri":"mem://guide","name":"Guide","mimeType":"text/plain"}]}}\n' ;;
    *'"method":"prompts/get"'*)
      printf '{"jsonrpc":"2.0","id":5,"result":{"description":"Skill body","messages":[{"role":"user","content":{"type":"text","text":"Check edge cases"}}]}}\n' ;;
    *'"method":"resources/read"'*)
      printf '{"jsonrpc":"2.0","id":6,"result":{"contents":[{"uri":"mem://guide","text":"Always verify tests."}]}}\n' ;;
  esac
done
"#;
        let cfg = json!({
            "mcpServers": {
                "demo": {"command": "sh", "args": ["-c", script]}
            }
        });
        std::fs::write(dir.join(".twig/mcp.json"), cfg.to_string()).unwrap();

        let mut reg = McpRegistry::discover(&dir).unwrap();
        assert!(reg.server_names_summary().contains("demo"));
        let report = reg.list_report();
        let srv = &report["servers"][0];
        assert_eq!(srv["ok"], true);
        assert_eq!(srv["tools"][0]["name"], "echo");
        assert_eq!(srv["prompts"][0]["name"], "review-skill");
        assert_eq!(srv["resources"][0]["uri"], "mem://guide");

        let ctrl = Control::default();
        let prompt = reg
            .invoke("demo", "get_prompt", "review-skill", json!({}), &ctrl)
            .unwrap();
        assert!(prompt.contains("Check edge cases"), "{prompt}");

        let res = reg
            .invoke("demo", "read_resource", "mem://guide", json!({}), &ctrl)
            .unwrap();
        assert!(res.contains("Always verify tests."), "{res}");

        // Hung MCP server times out cleanly instead of blocking forever.
        let hung_cfg = json!({
            "mcpServers": {
                "hung": {"command": "sh", "args": ["-c", "sleep 30"]}
            }
        });
        std::fs::write(dir.join(".twig/mcp.json"), hung_cfg.to_string()).unwrap();
        let mut hung_reg = McpRegistry::discover(&dir).unwrap();
        hung_reg.init_timeout = Duration::from_millis(200);
        let err = hung_reg
            .invoke("hung", "tools", "", json!({}), &ctrl)
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");

        // Crashing MCP server surfaces stderr diagnostics AND runs in worktree cwd.
        std::fs::write(dir.join("marker.txt"), "in-worktree").unwrap();
        let crash_cfg = json!({
            "mcpServers": {
                "crash": {"command": "sh", "args": ["-c", "cat marker.txt >&2; echo 'missing API_TOKEN' >&2; exit 1"]}
            }
        });
        std::fs::write(dir.join(".twig/mcp.json"), crash_cfg.to_string()).unwrap();
        let mut crash_reg = McpRegistry::discover(&dir).unwrap();
        let crash_err = crash_reg
            .invoke("crash", "tools", "", json!({}), &ctrl)
            .unwrap_err();
        assert!(crash_err.contains("in-worktree"), "{crash_err}");
        assert!(crash_err.contains("missing API_TOKEN"), "{crash_err}");

        // Toggleable REA MCP (`init_rea` -> disabled by default -> `enable` -> `disable`).
        let init_msg =
            McpRegistry::configure_project_file(&dir, &json!({"action": "init_rea"})).unwrap();
        assert!(init_msg.contains("disabled by default"), "{init_msg}");
        let mut rea_reg = McpRegistry::discover(&dir).unwrap();
        assert!(!rea_reg.server_names_summary().contains("rea"));
        let rep = rea_reg.list_report();
        assert!(rep["servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["server"] == "rea" && s["disabled"] == true));
        let en_msg = McpRegistry::configure_project_file(
            &dir,
            &json!({"action": "enable", "server": "rea"}),
        )
        .unwrap();
        assert!(en_msg.contains("enabled"), "{en_msg}");
        let rea_enabled = McpRegistry::discover(&dir).unwrap();
        assert!(rea_enabled.server_names_summary().contains("rea"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
