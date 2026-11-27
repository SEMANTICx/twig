//! OpenAI-compatible client pointed at magpie.
//!
//! Strict DAG: `mod` (Provider) -> `stream` -> `types`; `stub` -> `types`.

pub mod stream;
pub mod stub;
pub mod types;

pub use types::{parse_reply, summarize_usage, Llm, Reply, Roles, ToolCall, Usage};

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::paths;
use crate::control::Control;

pub const DEFAULT_BASE: &str = "http://127.0.0.1:3425/v1";
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

    /// Unified persisted model, effort, and role-routing configuration entrypoint.
    pub fn configure(params: &Value) -> Result<Value, String> {
        match params.get("action").and_then(Value::as_str).unwrap_or("get_roles") {
            "get_roles" => Ok(build_roles_report()),
            "reset_roles" => {
                write_optional_file(&roles_path(), None)?;
                Ok(build_roles_report())
            }
            "set_role" => {
                let role = params
                    .get("role")
                    .and_then(Value::as_str)
                    .ok_or("set_role requires `role`")?;
                let model = params.get("model").and_then(Value::as_str);
                let effort = params.get("effort").and_then(Value::as_str);
                update_role_spec(role, model, effort)?;
                Ok(build_roles_report())
            }
            "resolve_role" => {
                let role = params.get("role").and_then(Value::as_str).unwrap_or("code");
                let specs = load_role_specs();
                let canon = canonicalize_role_name(role).unwrap_or(role);
                let pinned = specs.get(canon);
                Ok(json!({
                    "model": pinned.map(|s| s.model.clone()),
                    "effort": pinned.and_then(|s| s.effort.clone()),
                }))
            }
            "save_model" => {
                write_optional_file(
                    &paths::state_dir().join("model.txt"),
                    params.get("model").and_then(Value::as_str),
                )?;
                Ok(json!({"ok": true}))
            }
            "save_effort" => {
                write_optional_file(
                    &paths::state_dir().join("effort.txt"),
                    params.get("effort").and_then(Value::as_str),
                )?;
                Ok(json!({"ok": true}))
            }
            other => Err(format!("unknown Provider::configure action {other:?}")),
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
        if model.starts_with("cli:") {
            self.calls.fetch_add(1, Ordering::Relaxed);
            return complete_via_cli(model, system, messages, tools, self.timeout, None, None);
        }
        let mut msgs = vec![json!({"role": "system", "content": system})];
        msgs.extend_from_slice(messages);

        let mut payload = json!({
            "model": model,
            "messages": msgs,
        });
        if let Some(level) = self.effort() {
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
        let mut attempt = 0u32;
        let text = loop {
            let response = ureq::post(&format!("{}/chat/completions", self.base))
                .set("Content-Type", "application/json")
                .set("Authorization", &format!("Bearer {}", self.key))
                .timeout(self.timeout)
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
        if model.starts_with("cli:") {
            self.calls.fetch_add(1, Ordering::Relaxed);
            return complete_via_cli(
                model,
                system,
                messages,
                tools,
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
        if let Some(level) = self.effort() {
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

fn format_cli_prompt(system: &str, messages: &[Value], read_only: bool) -> String {
    let mut sections = Vec::new();
    if read_only {
        sections.push("[Mode: strict read-only — do NOT modify, create, or delete files.]".to_owned());
    }
    let sys_trimmed = system.trim();
    if !sys_trimmed.is_empty() && sys_trimmed.len() <= 4_000 {
        sections.push(format!("[System]\n{sys_trimmed}"));
    }
    let mut tail = Vec::new();
    let mut budget = 48_000usize;
    for msg in messages.iter().rev() {
        let role = msg["role"].as_str().unwrap_or("user");
        let content = msg["content"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| msg["content"].to_string());
        let trimmed = content.trim();
        if trimmed.is_empty() || trimmed == "null" {
            continue;
        }
        let clipped: String = trimmed.chars().take(16_000).collect();
        let entry = format!("[{role}]\n{clipped}");
        if entry.len() > budget && !tail.is_empty() {
            break;
        }
        budget = budget.saturating_sub(entry.len());
        tail.push(entry);
    }
    tail.reverse();
    sections.extend(tail);
    sections.join("\n\n")
}

fn absorb_cli_stream_line(
    engine: &str,
    line: &str,
    acc_text: &mut String,
    usage: &mut Usage,
    emit: &mut Option<&mut dyn FnMut(Value)>,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if let Some(u) = v.get("usage") {
            let parsed = Usage::parse(u);
            if parsed.input_tokens.is_some() {
                usage.input_tokens = parsed.input_tokens;
            }
            if parsed.output_tokens.is_some() {
                usage.output_tokens = parsed.output_tokens;
            }
            if parsed.cached_input_tokens.is_some() {
                usage.cached_input_tokens = parsed.cached_input_tokens;
            }
        }
        match engine {
            "codex" => {
                if v["type"] == "item.completed" {
                    let item = &v["item"];
                    match item["type"].as_str() {
                        Some("agent_message") => {
                            if let Some(t) = item["text"].as_str().filter(|s| !s.is_empty()) {
                                if !acc_text.is_empty() {
                                    acc_text.push('\n');
                                }
                                acc_text.push_str(t);
                                if let Some(ref mut sink) = emit {
                                    sink(json!({"ev": "model.delta", "text": format!("{t}\n")}));
                                }
                            }
                        }
                        Some("reasoning") => {
                            if let Some(t) = item["text"].as_str().filter(|s| !s.is_empty()) {
                                if let Some(ref mut sink) = emit {
                                    sink(json!({"ev": "model.reasoning", "text": format!("{t}\n")}));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                return;
            }
            "claude" => {
                match v["type"].as_str() {
                    Some("stream_event") => {
                        let delta = &v["event"]["delta"];
                        match delta["type"].as_str() {
                            Some("text_delta") => {
                                if let Some(t) = delta["text"].as_str().filter(|s| !s.is_empty()) {
                                    acc_text.push_str(t);
                                    if let Some(ref mut sink) = emit {
                                        sink(json!({"ev": "model.delta", "text": t}));
                                    }
                                }
                            }
                            Some("thinking_delta") => {
                                if let Some(t) =
                                    delta["thinking"].as_str().filter(|s| !s.is_empty())
                                {
                                    if let Some(ref mut sink) = emit {
                                        sink(json!({"ev": "model.reasoning", "text": t}));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("assistant") if acc_text.is_empty() => {
                        for block in v["message"]["content"].as_array().into_iter().flatten() {
                            if block["type"] == "text" {
                                if let Some(t) = block["text"].as_str().filter(|s| !s.is_empty()) {
                                    acc_text.push_str(t);
                                    if let Some(ref mut sink) = emit {
                                        sink(json!({"ev": "model.delta", "text": t}));
                                    }
                                }
                            }
                        }
                    }
                    Some("result") if acc_text.is_empty() => {
                        if let Some(r) = v["result"].as_str().filter(|s| !s.is_empty()) {
                            acc_text.push_str(r);
                            if let Some(ref mut sink) = emit {
                                sink(json!({"ev": "model.delta", "text": r}));
                            }
                        }
                    }
                    _ => {}
                }
                return;
            }
            _ => {}
        }
    }

    // Plain-text fallback (`gemini -p` or non-JSON lines): stream line immediately.
    if !acc_text.is_empty() {
        acc_text.push('\n');
    }
    acc_text.push_str(line.trim_end_matches(['\r', '\n']));
    if let Some(ref mut sink) = emit {
        sink(json!({"ev": "model.delta", "text": format!("{}\n", line.trim_end_matches(['\r', '\n']))}));
    }
}

fn complete_via_cli(
    cli_spec: &str,
    system: &str,
    messages: &[Value],
    tools: Option<&Value>,
    timeout: Duration,
    control: Option<&Control>,
    mut emit: Option<&mut dyn FnMut(Value)>,
) -> Result<Reply, String> {
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Instant;

    let rest = cli_spec.strip_prefix("cli:").unwrap_or(cli_spec);
    let (engine, submodel) = match rest.split_once(':') {
        Some((e, m)) if !m.trim().is_empty() => (e.trim(), Some(m.trim())),
        _ => (rest.trim(), None),
    };
    let read_only = tools.is_none()
        || system.contains("Plan mode")
        || system.contains("read-only")
        || messages.iter().any(|m| {
            m["role"] == "system"
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("Plan mode is active") || s.contains("read-only"))
        });
    let prompt = format_cli_prompt(system, messages, read_only);
    let streaming = emit.is_some();

    let mut cmd = match engine {
        "codex" => {
            let mut c = Command::new("codex");
            c.args(["exec", "--skip-git-repo-check"]);
            if streaming {
                c.arg("--json");
            }
            if read_only {
                c.args(["--sandbox", "read-only"]);
            } else {
                c.arg("--full-auto");
            }
            if let Some(m) = submodel {
                c.args(["-m", m]);
            }
            c.arg(&prompt);
            c
        }
        "claude" => {
            let mut c = Command::new("claude");
            c.args(["-p", &prompt]);
            if streaming {
                c.args(["--output-format", "stream-json", "--verbose"]);
            }
            if read_only {
                c.args(["--permission-mode", "plan"]);
            } else {
                c.arg("--dangerously-skip-permissions");
            }
            if let Some(m) = submodel {
                c.args(["--model", m]);
            }
            c
        }
        "gemini" => {
            let mut c = Command::new("gemini");
            c.args(["-p", &prompt]);
            if !read_only {
                c.arg("--yolo");
            }
            if let Some(m) = submodel {
                c.args(["-m", m]);
            }
            c
        }
        other => {
            return Err(format!(
                "unsupported CLI model {other:?}; expected cli:codex, cli:claude, or cli:gemini"
            ));
        }
    };

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {cli_spec}: {e}"))?;
    let out_pipe = child.stdout.take().ok_or("cli missing stdout")?;
    let mut err_pipe = child.stderr.take().ok_or("cli missing stderr")?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(out_pipe).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = err_pipe.read_to_string(&mut buf);
        buf
    });

    let mut acc_text = String::new();
    let mut usage = Usage::default();
    let started = Instant::now();
    let status = loop {
        while let Ok(line) = rx.try_recv() {
            absorb_cli_stream_line(engine, &line, &mut acc_text, &mut usage, &mut emit);
        }
        if control.is_some_and(Control::cancelled) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cancelled".into());
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{cli_spec} timed out after {}s", timeout.as_secs()));
        }
        match child.try_wait().map_err(|e| format!("wait {cli_spec}: {e}"))? {
            Some(s) => break s,
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    while let Ok(line) = rx.try_recv() {
        absorb_cli_stream_line(engine, &line, &mut acc_text, &mut usage, &mut emit);
    }

    let stderr = err_handle.join().unwrap_or_default();
    if !status.success() {
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            acc_text.trim()
        };
        let clipped: String = detail.chars().take(600).collect();
        return Err(format!("{cli_spec} failed ({status}): {clipped}"));
    }
    Ok(Reply {
        text: acc_text.trim().to_owned(),
        calls: Vec::new(),
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_stream_parser_extracts_codex_and_claude_deltas_and_token_usage() {
        let mut codex_text = String::new();
        let mut codex_usage = Usage::default();
        let mut events = Vec::new();
        {
            let mut sink_fn = |ev: Value| events.push(ev);
            let mut opt_sink: Option<&mut dyn FnMut(Value)> = Some(&mut sink_fn);
            absorb_cli_stream_line(
                "codex",
                r#"{"type":"item.completed","item":{"type":"reasoning","text":"checking invariants"}}"#,
                &mut codex_text,
                &mut codex_usage,
                &mut opt_sink,
            );
            absorb_cli_stream_line(
                "codex",
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"fixed parser"}}"#,
                &mut codex_text,
                &mut codex_usage,
                &mut opt_sink,
            );
            absorb_cli_stream_line(
                "codex",
                r#"{"type":"turn.completed","usage":{"input_tokens":1200,"cached_input_tokens":900,"output_tokens":85}}"#,
                &mut codex_text,
                &mut codex_usage,
                &mut opt_sink,
            );
        }
        assert_eq!(codex_text, "fixed parser");
        assert_eq!(codex_usage.input_tokens, Some(1200));
        assert_eq!(codex_usage.cached_input_tokens, Some(900));
        assert_eq!(codex_usage.output_tokens, Some(85));
        assert_eq!(events.len(), 2);

        let mut claude_text = String::new();
        let mut claude_usage = Usage::default();
        let mut no_sink: Option<&mut dyn FnMut(Value)> = None;
        absorb_cli_stream_line(
            "claude",
            r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"hello "}}}"#,
            &mut claude_text,
            &mut claude_usage,
            &mut no_sink,
        );
        absorb_cli_stream_line(
            "claude",
            r#"{"type":"result","result":"hello world","usage":{"input_tokens":500,"cache_read_input_tokens":400,"output_tokens":40}}"#,
            &mut claude_text,
            &mut claude_usage,
            &mut no_sink,
        );
        assert_eq!(claude_text, "hello ");
        assert_eq!(claude_usage.input_tokens, Some(500));
        assert_eq!(claude_usage.cached_input_tokens, Some(400));
        assert_eq!(claude_usage.output_tokens, Some(40));
    }
}
