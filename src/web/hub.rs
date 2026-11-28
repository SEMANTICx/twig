//! Studio Configuration, Multi-Project Workspace Registry, Scheduled Routines,
//! Curated Customize/Plugin Packs, and GitHub Automation Queue handlers for `twig web`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{broadcast_sse, dispatch_background_turn, url_decode_simple, WebState};
use crate::automation::{Automation, AutomationConfig, ShippingPermissions, SourceKind, WorkItem};
use crate::config::{paths, HarnessConfig};
use crate::llm::{cli, ConfigOp, Llm, Provider};
use crate::runtime::guidance::SkillStore;
use crate::runtime::mcp::McpRegistry;
use crate::runtime::session::scan_sessions;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn expand_home(raw: &str) -> PathBuf {
    let t = raw.trim();
    if let Some(rest) = t.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(t)
}

/* ─── 1. Multi-Project Workspace Registry (Claude Projects + Codex Projects) ─── */

fn projects_path() -> PathBuf {
    paths::config_dir().join("projects.json")
}

fn load_saved_project_paths() -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(projects_path()) else {
        return Vec::new();
    };
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v["projects"].as_array().cloned())
        .map(|arr| {
            arr.into_iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn save_project_paths(list: &[String]) -> Result<(), String> {
    let p = projects_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(&json!({"projects": list})).map_err(|e| e.to_string())?;
    std::fs::write(&p, format!("{body}\n")).map_err(|e| e.to_string())
}

pub(super) fn collect_projects(state: &Arc<WebState>) -> Value {
    let mut seen = BTreeSet::new();
    let mut ordered = Vec::new();
    let def = state.default_repo.display().to_string();
    if seen.insert(def.clone()) {
        ordered.push(def);
    }
    for saved in load_saved_project_paths() {
        if Path::new(&saved).is_dir() && seen.insert(saved.clone()) {
            ordered.push(saved);
        }
    }
    for s in scan_sessions(None, 30) {
        if let Some(wt) = s.worktree {
            if !wt.contains(".twig-worktrees") && Path::new(&wt).is_dir() && seen.insert(wt.clone()) {
                ordered.push(wt);
            }
        }
    }
    let items: Vec<Value> = ordered
        .into_iter()
        .map(|p| {
            let path = Path::new(&p);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.clone());
            let branch = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .unwrap_or_default();
            json!({
                "path": p,
                "name": name,
                "branch": branch,
                "has_agents_md": path.join("AGENTS.md").is_file() || path.join("CLAUDE.md").is_file(),
                "has_prefs": path.join(".twig/preferences.md").is_file(),
            })
        })
        .collect();
    json!({ "projects": items })
}

pub(super) fn apply_projects(req: &Value, _state: &Arc<WebState>) -> Result<Value, String> {
    let action = req["action"].as_str().unwrap_or("add");
    let raw = req["path"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("project path required")?;
    let mut list = load_saved_project_paths();
    match action {
        "add" => {
            let expanded = expand_home(raw);
            let canon = expanded
                .canonicalize()
                .map_err(|_| format!("directory not found: {}", expanded.display()))?;
            if !canon.is_dir() {
                return Err("path is not a directory".into());
            }
            let s = canon.display().to_string();
            if !list.contains(&s) {
                list.push(s.clone());
                save_project_paths(&list)?;
            }
            Ok(json!({"ok": true, "path": s}))
        }
        "remove" => {
            list.retain(|x| x != raw);
            save_project_paths(&list)?;
            Ok(json!({"ok": true}))
        }
        other => Err(format!("unknown project action {other:?}")),
    }
}

/* ─── 2. Local Recurring Scheduled Routines (Claude Scheduled + Codex Automations) ─── */

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScheduleItem {
    id: String,
    name: String,
    worktree: String,
    interval_mins: u64,
    prompt: String,
    role: String,
    enabled: bool,
    #[serde(default)]
    last_run: u64,
    #[serde(default)]
    last_sid: Option<String>,
}

fn schedules_path() -> PathBuf {
    paths::config_dir().join("schedules.json")
}

fn load_schedules() -> Vec<ScheduleItem> {
    let Ok(raw) = std::fs::read_to_string(schedules_path()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<ScheduleItem>>(&raw).unwrap_or_default()
}

fn save_schedules(items: &[ScheduleItem]) -> Result<(), String> {
    let p = schedules_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(items).map_err(|e| e.to_string())?;
    std::fs::write(&p, format!("{body}\n")).map_err(|e| e.to_string())
}

pub(super) fn collect_schedules(_state: &Arc<WebState>) -> Value {
    json!({ "schedules": load_schedules() })
}

pub(super) fn apply_schedules(req: &Value, state: &Arc<WebState>) -> Result<Value, String> {
    let action = req["action"].as_str().ok_or("action required")?;
    let mut items = load_schedules();
    match action {
        "add" => {
            let name = req["name"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("routine name required")?;
            let prompt = req["prompt"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("routine prompt required")?;
            let interval_mins = req["interval_mins"].as_u64().unwrap_or(60).clamp(5, 10_080);
            let worktree = req["worktree"]
                .as_str()
                .filter(|s| !s.is_empty() && Path::new(s).is_dir())
                .map(str::to_owned)
                .unwrap_or_else(|| state.default_repo.display().to_string());
            let role = req["role"]
                .as_str()
                .filter(|s| matches!(*s, "code" | "plan"))
                .unwrap_or("plan")
                .to_owned();
            let id = format!("sch-{:x}", now_secs());
            items.push(ScheduleItem {
                id: id.clone(),
                name: name.to_owned(),
                worktree,
                interval_mins,
                prompt: prompt.to_owned(),
                role,
                enabled: true,
                last_run: now_secs(),
                last_sid: None,
            });
            save_schedules(&items)?;
            broadcast_sse(state, &json!({"ev": "schedule.updated"}));
            Ok(json!({"ok": true, "id": id}))
        }
        "toggle" => {
            let id = req["id"].as_str().ok_or("id required")?;
            let it = items.iter_mut().find(|x| x.id == id).ok_or("schedule not found")?;
            it.enabled = !it.enabled;
            if it.enabled && it.last_run == 0 {
                it.last_run = now_secs();
            }
            save_schedules(&items)?;
            broadcast_sse(state, &json!({"ev": "schedule.updated"}));
            Ok(json!({"ok": true}))
        }
        "remove" => {
            let id = req["id"].as_str().ok_or("id required")?;
            items.retain(|x| x.id != id);
            save_schedules(&items)?;
            broadcast_sse(state, &json!({"ev": "schedule.updated"}));
            Ok(json!({"ok": true}))
        }
        "run_now" => {
            let id = req["id"].as_str().ok_or("id required")?;
            let idx = items.iter().position(|x| x.id == id).ok_or("schedule not found")?;
            let dispatch_req = json!({
                "worktree": items[idx].worktree,
                "role": items[idx].role,
                "prompt": format!("[Scheduled · {}] {}", items[idx].name, items[idx].prompt),
            });
            let res = dispatch_background_turn(&dispatch_req, state)?;
            let sid = res["sid"].as_str().map(str::to_owned);
            items[idx].last_run = now_secs();
            items[idx].last_sid = sid.clone();
            save_schedules(&items)?;
            broadcast_sse(state, &json!({"ev": "schedule.updated", "sid": sid}));
            Ok(json!({"ok": true, "sid": sid}))
        }
        other => Err(format!("unknown schedule action {other:?}")),
    }
}

pub(super) fn spawn_scheduler_loop(state: Arc<WebState>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(30));
        let mut items = load_schedules();
        if items.is_empty() {
            continue;
        }
        let now = now_secs();
        let mut changed = false;
        for it in &mut items {
            if !it.enabled {
                continue;
            }
            let due_at = it.last_run.saturating_add(it.interval_mins.max(5) * 60);
            if now >= due_at {
                let req = json!({
                    "worktree": it.worktree,
                    "role": it.role,
                    "prompt": format!("[Scheduled · {}] {}", it.name, it.prompt),
                });
                if let Ok(res) = dispatch_background_turn(&req, &state) {
                    it.last_sid = res["sid"].as_str().map(str::to_owned);
                }
                it.last_run = now;
                changed = true;
            }
        }
        if changed {
            let _ = save_schedules(&items);
            broadcast_sse(&state, &json!({"ev": "schedule.updated"}));
        }
    });
}

/* ─── 3. Studio Settings, Customize Presets & Curated Plugin Packs ─── */

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

pub(super) fn collect_settings(query: &str, default_repo: &Path) -> Result<Value, String> {
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
    let agents_doc = std::fs::read_to_string(wt.join("AGENTS.md"))
        .or_else(|_| std::fs::read_to_string(wt.join("CLAUDE.md")))
        .unwrap_or_default();

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
            "agents_md": agents_doc,
        }
    }))
}

fn detect_repo_checks(wt: &Path) -> (&'static str, &'static str) {
    if wt.join("Cargo.toml").is_file() {
        ("cargo check --quiet", "cargo test --quiet")
    } else if wt.join("package.json").is_file() {
        ("npm run build --if-present", "npm test")
    } else if wt.join("pyproject.toml").is_file() {
        ("python3 -m py_compile", "pytest -q")
    } else if wt.join("go.mod").is_file() {
        ("go vet ./...", "go test ./...")
    } else {
        ("", "git diff --check")
    }
}

pub(super) fn apply_settings(req: &Value, default_repo: &Path) -> Result<Value, String> {
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
        "bundle" => {
            let pack = req["pack"].as_str().ok_or("pack required")?;
            match pack {
                "verify_hooks" => {
                    let (post_cmd, check_cmd) = detect_repo_checks(&wt);
                    let pj_path = wt.join(".twig/project.json");
                    let mut pj: Value = std::fs::read_to_string(&pj_path)
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_else(|| json!({}));
                    if let Some(obj) = pj.as_object_mut() {
                        if !post_cmd.is_empty() {
                            obj.insert("post_edit_check".into(), json!(post_cmd));
                        }
                        obj.insert("check".into(), json!(check_cmd));
                    }
                    if let Some(parent) = pj_path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    std::fs::write(&pj_path, format!("{}\n", serde_json::to_string_pretty(&pj).map_err(|e| e.to_string())?))
                        .map_err(|e| e.to_string())?;
                    Ok(json!({"ok": true, "summary": format!("Configured auto-detected hooks ({check_cmd})")}))
                }
                "rea_pack" => {
                    let msg = McpRegistry::configure_project_file(&wt, &json!({"action": "init_rea"}))?;
                    Ok(json!({"ok": true, "summary": msg}))
                }
                "cross_review" => {
                    let runners = cli::detected_runners();
                    let target = runners
                        .iter()
                        .find(|r| r.contains("codex") || r.contains("claude"))
                        .cloned()
                        .unwrap_or_else(|| Provider::new().roles().get("code").to_owned());
                    Provider::apply_config(ConfigOp::SetRole {
                        role: "reviewer",
                        model: Some(&target),
                        effort: Some("high"),
                    })?;
                    Ok(json!({"ok": true, "summary": format!("Routed `reviewer` role to {target} (high effort)")}))
                }
                other => Err(format!("unknown bundle pack {other:?}")),
            }
        }
        other => Err(format!("unknown settings section {other:?}")),
    }
}

/* ─── 4. Durable GitHub Queue Automation ─── */

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

pub(super) fn collect_automations(query: &str, state: &Arc<WebState>) -> Value {
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

pub(super) fn apply_automations(req: &Value, state: &Arc<WebState>) -> Result<Value, String> {
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
            let number = req["number"].as_u64().filter(|&n| n > 0).unwrap_or_else(|| now_secs() % 100_000);
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
