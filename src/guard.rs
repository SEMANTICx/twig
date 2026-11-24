//! Progress and budget guard.
//!
//! Progress is a workspace digest change, never a repetition pattern. Pattern
//! matching both false-positives (a long `write_stdin` wait reads as a loop) and
//! false-negatives (repeats interleaved with system messages slip through), so a
//! budget check is strictly better at any cost.
//!
//! A guard never ends a turn. It degrades: `Ok` -> `Warn` -> `Narrow` -> `Abort`.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Narrow,
    Abort,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Narrow => "narrow",
            Level::Abort => "abort",
        }
    }
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "__pycache__",
    ".venv",
    "target",
    "dist",
    ".twig-worktrees",
];

pub struct Guard {
    worktree: PathBuf,
    /// Anything the harness itself writes must be excluded, or every record
    /// append reads as progress and a stall can never be detected.
    ignore: Vec<PathBuf>,
    max_turns: u32,
    stall_turns: u32,
    sample_every: u32,
    digest: Option<String>,
    stall: u32,
    samples: u32,
}

impl Guard {
    pub fn new(worktree: impl Into<PathBuf>, max_turns: u32, stall_turns: u32, sample_every: u32) -> Self {
        Self {
            worktree: worktree.into(),
            ignore: Vec::new(),
            max_turns,
            stall_turns,
            sample_every: sample_every.max(1),
            digest: None,
            stall: 0,
            samples: 0,
        }
    }

    pub fn ignoring(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.ignore = paths
            .into_iter()
            .filter_map(|p| p.canonicalize().ok())
            .collect();
        self
    }

    fn ignored(&self, path: &Path) -> bool {
        let real = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => return false,
        };
        self.ignore.iter().any(|ig| real == *ig || real.starts_with(ig))
    }

    /// Stat-only walk: no file contents are read, so this stays O(files).
    pub fn digest(&self) -> String {
        let mut rows: Vec<(String, u64, u128)> = Vec::new();
        let mut stack = vec![self.worktree.clone()];
        while let Some(dir) = stack.pop() {
            if self.ignored(&dir) {
                continue;
            }
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.is_dir() {
                    if SKIP_DIRS.contains(&name.as_str()) || self.ignored(&path) {
                        continue;
                    }
                    stack.push(path);
                } else if meta.is_file() {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    rows.push((path.to_string_lossy().to_string(), meta.len(), mtime));
                }
            }
        }
        rows.sort();
        let mut hasher = blake3::Hasher::new();
        for (path, len, mtime) in rows {
            hasher.update(path.as_bytes());
            hasher.update(b"\0");
            hasher.update(len.to_string().as_bytes());
            hasher.update(b"\0");
            hasher.update(mtime.to_string().as_bytes());
            hasher.update(b"\0");
        }
        hasher.finalize().to_hex().to_string()
    }

    /// Degradation level for the *next* turn.
    pub fn tick(&mut self, turns_used: u32) -> Level {
        if turns_used >= self.max_turns {
            return Level::Abort;
        }
        if self.digest.is_none() || turns_used.is_multiple_of(self.sample_every) {
            let current = self.digest();
            self.samples += 1;
            match &self.digest {
                Some(prev) if *prev == current => self.stall += 1,
                _ => self.stall = 0,
            }
            self.digest = Some(current);
        }
        if self.stall >= self.stall_turns {
            return Level::Narrow;
        }
        if turns_used + 4 >= self.max_turns {
            return Level::Warn;
        }
        Level::Ok
    }

    pub fn report(&self) -> (u32, u32, String) {
        (
            self.samples,
            self.stall,
            self.digest.as_deref().unwrap_or("").chars().take(12).collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("twig-guard-{}", blake3::hash(nanoid().as_bytes()).to_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        dir
    }

    fn nanoid() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string()
    }

    #[test]
    fn degrades_never_aborts_early() {
        let dir = tmpdir();
        let mut g = Guard::new(&dir, 12, 2, 1);
        assert_eq!(g.tick(1), Level::Ok);
        assert_ne!(g.tick(2), Level::Abort);
        assert_eq!(g.tick(3), Level::Narrow);
        assert_eq!(g.tick(12), Level::Abort);
    }

    #[test]
    fn warn_before_abort() {
        let dir = tmpdir();
        let mut g = Guard::new(&dir, 12, 99, 1);
        assert_eq!(g.tick(8), Level::Warn);
    }

    #[test]
    fn record_writes_do_not_reset_the_stall() {
        let dir = tmpdir();
        let runs = dir.join("runs-elsewhere");
        std::fs::create_dir_all(&runs).unwrap();
        let mut g = Guard::new(&dir, 99, 2, 1).ignoring([runs.clone()]);
        g.tick(1);
        g.tick(2);
        for i in 0..5 {
            std::fs::write(runs.join(format!("{i}.jsonl")), b"noise").unwrap();
        }
        assert_eq!(g.tick(3), Level::Narrow, "harness writes must not read as progress");
    }

    #[test]
    fn real_edit_resets_the_stall() {
        let dir = tmpdir();
        let mut g = Guard::new(&dir, 99, 2, 1);
        g.tick(1);
        g.tick(2);
        std::fs::write(dir.join("a.txt"), b"changed").unwrap();
        assert_ne!(g.tick(3), Level::Narrow);
    }
}
