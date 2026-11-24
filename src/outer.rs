//! Outer loop: the part that means you are not watching.
//!
//! sources -> cheap filter -> queue -> one worktree per item -> run -> verify
//!
//! The queue is append-only JSONL, so a crash never loses work and a rerun never
//! duplicates it. One worktree per item bounds the blast radius, which is why the
//! in-loop checks can stay cheap.

use std::collections::{BTreeMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::provider::Llm;

pub struct Queue {
    path: PathBuf,
    seen: HashSet<String>,
}

impl Queue {
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        OpenOptions::new().create(true).append(true).open(&path)?;
        let seen = Self::rows_at(&path)
            .into_iter()
            .filter_map(|r| r["id"].as_str().map(str::to_string))
            .collect();
        Ok(Self { path, seen })
    }

    fn rows_at(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn append(&self, row: &Value) -> std::io::Result<()> {
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(f, "{row}")
    }

    /// Idempotent: the same item from the same source never queues twice.
    pub fn put(&mut self, kind: &str, source: &str, title: &str, body: &str) -> std::io::Result<Option<String>> {
        let id = blake3::hash(format!("{kind}\0{source}\0{title}").as_bytes()).to_hex()[..12].to_string();
        if !self.seen.insert(id.clone()) {
            return Ok(None);
        }
        self.append(&json!({"id": id, "kind": kind, "source": source,
                            "title": title, "body": body, "state": "queued"}))?;
        Ok(Some(id))
    }

    pub fn mark(&self, id: &str, state: &str) -> std::io::Result<()> {
        self.append(&json!({"id": id, "state": state}))
    }

    /// First row per id is the item; later rows only change its state.
    pub fn pending(&self) -> Vec<Value> {
        let mut items: BTreeMap<String, Value> = BTreeMap::new();
        let mut closed: HashSet<String> = HashSet::new();
        for row in Self::rows_at(&self.path) {
            let Some(id) = row["id"].as_str().map(str::to_string) else { continue };
            if matches!(row["state"].as_str(), Some("done" | "dropped")) {
                closed.insert(id.clone());
            }
            items.entry(id).or_insert(row);
        }
        items.into_iter().filter(|(id, _)| !closed.contains(id)).map(|(_, r)| r).collect()
    }
}

/// One worktree per item, held OUTSIDE the repo: a worktree inside its own source
/// tree copies itself, and the agent then sees duplicates of what it just wrote.
pub fn worktree(repo: &Path, item_id: &str) -> std::io::Result<PathBuf> {
    let repo = repo.canonicalize()?;
    let name = repo.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let root = repo.parent().unwrap_or(&repo).join(".twig-worktrees").join(name);
    let dest = root.join(item_id);
    if dest.is_dir() {
        return Ok(dest);
    }
    std::fs::create_dir_all(&root)?;
    let git = Command::new("git")
        .arg("-C").arg(&repo)
        .args(["worktree", "add", "-q", "-b", &format!("twig/{item_id}")])
        .arg(&dest)
        .output();
    if matches!(&git, Ok(o) if o.status.success()) {
        return Ok(dest);
    }
    copy_tree(&repo, &dest)?; // not a git repo: a plain copy still isolates
    Ok(dest)
}

const SKIP: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__", ".twig-worktrees"];

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let name = entry.file_name();
        if SKIP.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(&name));
        if entry.file_type()?.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// One near-free yes/no call before any real work. Name collisions are real
/// (magpie is a bird), and routing noise to a strong model burns the budget.
pub fn relevant(llm: &dyn Llm, text: &str, topic: &str) -> Result<bool, String> {
    let model = llm.roles().get("filter").to_string();
    let clipped: String = text.chars().take(2000).collect();
    let reply = llm.complete(
        &model,
        "Answer with a single letter: y if the text concerns the topic, n otherwise.",
        &[json!({"role": "user", "content": format!("topic={topic}\ntext={clipped}")})],
        None,
    )?;
    Ok(reply.text.trim().to_lowercase().starts_with('y'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stub::StubLlm;

    fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!("twig-outer-{tag}-{}-{}", std::process::id(),
                                                  N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn queue_is_idempotent_and_survives_reopen() {
        let d = tmp("q");
        let mut q = Queue::open(d.join("q.jsonl")).unwrap();
        let id = q.put("issue", "gh", "scroll drifts", "every 750ms").unwrap().unwrap();
        assert!(q.put("issue", "gh", "scroll drifts", "dup").unwrap().is_none());
        let mut q2 = Queue::open(d.join("q.jsonl")).unwrap();
        assert!(q2.put("issue", "gh", "scroll drifts", "after restart").unwrap().is_none());
        assert_eq!(q2.pending().len(), 1);
        q2.mark(&id, "done").unwrap();
        assert!(q2.pending().is_empty());
    }

    #[test]
    fn worktree_lives_outside_the_repo() {
        let d = tmp("wt");
        let repo = d.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.txt"), "x").unwrap();
        let wt = worktree(&repo, "item1").unwrap();
        assert!(wt.join("a.txt").exists());
        assert!(!wt.starts_with(repo.canonicalize().unwrap()), "{wt:?} must not sit inside the repo");
    }

    #[test]
    fn triage_is_one_cheap_call() {
        let llm = StubLlm::new(vec![StubLlm::text("y"), StubLlm::text("n")]);
        assert!(relevant(&llm, "magpie gateway drops tool calls", "magpie").unwrap());
        assert!(!relevant(&llm, "a magpie stole my sandwich", "magpie").unwrap());
        assert_eq!(llm.calls(), 2);
    }
}
