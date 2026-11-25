//! Stable operational lessons distilled from training evidence, never task text.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub results: String,
    pub row: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Lesson {
    pub id: String,
    pub guidance: String,
    pub evidence: Vec<Evidence>,
}

pub fn load(path: &Path) -> Result<Vec<Lesson>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("lessons: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("lessons: {e}")),
    }
}

const GUIDANCE: &[(&str, &str)] = &[
    (
        "bounded-commands",
        "Use bounded commands and inspect timeout evidence before raising execution limits.",
    ),
    (
        "verify-results",
        "Verify changed behavior with executable checks; a final claim is not verification.",
    ),
    (
        "read-before-edit",
        "Read the relevant specification and existing implementation before editing.",
    ),
    (
        "bounded-context",
        "Read focused ranges and summarize large outputs before consuming more context.",
    ),
];

pub fn context(path: &Path) -> Result<String, String> {
    Ok(load(path)?
        .iter()
        .filter_map(|lesson| GUIDANCE.iter().find(|(id, _)| *id == lesson.id))
        .map(|(_, guidance)| format!("- {guidance}\n"))
        .collect())
}

pub fn record(path: &Path, results: &Path, rows: &[Value]) -> Result<(), String> {
    let mut lessons = load(path)?;
    let results = results
        .canonicalize()
        .map_err(|e| e.to_string())?
        .display()
        .to_string();
    for (row, evidence) in rows.iter().enumerate() {
        let mut ids = Vec::new();
        if evidence["tool_timeouts"].as_u64().unwrap_or(0) > 0 || evidence["killed"] == true {
            ids.push("bounded-commands");
        }
        if evidence["trajectory"]["pass"] == false {
            ids.push("verify-results");
        }
        for cut in evidence["papercuts"].as_array().into_iter().flatten() {
            match cut["component"].as_str() {
                Some("read") => ids.push("read-before-edit"),
                Some("context") => ids.push("bounded-context"),
                _ => {}
            }
        }
        for id in ids {
            let index = match lessons.iter().position(|lesson| lesson.id == id) {
                Some(index) => index,
                None => {
                    let guidance = GUIDANCE.iter().find(|(key, _)| *key == id).unwrap().1;
                    lessons.push(Lesson {
                        id: id.into(),
                        guidance: guidance.into(),
                        evidence: Vec::new(),
                    });
                    lessons.len() - 1
                }
            };
            let references = &mut lessons[index].evidence;
            if !references
                .iter()
                .any(|reference| reference.results == results && reference.row == row)
            {
                references.push(Evidence {
                    results: results.clone(),
                    row,
                });
            }
        }
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(
        &temp,
        serde_json::to_vec_pretty(&lessons).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(temp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lessons_survive_restart_with_evidence_without_task_text() {
        let root = std::env::temp_dir().join(format!("twig-lessons-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let rows = vec![json!({"goal":"SECRET", "judge_tail":"SECRET", "tool_timeouts":1})];
        let results = root.join("results.jsonl");
        std::fs::write(&results, format!("{}\n", rows[0])).unwrap();
        let path = root.join("lessons.json");
        record(&path, &results, &rows).unwrap();
        record(&path, &results, &rows).unwrap();
        assert_eq!(load(&path).unwrap()[0].evidence.len(), 1);
        assert!(context(&path).unwrap().contains("bounded commands"));
        assert!(!std::fs::read_to_string(path).unwrap().contains("SECRET"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
