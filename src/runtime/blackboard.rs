//! Runtime RCI blackboard primitives: role protocol card loading, anti-overfit
//! validation, generation status, and decisive `path:line` fact extraction.

use std::path::Path;

use serde_json::Value;

use crate::config::paths;

pub const MAX_ROLE_NOTE_CHARS: usize = 1_200;
pub const ROLES: &[&str] = &["scout", "code", "test", "advisor", "reviewer"];

/// Extract concise `file:line` / `[Observed]` / `Fact:` bullets from a subagent output.
pub fn extract_decisive_facts(role: &str, slice: Option<&str>, text: &str) -> Vec<String> {
    let tag = match slice.filter(|s| !s.trim().is_empty()) {
        Some(sl) => format!("[{role} @ {sl}]"),
        None => format!("[{role}]"),
    };
    let mut out = Vec::new();
    for raw_line in text.lines() {
        let trimmed = raw_line
            .trim()
            .trim_start_matches(['-', '*', '•'])
            .trim();
        if trimmed.len() < 12 || trimmed.len() > 280 {
            continue;
        }
        let has_fact_prefix = trimmed.starts_with("[Fact]")
            || trimmed.starts_with("Fact:")
            || trimmed.starts_with("[Observed]");
        let has_file_line = (trimmed.contains(".rs:")
            || trimmed.contains(".py:")
            || trimmed.contains(".ts:")
            || trimmed.contains(".go:")
            || trimmed.contains(".js:")
            || trimmed.contains(".md:"))
            && trimmed.chars().any(|c| c.is_ascii_digit());
        if has_fact_prefix || has_file_line {
            let clean = trimmed
                .strip_prefix("[Fact]")
                .or_else(|| trimmed.strip_prefix("Fact:"))
                .unwrap_or(trimmed)
                .trim();
            let entry = format!("{tag} {clean}");
            if !out.contains(&entry) {
                out.push(entry);
            }
            if out.len() >= 6 {
                break;
            }
        }
    }
    out
}

/// Default compact protocol cards distilled from RCI findings: split decisive slices,
/// forward verbatim `file:line` evidence, and avoid redundant shared-context debate.
pub fn baseline_role_note(role: &str) -> &'static str {
    match role {
        "scout" => "\
- Inspect ONLY your assigned `slice` (never re-scan unrelated directories).
- Prefer `outline` -> targeted `read(start_line, limit)` / `grep` over broad shell dumps.
- End your response with `Facts:` bullets citing exact `path:line` and observed invariants (no ungrounded speculation).",
        "code" => "\
- Stay strictly within your assigned module `slice` and interface contract.
- Read every target file before editing; reuse `[Shared Fact Blackboard]` locations instead of re-exploring from scratch.
- Run fast targeted unit/check commands and end with `Facts:` listing modified `path:line` and test exit status.",
        "test" => "\
- Construct minimal deterministic reproducers and edge-case assertions covering your assigned `slice`.
- Report exact failing/passing test names and `path:line` assertions under `Facts:`.",
        "advisor" => "\
- Synthesize disjoint `[Shared Fact Blackboard]` evidence across slices; call out cross-module contradictions and simpler designs.
- Cite concrete `path:line` facts rather than repeating generic advice.",
        "reviewer" => "\
- Verify proposed diffs against `[Shared Fact Blackboard]` invariants and security/sandbox boundaries.
- Report `blockers` ONLY when backed by a concrete `file` and reproducible `trigger` path.",
        _ => "- Forward concise `path:line` facts; avoid redundant exploration.",
    }
}

/// Load `<repo>/.twig/notes/<role>.md` (falling back to `~/.config/twig/notes/<role>.md`),
/// strictly bounded to `MAX_ROLE_NOTE_CHARS` so role cards never bloat subagent prompts.
pub fn load_role_note(repo: &Path, role: &str) -> Option<String> {
    if !ROLES.contains(&role) {
        return None;
    }
    let project_card = repo.join(".twig").join("notes").join(format!("{role}.md"));
    let global_card = paths::config_dir()
        .join("notes")
        .join(format!("{role}.md"));
    let raw = std::fs::read_to_string(&project_card)
        .ok()
        .or_else(|| std::fs::read_to_string(&global_card).ok())?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_ROLE_NOTE_CHARS).collect())
}

/// Anti-overfitting guard for candidate role cards: rejects oversized notes or notes
/// leaking repository-specific file stems from `banned_tokens`.
pub fn validate_role_note(body: &str, banned_tokens: &[String]) -> Result<(), String> {
    let chars = body.chars().count();
    if chars == 0 {
        return Err("role note is empty".into());
    }
    if chars > MAX_ROLE_NOTE_CHARS {
        return Err(format!(
            "role note exceeds {MAX_ROLE_NOTE_CHARS}-char budget ({chars} chars)"
        ));
    }
    let low = body.to_lowercase();
    if let Some(hit) = banned_tokens
        .iter()
        .find(|tok| tok.len() >= 4 && low.contains(tok.as_str()))
    {
        return Err(format!("role note leaks task/repo-specific token {hit:?}"));
    }
    Ok(())
}

/// Format current `.twig/notes/` RCI generation status for CLI, RPC, and TUI `/rci`.
pub fn status_report(repo: &Path) -> String {
    let notes_dir = repo.join(".twig").join("notes");
    let state_val = std::fs::read_to_string(notes_dir.join("state.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let gen_n = state_val
        .as_ref()
        .and_then(|v| v["generation"].as_u64())
        .unwrap_or(0);
    let active_cards: Vec<(&str, usize)> = ROLES
        .iter()
        .filter_map(|&r| load_role_note(repo, r).map(|body| (r, body.chars().count())))
        .collect();
    if active_cards.is_empty() {
        return format!(
            "RCI status: generation {gen_n} · 0 active role cards (run `/rci distill` or `twig rci distill` to initialize `.twig/notes/`)"
        );
    }
    let summary: Vec<String> = active_cards
        .into_iter()
        .map(|(r, c)| format!("{r} ({c}c)"))
        .collect();
    format!(
        "RCI status: generation {gen_n} · active role protocol cards: {}",
        summary.join(", ")
    )
}
