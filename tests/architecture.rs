//! Machine-enforced architectural guardrails for `twig`:
//! 1. Module size ceiling (`<= 1,650` lines across every `src/**/*.rs` file)
//! 2. Strict 7-layer downward import DAG (zero upward cross-layer dependencies)
//! 3. TUI design-system Single Source of Truth (`Color::Rgb` only in leaf `src/tui/theme.rs`,
//!    cell-width `clip_width` instead of scalar `clip_str`)
//! 4. Canonical `SubagentConfig::for_repo` construction and bounded `AGENTS.md` prompt budget

use std::path::{Path, PathBuf};

const MAX_SOURCE_LINES: usize = 1_650;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn layer_rank(top_module: &str) -> Option<(u8, &'static str)> {
    match top_module {
        "config" | "record" | "control" => Some((1, "L1:foundation")),
        "exec" => Some((2, "L2:exec")),
        "llm" => Some((3, "L3:llm")),
        "verify" => Some((4, "L4:verify")),
        "runtime" => Some((5, "L5:runtime")),
        "eval" | "automation" => Some((6, "L6:eval_automation")),
        "tui" | "web" | "cli" | "main" | "lib" => Some((7, "L7:interfaces")),
        _ => None,
    }
}

fn top_module_for_path(src_root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(src_root).unwrap();
    let first = rel.components().next().unwrap().as_os_str().to_string_lossy();
    first.strip_suffix(".rs").unwrap_or(&first).to_owned()
}

fn extract_crate_refs(production_src: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for raw_line in production_src.lines() {
        let line = raw_line.trim();
        if line.starts_with("//") {
            continue;
        }
        let code = line.split("//").next().unwrap_or("");
        let mut rest = code;
        while let Some(pos) = rest.find("crate::") {
            let after = &rest[pos + "crate::".len()..];
            if let Some(brace_inner) = after.strip_prefix('{') {
                if let Some(end) = brace_inner.find('}') {
                    for part in brace_inner[..end].split(',') {
                        let seg = part
                            .trim()
                            .split("::")
                            .next()
                            .unwrap_or("")
                            .split_whitespace()
                            .next()
                            .unwrap_or("");
                        if !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                            refs.push(seg.to_owned());
                        }
                    }
                }
                rest = brace_inner;
                continue;
            }
            let ident: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let len = ident.len();
            if !ident.is_empty() {
                refs.push(ident);
            }
            rest = &after[len..];
        }
    }
    refs
}

#[test]
fn every_source_module_respects_1650_line_ceiling() {
    let root = repo_root();
    let src = root.join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    assert!(!files.is_empty());

    let mut violations = Vec::new();
    for file in files {
        let body = std::fs::read_to_string(&file).unwrap();
        let lines = body.lines().count();
        if lines > MAX_SOURCE_LINES {
            let rel = file.strip_prefix(&root).unwrap().display();
            violations.push(format!("{rel}: {lines} lines (max {MAX_SOURCE_LINES})"));
        }
    }
    assert!(
        violations.is_empty(),
        "source files exceeded {MAX_SOURCE_LINES}-line deep-module ceiling:\n{}",
        violations.join("\n")
    );
}

#[test]
fn strict_seven_layer_downward_dag_has_zero_upward_imports() {
    let root = repo_root();
    let src = root.join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);

    let mut violations = Vec::new();
    for file in files {
        let top = top_module_for_path(&src, &file);
        if matches!(top.as_str(), "lib" | "main") {
            continue;
        }
        let Some((src_rank, src_layer)) = layer_rank(&top) else {
            panic!("unmapped top-level module `{top}` in {} — add it to layer_rank() in tests/architecture.rs", file.display());
        };
        let body = std::fs::read_to_string(&file).unwrap();
        let prod_body = body.split("\n#[cfg(test)]").next().unwrap_or(&body);
        for target in extract_crate_refs(prod_body) {
            if let Some((dst_rank, dst_layer)) = layer_rank(&target) {
                if dst_rank > src_rank {
                    let rel = file.strip_prefix(&root).unwrap().display();
                    violations.push(format!(
                        "{rel} ({src_layer}) -> crate::{target} ({dst_layer})"
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "upward cross-layer dependency violations detected:\n{}",
        violations.join("\n")
    );
}

#[test]
fn tui_theme_is_pure_leaf_and_sole_owner_of_rgb_palette_and_width_clipping() {
    let root = repo_root();
    let src = root.join("src");
    let theme_path = src.join("tui/theme.rs");
    let theme_body = std::fs::read_to_string(&theme_path).unwrap();
    let theme_prod = theme_body.split("\n#[cfg(test)]").next().unwrap_or(&theme_body);
    assert!(
        !theme_prod.contains("super::"),
        "src/tui/theme.rs must remain a pure leaf module with zero `super::*` sibling imports"
    );

    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    let mut rgb_leaks = Vec::new();
    for file in files {
        if file == theme_path {
            continue;
        }
        let body = std::fs::read_to_string(&file).unwrap();
        let rel = file.strip_prefix(&root).unwrap().display().to_string();
        for (idx, line) in body.lines().enumerate() {
            if line.contains("Color::Rgb(") {
                rgb_leaks.push(format!("{rel}:{}: {}", idx + 1, line.trim()));
            }
        }
        assert!(
            !body.contains("fn clip_str("),
            "{rel} reintroduced scalar-count `clip_str`; use `tui::theme::clip_width` instead"
        );
    }
    assert!(
        rgb_leaks.is_empty(),
        "raw `Color::Rgb(` literals found outside `src/tui/theme.rs`:\n{}",
        rgb_leaks.join("\n")
    );
}

#[test]
fn canonical_subagent_config_and_agents_md_budget_stay_clean() {
    let root = repo_root();
    let src = root.join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);

    let allowed_bwrap = [
        src.join("exec/isolation.rs"),
        src.join("runtime/subagent.rs"),
        src.join("cli.rs"),
    ];
    for file in files {
        if allowed_bwrap.contains(&file) {
            continue;
        }
        let body = std::fs::read_to_string(&file).unwrap();
        assert!(
            !body.contains("\"/usr/bin/bwrap\""),
            "{} hardcoded \"/usr/bin/bwrap\" instead of using SubagentConfig::for_repo or exec::isolation",
            file.strip_prefix(&root).unwrap().display()
        );
    }

    let agents_md = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
    assert!(
        agents_md.lines().count() <= 120 && agents_md.len() <= 8_000,
        "AGENTS.md exceeded prompt budget ({} lines, {} bytes)",
        agents_md.lines().count(),
        agents_md.len()
    );
}
