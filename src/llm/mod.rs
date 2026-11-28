//! OpenAI-compatible client pointed at magpie, role-routing configuration,
//! and local CLI runner bridges (`cli`).
//!
//! Strict DAG: `mod` (Provider) -> `{cli, stream}` -> `types`; `stub` -> `types`.

pub(crate) mod cli;
pub mod stream;
pub mod stub;
pub mod types;

use cli::is_cli_model;
pub use types::{parse_reply, summarize_usage, Llm, Reply, Roles, ToolCall, Usage};

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::paths;
use crate::control::Control;

pub enum ConfigOp<'a> {
    RolesReport,
    ResetRoles,
    SetRole {
        role: &'a str,
        model: Option<&'a str>,
        effort: Option<&'a str>,
    },
    ResolveRole(&'a str),
    SaveModel(Option<&'a str>),
    SaveEffort(Option<&'a str>),
}
const DEFAULT_BASE: &str = "http://127.0.0.1:3425/v1";
const CANONICAL_ROLES: &[&str] = &["plan", "advisor", "code", "scout", "reviewer"];
const MAX_RETRIES: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RoleSpec {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
}

pub(crate) fn is_retryable_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500 | 502 | 503 | 504)
}

pub struct Provider {
    base: String,
    key: String,
    roles: Roles,
    role_efforts: HashMap<String, String>,
    effort: Mutex<Option<String>>,
    calls: AtomicU64,
    timeout: Duration,
}

pub fn normalize_effort(raw: &str) -> Result<Option<String>, String> {
    match raw.trim().to_lowercase().as_str() {
        "" | "off" | "none" | "default" | "reset" => Ok(None),
        level @ ("low" | "medium" | "high" | "xhigh") => Ok(Some(level.to_owned())),
        other => Err(format!(
            "invalid effort {other:?}; expected off, low, medium, high, or xhigh"
        )),
    }
}

fn canonicalize_role_name(raw: &str) -> Option<&'static str> {
    match raw.trim().to_lowercase().as_str() {
        "plan" | "planner" | "operator" => Some("plan"),
        "advisor" | "oracle" | "council" => Some("advisor"),
        "code" | "coder" | "worker" | "test" => Some("code"),
        "scout" | "filter" | "explore" => Some("scout"),
        "reviewer" | "review" | "judge" => Some("reviewer"),
        _ => None,
    }
}

fn roles_path() -> std::path::PathBuf {
    paths::config_dir().join("roles.json")
}

fn parse_role_entry(v: &Value) -> Option<RoleSpec> {
    if let Some(m) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        return Some(RoleSpec {
            model: m.to_owned(),
            effort: None,
        });
    }
    let m = v
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let effort = v
        .get("effort")
        .and_then(Value::as_str)
        .and_then(|e| normalize_effort(e).ok().flatten());
    Some(RoleSpec {
        model: m.to_owned(),
        effort,
    })
}

fn write_optional_file(path: &std::path::Path, value: Option<&str>) -> Result<(), String> {
    match value.map(str::trim).filter(|s| !s.is_empty()) {
        Some(v) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(path, format!("{v}\n")).map_err(|e| e.to_string())
        }
        None => {
            if path.exists() {
                std::fs::remove_file(path).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}

fn read_saved_effort() -> Option<String> {
    let text = std::fs::read_to_string(paths::state_dir().join("effort.txt")).ok()?;
    normalize_effort(&text).ok().flatten()
}

fn load_role_specs() -> BTreeMap<String, RoleSpec> {
    let Ok(text) = std::fs::read_to_string(roles_path()) else {
        return BTreeMap::new();
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (k, v) in map {
        if let (Some(canon), Some(spec)) = (canonicalize_role_name(&k), parse_role_entry(&v)) {
            out.insert(canon.to_owned(), spec);
        }
    }
    out
}

fn update_role_spec(
    role: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<BTreeMap<String, RoleSpec>, String> {
    let canon = canonicalize_role_name(role).ok_or_else(|| {
        format!(
            "unknown role {role:?}; expected one of: {}",
            CANONICAL_ROLES.join(", ")
        )
    })?;
    let mut specs = load_role_specs();
    match model.map(str::trim) {
        None | Some("") | Some("reset") | Some("default") => {
            specs.remove(canon);
        }
        Some(m) => {
            let eff = match effort {
                Some(raw) => normalize_effort(raw)?,
                None => specs.get(canon).and_then(|s| s.effort.clone()),
            };
            specs.insert(
                canon.to_owned(),
                RoleSpec {
                    model: m.to_owned(),
                    effort: eff,
                },
            );
        }
    }
    let path = roles_path();
    if specs.is_empty() {
        write_optional_file(&path, None)?;
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let body = serde_json::to_string_pretty(&specs).map_err(|e| e.to_string())?;
        std::fs::write(&path, format!("{body}\n")).map_err(|e| e.to_string())?;
    }
    Ok(specs)
}

fn build_roles_report() -> Value {
    let fresh = Provider::new();
    let saved = load_role_specs();
    let fallback_eff = fresh.effort();
    let mut rows_json = Vec::new();
    let mut lines = Vec::new();
    lines.push(
        "Role routing (`~/.config/twig/roles.json` · `/roles <role> <model> [effort]`):".to_owned(),
    );
    for &role in CANONICAL_ROLES {
        let model = fresh.roles().get(role).to_owned();
        let effort = fresh
            .role_effort(role)
            .or_else(|| fallback_eff.clone())
            .unwrap_or_else(|| "off".into());
        let source = if std::env::var_os("TWIG_MODEL").is_some() {
            "env TWIG_MODEL"
        } else if std::env::var_os("TWIG_ROLES").is_some() {
            "env TWIG_ROLES"
        } else if saved.contains_key(role) {
            "roles.json"
        } else if Provider::saved_model().is_some() {
            "model.txt"
        } else {
            "default"
        };
        rows_json.push(json!({
            "role": role,
            "model": model,
            "effort": effort,
            "source": source,
        }));
        lines.push(format!(
            "  {role:<10} {model:<32} effort:{effort:<6} ({source})"
        ));
    }
    json!({
        "roles": rows_json,
        "report": lines.join("\n"),
    })
}

impl Provider {
    pub fn new() -> Self {
        let base = std::env::var("TWIG_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE.to_string());
        let key = std::env::var("TWIG_API_KEY").unwrap_or_else(|_| "magpie".to_string());
        let env_model = std::env::var("TWIG_MODEL").ok().filter(|s| !s.trim().is_empty());
        let env_roles = std::env::var("TWIG_ROLES").ok().filter(|s| !s.trim().is_empty());

        let mut roles = Roles::default();
        let mut role_efforts = HashMap::new();

        if env_model.is_none() {
            if let Some(saved) = Self::saved_model() {
                for role in ["filter", "plan", "advisor", "code", "scout", "reviewer", "judge"] {
                    roles.0.insert(role.into(), saved.clone());
                }
            }
            for (role, spec) in load_role_specs() {
                roles.0.insert(role.clone(), spec.model.clone());
                if role == "scout" {
                    roles.0.insert("filter".into(), spec.model.clone());
                } else if role == "reviewer" {
                    roles.0.insert("judge".into(), spec.model.clone());
                }
                if let Some(eff) = spec.effort {
                    role_efforts.insert(role, eff);
                }
            }
        }

        if let Some(raw) = env_roles {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) {
                for (k, v) in map {
                    if let Some(s) = v.as_str() {
                        roles.0.insert(k, s.to_owned());
                    } else if let Some(m) = v.get("model").and_then(Value::as_str) {
                        roles.0.insert(k.clone(), m.to_owned());
                        if let Some(e) = v
                            .get("effort")
                            .and_then(Value::as_str)
                            .and_then(|x| normalize_effort(x).ok().flatten())
                        {
                            role_efforts.insert(k, e);
                        }
                    }
                }
            }
        }

        let effort = std::env::var("TWIG_EFFORT")
            .ok()
            .and_then(|v| normalize_effort(&v).ok().flatten())
            .or_else(read_saved_effort);
        Self {
            base: base.trim_end_matches('/').to_string(),
            key,
            roles,
            role_efforts,
            effort: Mutex::new(effort),
            calls: AtomicU64::new(0),
            timeout: Duration::from_secs(600),
        }
    }

    pub fn saved_model() -> Option<String> {
        let text = std::fs::read_to_string(paths::state_dir().join("model.txt")).ok()?;
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    /// Typed configuration façade for persisted default model, reasoning effort, and role routing.
    pub fn apply_config(op: ConfigOp<'_>) -> Result<Value, String> {
        match op {
            ConfigOp::RolesReport => Ok(build_roles_report()),
            ConfigOp::ResetRoles => {
                write_optional_file(&roles_path(), None)?;
                Ok(build_roles_report())
            }
            ConfigOp::SetRole {
                role,
                model,
                effort,
            } => {
                update_role_spec(role, model, effort)?;
                Ok(build_roles_report())
            }
            ConfigOp::ResolveRole(role) => {
                let specs = load_role_specs();
                let canon = canonicalize_role_name(role).unwrap_or(role);
                let pinned = specs.get(canon);
                Ok(json!({
                    "model": pinned.map(|s| s.model.clone()),
                    "effort": pinned.and_then(|s| s.effort.clone()),
                }))
            }
            ConfigOp::SaveModel(model) => {
                write_optional_file(&paths::state_dir().join("model.txt"), model)?;
                Ok(json!({"ok": true}))
            }
            ConfigOp::SaveEffort(effort) => {
                write_optional_file(&paths::state_dir().join("effort.txt"), effort)?;
                Ok(json!({"ok": true}))
            }
        }
    }

    /// Query `{base}/models` on magpie (or any OpenAI-compatible gateway) for `/model` picker.
    pub fn list_models(&self) -> Result<Vec<String>, String> {
        let response = ureq::get(&format!("{}/models", self.base))
            .set("Authorization", &format!("Bearer {}", self.key))
            .timeout(Duration::from_secs(3))
            .call();
        let text = match response {
            Ok(r) => r.into_string().map_err(|e| format!("read models: {e}"))?,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                let snippet: String = body.chars().take(200).collect();
                return Err(format!("magpie HTTP {code}: {snippet}"));
            }
            Err(e) => return Err(format!("magpie unreachable at {}: {e}", self.base)),
        };
        let body: Value =
            serde_json::from_str(&text).map_err(|e| format!("models JSON: {e}"))?;
        let mut ids: Vec<String> = body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    pub fn with_base(base: impl Into<String>) -> Self {
        let mut p = Self::new();
        p.base = base.into().trim_end_matches('/').to_string();
        p
    }

    pub fn base(&self) -> &str {
        &self.base
    }
}

impl Default for Provider {
    fn default() -> Self {
        Self::new()
    }
}

impl Llm for Provider {
    fn roles(&self) -> &Roles {
        &self.roles
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    fn effort(&self) -> Option<String> {
        self.effort.lock().ok().and_then(|e| e.clone())
    }

    fn role_effort(&self, role: &str) -> Option<String> {
        let canon = canonicalize_role_name(role).unwrap_or(role);
        self.role_efforts
            .get(canon)
            .or_else(|| self.role_efforts.get(role))
            .cloned()
    }

    fn set_effort(&self, effort: Option<&str>) {
        if let Ok(mut guard) = self.effort.lock() {
            *guard = effort.map(str::to_owned);
        }
    }

    fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: Option<&Value>,
    ) -> Result<Reply, String> {
        let eff = self.effort();
        if is_cli_model(model) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            return cli::complete(
                model,
                system,
                messages,
                tools,
                eff.as_deref(),
                self.timeout,
                None,
                None,
            );
        }
        let mut msgs = vec![json!({"role": "system", "content": system})];
        msgs.extend_from_slice(messages);

        let mut payload = json!({
            "model": model,
            "messages": msgs,
        });
        if let Some(level) = eff {
            payload["reasoning_effort"] = json!(level);
        } else {
            payload["temperature"] = json!(0.0);
        }
        if let Some(tools) = tools {
            payload["tools"] = tools.clone();
            payload["tool_choice"] = json!("auto");
        }

        self.calls.fetch_add(1, Ordering::Relaxed);
        let body_str = payload.to_string();
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15).min(self.timeout))
            .timeout_write(Duration::from_secs(30).min(self.timeout))
            .timeout_read(self.timeout)
            .build();
        let mut attempt = 0u32;
        let text = loop {
            let response = agent
                .post(&format!("{}/chat/completions", self.base))
                .set("Content-Type", "application/json")
                .set("Authorization", &format!("Bearer {}", self.key))
                .send_string(&body_str);
            match response {
                Ok(r) => break r.into_string().map_err(|e| format!("read body: {e}"))?,
                Err(ureq::Error::Status(code, r)) => {
                    let body = r.into_string().unwrap_or_default();
                    let snippet: String = body.chars().take(400).collect();
                    if is_retryable_status(code) && attempt < MAX_RETRIES {
                        attempt += 1;
                        std::thread::sleep(Duration::from_secs(1u64 << (attempt - 1)));
                        continue;
                    }
                    return Err(format!("magpie HTTP {code}: {snippet}"));
                }
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        attempt += 1;
                        std::thread::sleep(Duration::from_secs(1u64 << (attempt - 1)));
                        continue;
                    }
                    return Err(format!("magpie unreachable at {}: {e}", self.base));
                }
            }
        };
        let body: Value =
            serde_json::from_str(&text).map_err(|e| format!("magpie returned non-JSON: {e}"))?;
        parse_reply(&body)
    }

    fn stream(
        &self,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: Option<&Value>,
        control: &Control,
        emit: &mut dyn FnMut(Value),
    ) -> Result<Reply, String> {
        let eff = self.effort();
        if is_cli_model(model) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            return cli::complete(
                model,
                system,
                messages,
                tools,
                eff.as_deref(),
                self.timeout,
                Some(control),
                Some(emit),
            );
        }
        let mut msgs = vec![json!({"role": "system", "content": system})];
        msgs.extend_from_slice(messages);
        let mut payload = json!({
            "model": model,
            "messages": msgs,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if let Some(level) = eff {
            payload["reasoning_effort"] = json!(level);
        }
        if let Some(t) = tools {
            payload["tools"] = t.clone();
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        stream::complete(
            self.base.clone(),
            self.key.clone(),
            payload,
            self.timeout,
            control,
            emit,
        )
    }
}
