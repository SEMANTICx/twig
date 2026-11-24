//! Playbook routing — the pstack mechanism, and the thing I left out last time.
//!
//! A playbook is not documentation. It is a checklist that gets copied into the
//! todo list verbatim, so the process is written once and never re-explained in a
//! prompt. Skipping a step is allowed, but only with a written reason: two skips
//! on the same step mean the playbook is wrong, not the task.

use std::path::Path;

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
                // "1. do the thing"  /  "12. do the thing"
                let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
                if !digits.is_empty() {
                    let rest = trimmed[digits.len()..].trim_start_matches(['.', ')']).trim();
                    if !rest.is_empty() {
                        steps.push(rest.to_string());
                    }
                } else if !trimmed.is_empty() && line.starts_with(char::is_whitespace) {
                    // An indented continuation belongs to the step above it. Steps are
                    // copied verbatim into the todo list, so truncating one changes it.
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
/// Returns the highest scorer, or `None` when nothing matches (which means the
/// caller should ask rather than guess).
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
        assert_eq!(route("there is a crash on idle", &books).unwrap().name, "bug-fix");
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
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("playbooks");
        let books = Playbook::load_dir(&dir);
        assert_eq!(books.len(), 3, "expected three playbooks, got {books:?}");
        assert!(books.iter().any(|b| b.name == "bug-fix"));
        assert!(books.iter().any(|b| b.name == "autonomous-run"));
        for book in &books {
            assert!(!book.when.is_empty(), "{} has no routing keywords", book.name);
            assert!(book.steps.len() >= 5, "{} is too thin", book.name);
        }
    }
}
