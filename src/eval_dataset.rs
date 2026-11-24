//! Corpus splitting and immutable imports. Checks and source metadata stay outside repo/.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use super::Task;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split { Train, Dev, Final }

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportTask {
    pub name: String,
    /// Stable real task/session/issue identifier, never supplied to the solver.
    pub source: String,
    pub goal: String,
    pub split: Split,
    /// Trusted host-side check, run after the isolated solver exits.
    pub check: String,
    #[serde(default)]
    pub forbid: Vec<String>,
    #[serde(default)]
    pub trajectory: Vec<crate::trajectory::Rule>,
    #[serde(default)]
    pub wip: Vec<String>,
}

pub fn import_snapshot(root: &Path, repo: &Path, task: &ImportTask) -> Result<PathBuf, String> {
    if task.name.is_empty() || !task.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("task name must contain only letters, digits, '-' or '_'".into());
    }
    if task.source.trim().is_empty() || task.goal.trim().is_empty() || task.check.trim().is_empty() {
        return Err("source, goal and check must be nonempty".into());
    }
    let repo = repo.canonicalize().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if root.starts_with(&repo) { return Err("corpus must be outside the imported repository".into()); }
    let dest = root.join(&task.name);
    std::fs::create_dir(&dest).map_err(|e| e.to_string())?;
    let result = (|| {
        super::copy_all(&repo, &dest.join("repo")).map_err(|e| e.to_string())?;
        if repo.join(".git").is_file() {
            // Linked worktrees keep their index/HEAD separately from common
            // objects. Copy both so staged and unstaged work remain distinguishable.
            let git_path = |option: &str| -> Result<PathBuf, String> {
                let output = std::process::Command::new("git").arg("-C").arg(&repo)
                    .args(["rev-parse", "--path-format=absolute", option]).output().map_err(|e| e.to_string())?;
                if !output.status.success() { return Err("cannot resolve imported Git metadata".into()); }
                Ok(PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
            };
            let common = git_path("--git-common-dir")?;
            let private = git_path("--git-dir")?;
            let target = dest.join("repo/.git");
            super::copy_all(&common, &target).map_err(|e| e.to_string())?;
            for file in ["HEAD", "index"] {
                if private.join(file).exists() { std::fs::copy(private.join(file), target.join(file)).map_err(|e| e.to_string())?; }
            }
        }
        std::fs::write(dest.join("task.json"), serde_json::to_vec_pretty(task).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        Ok(dest.clone())
    })();
    if result.is_err() { std::fs::remove_dir_all(&dest).ok(); }
    result
}

fn hash_tree(path: &Path, hash: &mut blake3::Hasher) -> Result<(), String> {
    let mut entries = std::fs::read_dir(path).map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if entry.file_name() == ".git" || entry.file_name() == "__pycache__" { continue; }
        hash.update(entry.file_name().as_encoded_bytes());
        hash.update(&[0]);
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() { hash.update(b"dir"); hash_tree(&entry.path(), hash)?; }
        else if kind.is_file() {
            hash.update(b"file");
            let mut file = std::fs::File::open(entry.path()).map_err(|e| e.to_string())?;
            std::io::copy(&mut file, hash).map_err(|e| e.to_string())?;
        } else if kind.is_symlink() {
            hash.update(b"link");
            hash.update(std::fs::read_link(entry.path()).map_err(|e| e.to_string())?.as_os_str().as_encoded_bytes());
        }
        hash.update(&[0]);
    }
    Ok(())
}

pub fn fingerprint(task: &Task) -> Result<String, String> {
    let mut hash = blake3::Hasher::new();
    hash.update(task.goal.as_bytes());
    hash.update(&[0]);
    let script = task.dir.join("check.sh");
    hash.update(&if script.exists() { std::fs::read(script).map_err(|e| e.to_string())? } else { task.check.as_bytes().to_vec() });
    hash.update(&serde_json::to_vec(&task.trajectory).map_err(|e| e.to_string())?);
    hash_tree(&task.dir.join("repo"), &mut hash)?;
    Ok(hash.finalize().to_hex().to_string())
}

pub struct Dataset { pub train: Vec<Task>, pub dev: Vec<Task>, pub final_holdout: Vec<Task> }

pub fn separate(tasks: Vec<Task>) -> Result<Dataset, String> {
    let explicit = tasks.iter().any(|t| t.split.is_some());
    if explicit && tasks.iter().any(|t| t.split.is_none()) {
        return Err("every task needs an explicit split when any task declares one".into());
    }
    let mut groups = BTreeMap::<String, Vec<Task>>::new();
    let mut fingerprints = BTreeMap::<String, Option<Split>>::new();
    for task in tasks {
        let fingerprint = fingerprint(&task)?;
        if fingerprints.insert(fingerprint.clone(), task.split).is_some() {
            return Err("duplicate snapshot: remove duplicate tasks before splitting".into());
        }
        if task.dir.parent().is_some_and(|root| root.join(".twig-final").join(&fingerprint).exists()) {
            return Err("consumed final snapshot cannot be reused in any split".into());
        }
        let group = task.source.clone().unwrap_or(fingerprint);
        groups.entry(group).or_default().push(task);
    }
    if !explicit && groups.len() < 3 { return Err("improvement requires at least three independent task groups for train/dev/final".into()); }
    let count = groups.len();
    let mut out = Dataset { train: Vec::new(), dev: Vec::new(), final_holdout: Vec::new() };
    for (i, (_, tasks)) in groups.into_iter().enumerate() {
        let split = if explicit {
            let split = tasks[0].split.unwrap();
            if tasks.iter().any(|t| t.split != Some(split)) { return Err("same source crosses protected splits".into()); }
            split
        } else if i < count.saturating_sub((count / 5).max(1) * 2) { Split::Train }
        else if i < count - (count / 5).max(1) { Split::Dev } else { Split::Final };
        match split { Split::Train => out.train.extend(tasks), Split::Dev => out.dev.extend(tasks), Split::Final => out.final_holdout.extend(tasks) }
    }
    if out.train.is_empty() || out.dev.is_empty() || out.final_holdout.is_empty() {
        return Err("train, dev and final splits must all be nonempty".into());
    }
    for (split, tasks) in [(Split::Train, &out.train), (Split::Dev, &out.dev), (Split::Final, &out.final_holdout)] {
        for task in tasks {
            let root = task.dir.parent().ok_or("task has no corpus directory")?.join(".twig-splits");
            std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
            let mut identities = vec![fingerprint(task)?];
            if let Some(source) = &task.source { identities.push(format!("source-{}", blake3::hash(source.as_bytes()).to_hex())); }
            let encoded = serde_json::to_vec(&split).map_err(|e| e.to_string())?;
            for identity in identities {
                let path = root.join(identity);
                match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(mut file) => { file.write_all(&encoded).map_err(|e| e.to_string())?; file.sync_all().map_err(|e| e.to_string())?; }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        if std::fs::read(path).map_err(|e| e.to_string())? != encoded { return Err("protected task/source split changed; use new independent tasks".into()); }
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
    }
    Ok(out)
}

/// Reserve before either baseline or candidate sees final data. A crash consumes
/// the set too; rerunning against the same final tasks would be iterative tuning.
pub fn reserve_final(root: &Path, tasks: &[Task]) -> Result<(), String> {
    let receipts = root.join(".twig-final");
    std::fs::create_dir_all(&receipts).map_err(|e| e.to_string())?;
    let hashes = tasks.iter().map(fingerprint).collect::<Result<BTreeSet<_>, _>>()?;
    for hash in hashes {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(receipts.join(hash))
            .map_err(|e| format!("final holdout already consumed or cannot be reserved: {e}"))?;
        writeln!(file, "reserved by pid {}", std::process::id()).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn imported_snapshot_preserves_work_and_final_cannot_be_reused() {
        let root = std::env::temp_dir().join(format!("twig-import-{}", std::process::id()));
        let repo = root.join("source");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("wip.txt"), "unfinished user work").unwrap();
        let task = ImportTask { name:"real-task".into(), source:"session:123".into(), goal:"fix bug".into(),
            split:Split::Final, check:"test -f wip.txt".into(), forbid:vec![], trajectory:vec![], wip:vec![] };
        let corpus = root.join("corpus");
        let dest = import_snapshot(&corpus, &repo, &task).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("repo/wip.txt")).unwrap(), "unfinished user work");
        let tasks = super::super::load_tasks(&corpus, None);
        reserve_final(&corpus, &tasks).unwrap();
        assert!(reserve_final(&corpus, &tasks).is_err());
        assert!(separate(tasks).err().unwrap().contains("consumed final"));
        assert!(!dest.join("repo/task.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn splits_cannot_be_moved_or_shared_and_receipts_hash_content() {
        let root = std::env::temp_dir().join(format!("twig-splits-{}", std::process::id()));
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("code"), "original").unwrap();
        let corpus = root.join("corpus");
        for (name, split) in [("a", Split::Train), ("b", Split::Dev), ("c", Split::Final)] {
            import_snapshot(&corpus, &source, &ImportTask { name:name.into(), source:format!("issue:{name}"),
                goal:format!("Fix {name}"), split, check:"true".into(), forbid:vec![], trajectory:vec![], wip:vec![] }).unwrap();
        }
        let tasks = super::super::load_tasks(&corpus, None);
        let before = fingerprint(&tasks[0]).unwrap();
        std::fs::write(tasks[0].dir.join("repo/code"), "changed").unwrap();
        assert_ne!(before, fingerprint(&tasks[0]).unwrap());
        separate(tasks.clone()).unwrap();
        let mut moved = tasks.clone();
        moved[0].split = Some(Split::Dev);
        moved[1].split = Some(Split::Train);
        assert!(separate(moved).is_err());
        let mut duplicate = tasks;
        duplicate[0].source = duplicate[1].source.clone();
        assert!(separate(duplicate).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
