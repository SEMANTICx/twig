//! The decision trail. This is the only thing a human reads after an overnight run.
//!
//! Append-only TSV so it diffs well in review and never needs a parser.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Decisions {
    path: PathBuf,
}

impl Decisions {
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        if file.metadata().map(|m| m.len() == 0).unwrap_or(true) {
            writeln!(file, "ts\tchoice\trejected\twhy")?;
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// One row per judgment call. Tabs and newlines in free text are folded,
    /// because a broken row is worse than a long one.
    pub fn log(&self, choice: &str, rejected: &str, why: &str) -> std::io::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(
            file,
            "{ts}\t{}\t{}\t{}",
            fold(choice),
            fold(rejected),
            fold(why)
        )
    }

    pub fn rows(&self) -> std::io::Result<Vec<String>> {
        Ok(std::fs::read_to_string(&self.path)?
            .lines()
            .skip(1)
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect())
    }
}

fn fold(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!(
            "twig-dec-{}-{}.tsv",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn writes_a_header_then_rows() {
        let path = tmp();
        let d = Decisions::open(&path).unwrap();
        d.log("narrow the slice", "batch all four fixes", "one commit per accepted slice").unwrap();
        let rows = d.rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("narrow the slice"));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn folds_embedded_tabs() {
        let path = tmp();
        let d = Decisions::open(&path).unwrap();
        d.log("a\tb", "c", "d\ne").unwrap();
        let rows = d.rows().unwrap();
        assert_eq!(rows[0].split('\t').count(), 4, "a stray tab must not add a column");
        std::fs::remove_file(path).ok();
    }
}
