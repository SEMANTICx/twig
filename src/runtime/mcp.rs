//! Minimal stdio JSON-RPC 2.0 MCP (Model Context Protocol) client.
//!
//! Discovers `mcpServers` from `<worktree>/.twig/mcp.json` and
//! `~/.config/twig/mcp.json` (project overrides global), spawns servers lazily
//! on first inspection or call, and cleans up child processes on drop.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::paths;

const MAX_MCP_OUTPUT_BYTES: usize = 64_000;

#[derive(Debug, Clone, Deserialize)]
struct McpFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct McpServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    tools: Vec<Value>,
}

impl McpProcess {
    fn spawn(cfg: &McpServerConfig) -> Result<Self, String> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn mcp {:?}: {e}", cfg.command))?;
        let stdin = child.stdin.take().ok_or("mcp missing stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("mcp missing stdout")?);
        let mut proc = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
            tools: Vec::new(),
        };
        let _ = proc.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "twig", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        proc.notify("notifications/initialized", json!({}))?;
        let listed = proc.request("tools/list", json!({}))?;
        proc.tools = listed["tools"].as_array().cloned().unwrap_or_default();
        Ok(proc)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        writeln!(self.stdin, "{msg}").map_err(|e| format!("mcp write: {e}"))?;
        self.stdin.flush().map_err(|e| format!("mcp flush: {e}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").map_err(|e| format!("mcp write: {e}"))?;
        self.stdin.flush().map_err(|e| format!("mcp flush: {e}"))?;
        let mut line = String::new();
        for _ in 0..256 {
            line.clear();
            let n = self
                .stdout
                .read_line(&mut line)
                .map_err(|e| format!("mcp read: {e}"))?;
            if n == 0 {
                return Err("mcp server closed stdout".into());
            }
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
        Err("mcp server did not respond".into())
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
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct McpRegistry {
    configs: BTreeMap<String, McpServerConfig>,
    live: BTreeMap<String, McpProcess>,
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
            configs,
            live: BTreeMap::new(),
        })
    }

    pub fn server_names_summary(&self) -> String {
        self.configs
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn ensure_live(&mut self, server: &str) -> Result<&mut McpProcess, String> {
        if !self.live.contains_key(server) {
            let cfg = self
                .configs
                .get(server)
                .ok_or_else(|| format!("unknown MCP server {server:?}"))?
                .clone();
            let proc = McpProcess::spawn(&cfg)?;
            self.live.insert(server.to_owned(), proc);
        }
        self.live
            .get_mut(server)
            .ok_or_else(|| format!("MCP server {server:?} unavailable"))
    }

    pub fn list_report(&mut self) -> Value {
        let names: Vec<String> = self.configs.keys().cloned().collect();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            match self.ensure_live(&name) {
                Ok(proc) => {
                    let tools = proc.tool_summaries();
                    out.push(json!({"server": name, "ok": true, "tools": tools}));
                }
                Err(e) => out.push(json!({"server": name, "ok": false, "error": e})),
            }
        }
        json!({"servers": out})
    }

    pub fn call(&mut self, server: &str, tool: &str, arguments: Value) -> Result<String, String> {
        if tool.trim().is_empty() {
            let proc = self.ensure_live(server)?;
            return Ok(proc.tool_schemas_json());
        }
        let proc = self.ensure_live(server)?;
        let res = proc.request("tools/call", json!({"name": tool, "arguments": arguments}))?;
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
        if text.len() > MAX_MCP_OUTPUT_BYTES {
            let clipped: String = text.chars().take(MAX_MCP_OUTPUT_BYTES).collect();
            Ok(format!("{clipped}\n... [mcp output truncated]"))
        } else {
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_project_mcp_json_servers() {
        let dir = std::env::temp_dir().join(format!("twig-mcp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".twig")).unwrap();
        std::fs::write(
            dir.join(".twig/mcp.json"),
            r#"{"mcpServers":{"demo":{"command":"echo","args":["hi"]}}}"#,
        )
        .unwrap();
        let reg = McpRegistry::discover(&dir).unwrap();
        assert!(reg.server_names_summary().contains("demo"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
