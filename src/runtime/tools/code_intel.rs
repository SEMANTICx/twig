//! Structural code intelligence (`outline` tool): symbol maps, caller/definition
//! cross-reference tracing (`symbol`), module dependency extraction (`deps`),
//! and optional Language Server (`lsp`) delegation.

use std::path::Path;

use super::fs::{collect_files, resolve_existing_under, MAX_GREP_FILE_BYTES};

const MAX_OUTLINE_SYMBOLS_FILE: usize = 200;
const MAX_OUTLINE_SYMBOLS_DIR: usize = 180;

/// Zero-dependency structural symbol map, caller/definition cross-reference tracer (`symbol`),
/// and module import/dependency extractor (`deps`) for codebase reverse-comprehension.
pub(super) fn outline_stateless(
    worktree: &Path,
    subpath: Option<&str>,
    symbol: Option<&str>,
    deps: bool,
    use_lsp: bool,
) -> (String, Option<i32>) {
    let target_arg = subpath.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(".");
    let (root, base) = match resolve_existing_under(worktree, target_arg) {
        Ok(v) => v,
        Err(e) => return (e, None),
    };
    let mut lsp_fallback_note: Option<String> = None;
    if use_lsp {
        match super::lsp::query_lsp(&root, &base, symbol) {
            Ok(lsp_out) => return (lsp_out, Some(0)),
            Err(reason) => lsp_fallback_note = Some(format!("[LSP fallback: {reason}]\n\n")),
        }
    }
    let (body, code) = outline_heuristic(&root, &base, target_arg, symbol, deps);
    match lsp_fallback_note {
        Some(prefix) => (format!("{prefix}{body}"), code),
        None => (body, code),
    }
}

fn outline_heuristic(
    root: &Path,
    base: &Path,
    target_arg: &str,
    symbol: Option<&str>,
    deps: bool,
) -> (String, Option<i32>) {
    if let Some(sym) = symbol.map(str::trim).filter(|s| !s.is_empty()) {
        return trace_symbol_xrefs(root, base, sym);
    }
    if base.is_file() {
        let rel_file = base
            .strip_prefix(root)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| target_arg.to_owned());
        if super::super::policy::is_sensitive_secret_path(&rel_file) {
            return (
                format!("refused: outline on secret file {target_arg} is blocked"),
                None,
            );
        }
        let Ok(text) = std::fs::read_to_string(base) else {
            return (format!("{target_arg}: unreadable or binary file"), None);
        };
        let ext = base.extension().and_then(|s| s.to_str()).unwrap_or("");
        let mut sections = Vec::new();
        if deps {
            let imports = extract_file_deps(&text, ext, 48);
            if !imports.is_empty() {
                sections.push(format!("### Imports / Dependencies\n{}", imports.join("\n")));
            }
        }
        let mut syms = extract_file_symbols(&text, ext, MAX_OUTLINE_SYMBOLS_FILE);
        if syms.len() >= MAX_OUTLINE_SYMBOLS_FILE {
            syms.push(format!("[capped at {MAX_OUTLINE_SYMBOLS_FILE} symbols]"));
        }
        if !syms.is_empty() {
            sections.push(syms.join("\n"));
        }
        if sections.is_empty() {
            return (format!("no structural symbols detected in {target_arg}"), Some(0));
        }
        return (sections.join("\n\n"), Some(0));
    }

    let mut files = Vec::new();
    collect_files(base, root, &mut files);
    files.sort();

    let mut out = Vec::new();
    let mut total_syms = 0usize;
    let mut capped = false;
    for (rel, abs) in files {
        if super::super::policy::is_sensitive_secret_path(&rel) {
            continue;
        }
        let ext = abs.extension().and_then(|s| s.to_str()).unwrap_or("");
        if !matches!(
            ext,
            "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "py" | "go" | "c" | "h" | "cpp" | "hpp" | "java" | "kt" | "swift"
        ) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&abs) else {
            continue;
        };
        if meta.len() > MAX_GREP_FILE_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let rem = MAX_OUTLINE_SYMBOLS_DIR.saturating_sub(total_syms);
        if rem == 0 {
            capped = true;
            break;
        }
        let imports = if deps {
            extract_file_deps(&text, ext, 10)
        } else {
            Vec::new()
        };
        let syms = extract_file_symbols(&text, ext, rem.min(24));
        if syms.is_empty() && imports.is_empty() {
            continue;
        }
        total_syms += syms.len().max(1);
        let mut block = format!("## {rel}");
        if !imports.is_empty() {
            block.push_str(&format!("\n  deps: {}", imports.join(", ")));
        }
        if !syms.is_empty() {
            block.push_str(&format!("\n{}", syms.join("\n")));
        }
        out.push(block);
    }
    if out.is_empty() {
        return (format!("no source symbols found under {target_arg}"), Some(0));
    }
    if capped {
        out.push(format!(
            "[capped at {MAX_OUTLINE_SYMBOLS_DIR} symbols; pass a narrower `path` for full detail]"
        ));
    }
    (out.join("\n\n"), Some(0))
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn contains_ident(hay: &str, ident: &str) -> bool {
    let hb = hay.as_bytes();
    let nb = ident.as_bytes();
    if nb.is_empty() || nb.len() > hb.len() {
        return false;
    }
    let mut pos = 0usize;
    while let Some(rel) = hay[pos..].find(ident) {
        let idx = pos + rel;
        let end = idx + nb.len();
        let prev_ok = idx == 0 || !is_ident_byte(hb[idx - 1]);
        let next_ok = end == hb.len() || !is_ident_byte(hb[end]);
        if prev_ok && next_ok {
            return true;
        }
        pos = idx + 1;
    }
    false
}

fn is_symbol_def_of(trimmed: &str, sym: &str) -> bool {
    for kw in [
        "fn ",
        "struct ",
        "enum ",
        "trait ",
        "type ",
        "const ",
        "static ",
        "mod ",
        "def ",
        "class ",
        "function ",
        "interface ",
        "func ",
    ] {
        if let Some((_, after)) = trimmed.split_once(kw) {
            let rest = after.trim_start();
            let rest = if kw == "func " && rest.starts_with('(') {
                rest.split_once(')').map(|(_, r)| r.trim_start()).unwrap_or(rest)
            } else {
                rest
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name == sym {
                return true;
            }
        }
    }
    false
}

fn extract_scope_label(trimmed: &str, ext: &str) -> Option<String> {
    if extract_file_symbols(trimmed, ext, 1).is_empty() {
        return None;
    }
    for kw in [
        "fn ", "def ", "function ", "func ", "impl ", "impl<", "class ", "struct ", "trait ",
    ] {
        if let Some((_, after)) = trimmed.split_once(kw) {
            let clean: String = after
                .split(['(', '{', ':', ';'])
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(48)
                .collect();
            if !clean.is_empty() {
                let tag = kw.trim().trim_end_matches('<');
                return Some(format!("{tag} {clean}"));
            }
        }
    }
    None
}

fn sanitize_code_line(raw: &str, ext: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut in_str: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                out.push_str("  ");
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
                out.push(q as char);
            } else {
                out.push(' ');
            }
            i += 1;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            break;
        }
        if ext == "py" && b == b'#' {
            break;
        }
        if b == b'"' {
            in_str = Some(b'"');
            out.push('"');
            i += 1;
            continue;
        }
        if b == b'\'' && ext != "rs" {
            in_str = Some(b'\'');
            out.push('\'');
            i += 1;
            continue;
        }
        let ch = raw[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn trace_symbol_xrefs(root: &Path, base: &Path, sym: &str) -> (String, Option<i32>) {
    let mut files = Vec::new();
    collect_files(base, root, &mut files);
    files.sort();

    let mut defs = Vec::new();
    let mut refs = Vec::new();

    for (rel, abs) in files {
        if super::super::policy::is_sensitive_secret_path(&rel) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&abs) else {
            continue;
        };
        if meta.len() > MAX_GREP_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        if bytes.iter().take(512).any(|&b| b == 0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let ext = abs.extension().and_then(|s| s.to_str()).unwrap_or("");
        let mut scope_stack: Vec<(String, usize)> = Vec::new();
        let mut brace_depth = 0usize;

        for (idx, raw_line) in text.lines().enumerate() {
            let trimmed = raw_line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("//")
                || trimmed.starts_with("/*")
                || trimmed.starts_with('*')
                || (ext == "py" && trimmed.starts_with('#'))
            {
                continue;
            }
            let sanitized = sanitize_code_line(raw_line, ext);
            let clean_trim = sanitized.trim();
            let indent = raw_line.bytes().take_while(|&b| b == b' ' || b == b'\t').count();

            if ext == "py" {
                while scope_stack.last().is_some_and(|(_, d)| indent <= *d) {
                    scope_stack.pop();
                }
            } else {
                let leading_closes = clean_trim.bytes().take_while(|&b| b == b'}').count();
                let pre_depth = brace_depth.saturating_sub(leading_closes);
                while scope_stack.last().is_some_and(|(_, d)| pre_depth <= *d) {
                    scope_stack.pop();
                }
            }

            let active_ctx = if scope_stack.is_empty() {
                "top-level".to_owned()
            } else {
                scope_stack
                    .iter()
                    .map(|(s, _)| s.as_str())
                    .collect::<Vec<_>>()
                    .join(" :: ")
            };

            let opens = clean_trim.bytes().filter(|&b| b == b'{').count();
            let closes = clean_trim.bytes().filter(|&b| b == b'}').count();
            let next_depth = (brace_depth + opens).saturating_sub(closes);

            if let Some(next_scope) = extract_scope_label(clean_trim, ext) {
                if ext == "py" {
                    scope_stack.push((next_scope, indent));
                } else if opens > closes {
                    scope_stack.push((next_scope, brace_depth));
                }
            }
            brace_depth = next_depth;

            if !contains_ident(clean_trim, sym) {
                continue;
            }
            let display_line = trimmed.split("//").next().unwrap_or(trimmed).trim_end();
            let snippet: String = display_line.chars().take(120).collect();
            if is_symbol_def_of(clean_trim, sym) {
                if defs.len() < 40 {
                    defs.push(format!("[def] {rel}:{}: {snippet}", idx + 1));
                }
            } else if refs.len() < 80 {
                refs.push(format!("[in {active_ctx}] {rel}:{}: {snippet}", idx + 1));
            }
        }
    }

    if defs.is_empty() && refs.is_empty() {
        return (
            format!("no definitions or references found for identifier {sym:?}"),
            Some(0),
        );
    }
    let mut sections = Vec::new();
    if !defs.is_empty() {
        sections.push(format!(
            "### Definitions of `{sym}` ({})\n{}",
            defs.len(),
            defs.join("\n")
        ));
    }
    if !refs.is_empty() {
        sections.push(format!(
            "### Callers & References to `{sym}` ({})\n{}",
            refs.len(),
            refs.join("\n")
        ));
    }
    (sections.join("\n\n"), Some(0))
}

fn extract_file_deps(text: &str, ext: &str, cap: usize) -> Vec<String> {
    let mut deps = Vec::new();
    for raw in text.lines() {
        if deps.len() >= cap {
            break;
        }
        let t = raw.trim();
        let matched = match ext {
            "rs" => {
                t.starts_with("use ")
                    || t.starts_with("pub use ")
                    || t.starts_with("mod ")
                    || t.starts_with("pub mod ")
            }
            "py" => t.starts_with("import ") || t.starts_with("from "),
            "ts" | "tsx" | "js" | "jsx" | "mjs" => {
                t.starts_with("import ")
                    || t.starts_with("export ") && t.contains(" from ")
                    || t.contains("require(")
            }
            "go" => t.starts_with("import "),
            "c" | "h" | "cpp" | "hpp" => t.starts_with("#include "),
            _ => false,
        };
        if matched {
            let clean: String = t.trim_end_matches(';').chars().take(96).collect();
            deps.push(clean);
        }
    }
    deps
}

fn extract_file_symbols(text: &str, ext: &str, cap: usize) -> Vec<String> {
    let mut out = Vec::new();
    for (idx, raw_line) in text.lines().enumerate() {
        if out.len() >= cap {
            break;
        }
        let indent = raw_line.bytes().take_while(|&b| b == b' ' || b == b'\t').count();
        if indent > 8 {
            continue;
        }
        let trimmed = raw_line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }
        let is_sym = match ext {
            "rs" => {
                let s = trimmed
                    .strip_prefix("pub(crate) ")
                    .or_else(|| trimmed.strip_prefix("pub(super) "))
                    .or_else(|| trimmed.strip_prefix("pub "))
                    .unwrap_or(trimmed);
                let s = s.strip_prefix("async ").unwrap_or(s);
                let s = s.strip_prefix("unsafe ").unwrap_or(s);
                let s = s.strip_prefix("const ").unwrap_or(s);
                s.starts_with("fn ")
                    || s.starts_with("struct ")
                    || s.starts_with("enum ")
                    || s.starts_with("trait ")
                    || s.starts_with("type ")
                    || s.starts_with("mod ")
                    || trimmed.starts_with("impl ")
                    || trimmed.starts_with("impl<")
            }
            "py" => {
                trimmed.starts_with("def ")
                    || trimmed.starts_with("async def ")
                    || trimmed.starts_with("class ")
            }
            "ts" | "tsx" | "js" | "jsx" | "mjs" => {
                let s = trimmed.strip_prefix("export ").unwrap_or(trimmed);
                let s = s.strip_prefix("default ").unwrap_or(s);
                let s = s.strip_prefix("async ").unwrap_or(s);
                s.starts_with("function ")
                    || s.starts_with("class ")
                    || s.starts_with("interface ")
                    || s.starts_with("type ")
                    || s.starts_with("enum ")
                    || trimmed.starts_with("export const ")
            }
            "go" => trimmed.starts_with("func ") || trimmed.starts_with("type "),
            _ => {
                indent <= 4
                    && (trimmed.starts_with("class ")
                        || trimmed.starts_with("struct ")
                        || trimmed.starts_with("interface ")
                        || trimmed.starts_with("enum ")
                        || (trimmed.contains('(')
                            && trimmed.ends_with('{')
                            && !trimmed.starts_with("if ")
                            && !trimmed.starts_with("for ")
                            && !trimmed.starts_with("while ")
                            && !trimmed.starts_with("switch ")))
            }
        };
        if is_sym {
            let clean: String = raw_line
                .trim_end()
                .trim_end_matches('{')
                .trim_end()
                .chars()
                .take(120)
                .collect();
            out.push(format!("{:>4}: {clean}", idx + 1));
        }
    }
    out
}
