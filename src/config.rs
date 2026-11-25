//! The tunable surface and portable runtime paths.
//!
//! Prompts, tool descriptions and a handful of bounded limits are the only
//! thing `twig improve` may change. Code, the deny list, the verifier and the
//! scoring set are deliberately out of reach: a loop that can edit its own
//! exam learns to loosen the exam.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_SYSTEM: &str = "\
You are a coding agent working inside one directory.

Work in small steps. Read before you write. After any change, run something that
proves it works; a claim without a command output is not evidence.

Do not ask for permission for ordinary work. Do not explain at length what you are
about to do; do it and show the result.";

/// Runtime paths never refer to the compilation checkout.
pub mod paths {
    use std::path::PathBuf;

    pub fn home() -> PathBuf {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn config_dir() -> PathBuf {
        std::env::var_os("TWIG_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
                    return PathBuf::from(xdg).join("twig");
                }
                #[cfg(windows)]
                if let Some(appdata) = std::env::var_os("APPDATA") {
                    return PathBuf::from(appdata).join("twig");
                }
                home().join(".config").join("twig")
            })
    }

    pub fn state_dir() -> PathBuf {
        std::env::var_os("TWIG_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
                    return PathBuf::from(xdg).join("twig");
                }
                #[cfg(windows)]
                if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                    return PathBuf::from(local).join("twig").join("state");
                }
                home().join(".local").join("state").join("twig")
            })
    }

    pub fn data_dir() -> PathBuf {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(xdg);
        }
        #[cfg(windows)]
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local);
        }
        home().join(".local").join("share")
    }

    pub fn config_path() -> PathBuf {
        std::env::var_os("TWIG_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_dir().join("harness.json"))
    }

    pub fn session_id() -> String {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}-{n:x}", std::process::id())
    }

    pub fn valid_session(id: &str) -> bool {
        !id.is_empty()
            && id.len() < 100
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessConfig {
    pub system: String,
    pub bash_description: String,
    pub read_description: String,
    pub max_output: usize,
    pub read_max_lines: usize,
    pub bash_timeout_secs: u64,
    pub max_turns: u32,
    pub stall_turns: u32,
    pub context_max_bytes: usize,
    pub isolation: bool,
    pub network: bool,
    pub subagent_concurrency: usize,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            system: DEFAULT_SYSTEM.into(),
            bash_description: "Run a shell command in the worktree for building, testing, git, and CLI tasks (prefer read/edit/write/grep/glob for file access)."
                .into(),
            read_description: "Read a text file inside the worktree.".into(),
            max_output: 8000,
            read_max_lines: 200,
            bash_timeout_secs: 120,
            max_turns: 40,
            stall_turns: 6,
            context_max_bytes: 120_000,
            isolation: false,
            network: true,
            subagent_concurrency: 3,
        }
    }
}

/// Out-of-range values are rejected, never clamped: a clamped proposal is a
/// different proposal from the one that was argued for.
const BOUNDS: &[(&str, u64, u64)] = &[
    ("max_output", 1000, 64_000),
    ("read_max_lines", 50, 2000),
    ("bash_timeout_secs", 5, 600),
    ("max_turns", 5, 80),
    ("stall_turns", 2, 20),
];

impl HarnessConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(8_000..=2_000_000).contains(&self.context_max_bytes) {
            return Err("context_max_bytes must be 8000..2000000".into());
        }
        if !(1..=8).contains(&self.subagent_concurrency) {
            return Err("subagent_concurrency must be 1..8".into());
        }
        let values = [
            self.max_output as u64,
            self.read_max_lines as u64,
            self.bash_timeout_secs,
            self.max_turns as u64,
            self.stall_turns as u64,
        ];
        for ((name, lo, hi), v) in BOUNDS.iter().zip(values) {
            if v < *lo || v > *hi {
                return Err(format!("{name}={v} outside {lo}..={hi}"));
            }
        }
        for (name, text, limit) in [
            ("system", &self.system, 6000),
            ("bash_description", &self.bash_description, 1500),
            ("read_description", &self.read_description, 1500),
        ] {
            if text.trim().is_empty() || text.len() > limit {
                return Err(format!(
                    "{name} must be 1..={limit} bytes, got {}",
                    text.len()
                ));
            }
        }
        Ok(())
    }

    /// Apply a partial proposal on top of `self`. Omitted fields keep their
    /// current value; a missing field must never silently reset to the default.
    pub fn merged(&self, patch: &Value) -> Result<Self, String> {
        let mut base = serde_json::to_value(self).map_err(|e| e.to_string())?;
        let (Value::Object(base_map), Value::Object(patch_map)) = (&mut base, patch) else {
            return Err("config proposal must be a JSON object".into());
        };
        for (k, v) in patch_map {
            base_map.insert(k.clone(), v.clone());
        }
        let cfg: Self = serde_json::from_value(base).map_err(|e| format!("config: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg: Self =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        self.validate().map_err(std::io::Error::other)?;
        let body = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, body + "\n")
    }

    /// `TWIG_CONFIG` if set; otherwise the XDG config file if present; otherwise defaults.
    pub fn resolve() -> Result<(Self, String), String> {
        if let Ok(path) = std::env::var("TWIG_CONFIG") {
            return Ok((Self::load(Path::new(&path))?, path));
        }
        let default_path = paths::config_path();
        if default_path.exists() {
            let label = default_path.display().to_string();
            return Ok((Self::load(&default_path)?, label));
        }
        Ok((Self::default(), "defaults".into()))
    }

    /// What changed, one entry per field, for the decision trail.
    pub fn diff(&self, other: &Self) -> Vec<String> {
        let a = serde_json::to_value(self).unwrap_or_default();
        let b = serde_json::to_value(other).unwrap_or_default();
        let mut out = Vec::new();
        if let (Some(a), Some(b)) = (a.as_object(), b.as_object()) {
            for (k, va) in a {
                let vb = &b[k];
                if va == vb {
                    continue;
                }
                match (va.as_str(), vb.as_str()) {
                    (Some(x), Some(y)) => {
                        out.push(format!("{k}: text {} -> {} bytes", x.len(), y.len()))
                    }
                    _ => out.push(format!("{k}: {va} -> {vb}")),
                }
            }
        }
        out
    }
}

pub fn project_config_path() -> PathBuf {
    paths::config_path()
}
