//! Cheap bounded skill metadata; bodies are read only after explicit selection.
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata { pub name: String, pub description: String, pub path: PathBuf }
#[derive(Debug, Clone)]
pub struct SkillIndex { root: PathBuf, entries: Vec<SkillMetadata> }
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
            let path = if kind.is_dir() { entry.path().join("SKILL.md") }
                else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "md") { entry.path() }
                else { continue };
            if !path.exists() { continue; }
            let path = path.canonicalize()?;
            if !path.starts_with(&root) { return Err(io::Error::new(io::ErrorKind::PermissionDenied, "skill escapes index root")); }
            let mut reader = io::BufReader::new(std::fs::File::open(&path)?.take(HEADER_BYTES));
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line.trim() != "---" { continue; }
            let mut name = String::new();
            let mut description = String::new();
            let mut description_block = false;
            let mut closed = false;
            loop {
                line.clear(); if reader.read_line(&mut line)? == 0 { break; }
                let trimmed = line.trim();
                if trimmed == "---" { closed = true; break; }
                if let Some(value) = line.strip_prefix("name:") { name = value.trim().trim_matches(['\'', '"']).to_string(); description_block = false; }
                else if let Some(value) = line.strip_prefix("description:") {
                    let value = value.trim(); description_block = matches!(value, ">" | "|" | ">-" | "|-");
                    if !description_block { description = value.trim_matches(['\'', '"']).to_string(); }
                } else if description_block && line.starts_with(char::is_whitespace) {
                    if !description.is_empty() { description.push(' '); } description.push_str(trimmed);
                } else if !trimmed.is_empty() { description_block = false; }
            }
            if !closed || name.is_empty() { return Err(io::Error::new(io::ErrorKind::InvalidData, format!("invalid or oversized skill header: {}", path.display()))); }
            if entries.iter().any(|e: &SkillMetadata| e.name == name) { return Err(io::Error::new(io::ErrorKind::InvalidData, format!("duplicate skill {name}"))); }
            entries.push(SkillMetadata { name, description, path });
            if entries.len() > MAX_SKILLS { return Err(io::Error::new(io::ErrorKind::InvalidData, "too many skills")); }
        }
        entries.sort_by(|a,b|a.name.cmp(&b.name));
        Ok(Self {root,entries})
    }
    pub fn metadata(&self) -> &[SkillMetadata] { &self.entries }
    pub fn load(&self, name: &str) -> io::Result<String> {
        let metadata = self.entries.iter().find(|e| e.name == name).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown skill"))?;
        let path = metadata.path.canonicalize()?;
        if !path.starts_with(&self.root) { return Err(io::Error::new(io::ErrorKind::PermissionDenied, "skill escapes index root")); }
        read_bounded(&path, MAX_SKILL_BYTES)
    }
}
fn read_bounded(path: &Path, limit: u64) -> io::Result<String> {
    let mut text = String::new(); std::fs::File::open(path)?.take(limit + 1).read_to_string(&mut text)?;
    if text.len() as u64 > limit { return Err(io::Error::new(io::ErrorKind::InvalidData, "instruction file exceeds size limit")); }
    Ok(text)
}
/// Append a selected existing playbook and explicit per-project preferences.
pub fn inject_playbook(system: &mut String, book: &crate::playbook::Playbook, preferences: &str) {
    system.push_str(&format!("\n\nProject playbook: {}\n", book.name));
    for (i, step) in book.steps.iter().enumerate() { system.push_str(&format!("{}. {}\n", i + 1, step)); }
    if !preferences.is_empty() { system.push_str("\nProject preferences:\n"); system.push_str(preferences); }
}
/// Explicit paths keep project configuration portable; missing files are errors.
pub fn load_project_instructions(playbook: &Path, preferences: Option<&Path>) -> io::Result<String> {
    let mut text = read_bounded(playbook, MAX_SKILL_BYTES)?;
    if let Some(path) = preferences { text.push_str("\n\nProject preferences:\n"); text.push_str(&read_bounded(path, MAX_SKILL_BYTES)?); }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn index_is_metadata_only_and_body_is_loaded_on_demand() {
        let root = std::env::temp_dir().join(format!("twig-skills-{}", std::process::id()));
        std::fs::create_dir_all(root.join("repair")).unwrap();
        let path = root.join("repair/SKILL.md");
        std::fs::write(&path, "---\nname: repair\ndescription: >\n  Fix broken things.\n---\nSECRET BODY").unwrap();
        let index = SkillIndex::discover(&root).unwrap();
        assert_eq!(index.metadata()[0].description, "Fix broken things.");
        assert!(!serde_json::to_string(index.metadata()).unwrap().contains("SECRET BODY"));
        std::fs::write(&path, "CHANGED BODY").unwrap();
        assert_eq!(index.load("repair").unwrap(), "CHANGED BODY");
        assert!(index.load("../repair").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
