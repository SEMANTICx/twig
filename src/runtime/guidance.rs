//! Playbooks and on-demand project skills.
//!
//! Built-in playbooks are embedded in the binary so runtime commands never
//! depend on `CARGO_MANIFEST_DIR`. `SkillIndex` hides its internal metadata list
//! behind narrow summary/load operations so callers never depend on its layout.

use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Playbook {
    pub name: String,
    pub when: Vec<String>,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Skipped(String),
}

#[derive(Debug, Clone)]
pub struct TodoItem {
    pub step: String,
    pub status: StepStatus,
}

impl Playbook {
    pub fn builtin() -> Vec<Self> {
        [
            ("bug-fix", include_str!("../../playbooks/bug-fix.md")),
            ("feature", include_str!("../../playbooks/feature.md")),
            (
                "autonomous-run",
                include_str!("../../playbooks/autonomous-run.md"),
            ),
        ]
        .into_iter()
        .filter_map(|(name, body)| Self::parse(body, name))
        .collect()
    }

    /// Loads playbooks from `TWIG_PLAYBOOKS_DIR` or XDG config if present,
    /// falling back to the embedded built-in playbooks.
    pub fn load_default() -> Vec<Self> {
        let dir = std::env::var_os("TWIG_PLAYBOOKS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::config::paths::config_dir().join("playbooks"));
        let external = Self::load_dir(&dir);
        if external.is_empty() {
            Self::builtin()
        } else {
            external
        }
    }

    pub fn parse(text: &str, fallback_name: &str) -> Option<Self> {
        let mut name = fallback_name.to_string();
        let mut when: Vec<String> = Vec::new();
        let mut steps: Vec<String> = Vec::new();
        let mut in_front = false;
        let mut in_steps = false;

        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed == "---" {
                in_front = !in_front;
                continue;
            }
            if in_front {
                if let Some(rest) = trimmed.strip_prefix("name:") {
                    name = rest.trim().to_string();
                } else if let Some(rest) = trimmed.strip_prefix("when:") {
                    when = rest
                        .split(',')
                        .map(|k| k.trim().to_lowercase())
                        .filter(|k| !k.is_empty())
                        .collect();
                }
                continue;
            }
            if trimmed.starts_with("## ") {
                in_steps = trimmed.eq_ignore_ascii_case("## steps");
                continue;
            }
            if in_steps {
                let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
                if !digits.is_empty() {
                    let rest = trimmed[digits.len()..].trim_start_matches(['.', ')']).trim();
                    if !rest.is_empty() {
                        steps.push(rest.to_string());
                    }
                } else if !trimmed.is_empty() && line.starts_with(char::is_whitespace) {
                    if let Some(last) = steps.last_mut() {
                        last.push(' ');
                        last.push_str(trimmed);
                    }
                }
            }
        }

        if steps.is_empty() {
            return None;
        }
        Some(Self { name, when, steps })
    }

    pub fn load_dir(dir: &Path) -> Vec<Self> {
        let mut books = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return books;
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        paths.sort();
        for path in paths {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let fallback = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Some(book) = Self::parse(&text, &fallback) {
                books.push(book);
            }
        }
        books
    }

    pub fn todo(&self) -> Vec<TodoItem> {
        self.steps
            .iter()
            .map(|step| TodoItem {
                step: step.clone(),
                status: StepStatus::Pending,
            })
            .collect()
    }
}

/// Match a goal to a playbook by counting keyword hits from each `when:` line.
pub fn route<'a>(goal: &str, books: &'a [Playbook]) -> Option<&'a Playbook> {
    let goal = goal.to_lowercase();
    let mut best: Option<(&Playbook, usize)> = None;
    for book in books {
        let score = book
            .when
            .iter()
            .filter(|kw| goal.contains(kw.as_str()))
            .count();
        if score == 0 {
            continue;
        }
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((book, score));
        }
    }
    best.map(|(b, _)| b)
}

/// Every skip must carry a reason. Returns the offending step indices.
pub fn validate_skips(items: &[TodoItem]) -> Result<(), Vec<usize>> {
    let bad: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| match &item.status {
            StepStatus::Skipped(reason) => reason.trim().is_empty(),
            StepStatus::Pending => false,
        })
        .map(|(i, _)| i)
        .collect();
    if bad.is_empty() {
        Ok(())
    } else {
        Err(bad)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillMetadata {
    name: String,
    description: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SkillIndex {
    root: PathBuf,
    entries: Vec<SkillMetadata>,
}

const HEADER_BYTES: u64 = 16_384;
const MAX_SKILL_BYTES: u64 = 262_144;
const MAX_SKILLS: usize = 1024;

impl SkillIndex {
    /// One directory level, matching `<name>/SKILL.md` and standalone Markdown.
    /// Only the bounded frontmatter is read, never the skill body at discovery.
    pub fn discover(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = if kind.is_dir() {
                entry.path().join("SKILL.md")
            } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "md") {
                entry.path()
            } else {
                continue;
            };
            if !path.exists() {
                continue;
            }
            let path = path.canonicalize()?;
            if !path.starts_with(&root) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "skill escapes index root",
                ));
            }
            let mut reader = io::BufReader::new(std::fs::File::open(&path)?.take(HEADER_BYTES));
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line.trim() != "---" {
                continue;
            }
            let mut name = String::new();
            let mut description = String::new();
            let mut description_block = false;
            let mut closed = false;
            loop {
                line.clear();
                if reader.read_line(&mut line)? == 0 {
                    break;
                }
                let trimmed = line.trim();
                if trimmed == "---" {
                    closed = true;
                    break;
                }
                if let Some(value) = line.strip_prefix("name:") {
                    name = value.trim().trim_matches(['\'', '"']).to_string();
                    description_block = false;
                } else if let Some(value) = line.strip_prefix("description:") {
                    let value = value.trim();
                    description_block = matches!(value, ">" | "|" | ">-" | "|-");
                    if !description_block {
                        description = value.trim_matches(['\'', '"']).to_string();
                    }
                } else if description_block && line.starts_with(char::is_whitespace) {
                    if !description.is_empty() {
                        description.push(' ');
                    }
                    description.push_str(trimmed);
                } else if !trimmed.is_empty() {
                    description_block = false;
                }
            }
            if !closed || name.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid or oversized skill header: {}", path.display()),
                ));
            }
            if entries.iter().any(|e: &SkillMetadata| e.name == name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate skill {name}"),
                ));
            }
            entries.push(SkillMetadata {
                name,
                description,
                path,
            });
            if entries.len() > MAX_SKILLS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "too many skills",
                ));
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { root, entries })
    }

    /// JSON metadata catalog for tool responses and system prompts. Never includes bodies.
    pub fn summary_json(&self) -> String {
        serde_json::to_string(&self.entries).unwrap_or_default()
    }

    pub fn load(&self, name: &str) -> io::Result<String> {
        let metadata = self
            .entries
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown skill"))?;
        let path = metadata.path.canonicalize()?;
        if !path.starts_with(&self.root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "skill escapes index root",
            ));
        }
        read_bounded(&path, MAX_SKILL_BYTES)
    }
}

fn read_bounded(path: &Path, limit: u64) -> io::Result<String> {
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "instruction file exceeds size limit",
        ));
    }
    Ok(text)
}

/// Append a selected playbook and explicit per-project preferences.
pub fn inject_playbook(system: &mut String, book: &Playbook, preferences: &str) {
    system.push_str(&format!("\n\nProject playbook: {}\n", book.name));
    for (i, step) in book.steps.iter().enumerate() {
        system.push_str(&format!("{}. {}\n", i + 1, step));
    }
    if !preferences.is_empty() {
        system.push_str("\nProject preferences:\n");
        system.push_str(preferences);
    }
}

/// Explicit paths keep project configuration portable; missing files are errors.
pub fn load_project_instructions(
    playbook: &Path,
    preferences: Option<&Path>,
) -> io::Result<String> {
    let mut text = read_bounded(playbook, MAX_SKILL_BYTES)?;
    if let Some(path) = preferences {
        text.push_str("\n\nProject preferences:\n");
        text.push_str(&read_bounded(path, MAX_SKILL_BYTES)?);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
---
name: bug-fix
when: bug, broken, crash, regression
---

# bug-fix

## steps

1. Restate the defect in one sentence.
2. Reproduce it.
3. Record the repro as a journal.

## forbidden

- silencing the symptom
";

    #[test]
    fn parses_steps_and_keywords() {
        let book = Playbook::parse(SAMPLE, "fallback").unwrap();
        assert_eq!(book.name, "bug-fix");
        assert_eq!(book.when, vec!["bug", "broken", "crash", "regression"]);
        assert_eq!(book.steps.len(), 3);
        assert!(book.steps[0].starts_with("Restate"));
        assert!(!book.steps.iter().any(|s| s.contains("silencing")));
    }

    #[test]
    fn continuation_lines_join_their_step() {
        let text = "## steps\n\n1. Record the repro as a journal\n   so it can be replayed:\n   capture real calls.\n2. Fix it.\n";
        let book = Playbook::parse(text, "x").unwrap();
        assert_eq!(book.steps.len(), 2);
        assert_eq!(
            book.steps[0],
            "Record the repro as a journal so it can be replayed: capture real calls."
        );
    }

    #[test]
    fn routes_by_keyword() {
        let book = Playbook::parse(SAMPLE, "f").unwrap();
        let books = vec![book];
        assert_eq!(
            route("there is a crash on idle", &books).unwrap().name,
            "bug-fix"
        );
        assert!(route("add a settings page", &books).is_none());
    }

    #[test]
    fn skip_without_reason_is_rejected() {
        let book = Playbook::parse(SAMPLE, "f").unwrap();
        let mut items = book.todo();
        items[1].status = StepStatus::Skipped(String::new());
        assert_eq!(validate_skips(&items), Err(vec![1]));

        items[1].status = StepStatus::Skipped("covered by tests/unit/x.rs".into());
        assert!(validate_skips(&items).is_ok());
    }

    #[test]
    fn loads_the_real_playbooks() {
        let books = Playbook::builtin();
        assert_eq!(books.len(), 3, "expected three playbooks, got {books:?}");
        assert!(books.iter().any(|b| b.name == "bug-fix"));
        assert!(books.iter().any(|b| b.name == "autonomous-run"));
        for book in &books {
            assert!(
                !book.when.is_empty(),
                "{} has no routing keywords",
                book.name
            );
            assert!(book.steps.len() >= 5, "{} is too thin", book.name);
        }
    }

    #[test]
    fn index_is_metadata_only_and_body_is_loaded_on_demand() {
        let root = std::env::temp_dir().join(format!("twig-skills-{}", std::process::id()));
        std::fs::create_dir_all(root.join("repair")).unwrap();
        let path = root.join("repair/SKILL.md");
        std::fs::write(
            &path,
            "---\nname: repair\ndescription: >\n  Fix broken things.\n---\nSECRET BODY",
        )
        .unwrap();
        let index = SkillIndex::discover(&root).unwrap();
        let catalog = index.summary_json();
        assert!(catalog.contains("Fix broken things."));
        assert!(!catalog.contains("SECRET BODY"));
        std::fs::write(&path, "CHANGED BODY").unwrap();
        assert_eq!(index.load("repair").unwrap(), "CHANGED BODY");
        assert!(index.load("../repair").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
