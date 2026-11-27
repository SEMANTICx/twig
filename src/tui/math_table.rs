//! Zero-dependency GFM pipe-table layout engine and Unicode LaTeX mathematical
//! pretty-printer for terminal TUI rendering (`$...$`, `$$...$$`, `\(...\)`, `\[...\]`).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const IVORY: Color = Color::Rgb(245, 241, 234);
const ACCENT: Color = Color::Rgb(217, 119, 87);
const ACCENT_PEAK: Color = Color::Rgb(245, 183, 153);
const SAGE: Color = Color::Rgb(136, 184, 136);
const AMBER: Color = Color::Rgb(229, 174, 92);
const MUTED: Color = Color::Rgb(152, 144, 134);
const HAIRLINE: Color = Color::Rgb(112, 105, 98);

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn clip_width(s: &str, max_w: usize) -> String {
    if display_width(s) <= max_w {
        return s.to_owned();
    }
    let limit = max_w.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > limit {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}


fn wrap_width(s: &str, max_w: usize) -> Vec<String> {
    let limit = max_w.max(4);
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + w > limit && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
            if ch == ' ' {
                continue;
            }
        }
        cur.push(ch);
        cur_w += w;
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColAlign {
    Left,
    Center,
    Right,
}

/// Check whether `line` looks like a GFM table separator row (`| --- | :---: | ---: |`).
pub(super) fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    if !t.contains('|') || !t.contains('-') {
        return false;
    }
    let cells = split_table_row(t);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let s = c.trim();
            !s.is_empty()
                && s.chars().any(|ch| ch == '-')
                && s.chars().all(|ch| ch == '-' || ch == ':')
        })
}

fn split_table_row(line: &str) -> Vec<String> {
    let mut trimmed = line.trim();
    if let Some(s) = trimmed.strip_prefix('|') {
        trimmed = s;
    }
    if let Some(s) = trimmed.strip_suffix('|') {
        if !s.ends_with('\\') {
            trimmed = s;
        }
    }
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&'|') {
            cur.push('|');
            chars.next();
        } else if ch == '|' {
            cells.push(cur.trim().to_owned());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    cells.push(cur.trim().to_owned());
    cells
}

/// Render consecutive GFM pipe-table lines into a CJK-width-aligned Unicode box table.
pub(super) fn render_gfm_table<F>(
    raw_rows: &[&str],
    max_width: usize,
    inline_fn: F,
) -> Option<Vec<Line<'static>>>
where
    F: Fn(&str, Style) -> Vec<Span<'static>>,
{
    if raw_rows.len() < 2 || !is_table_separator(raw_rows[1]) {
        return None;
    }
    let visible_inline_text = |raw: &str| -> String {
        inline_fn(raw, Style::default().fg(IVORY))
            .into_iter()
            .map(|sp| sp.content.into_owned())
            .collect()
    };
    let header_cells = split_table_row(raw_rows[0]);
    let sep_cells = split_table_row(raw_rows[1]);
    let cols = header_cells.len().max(sep_cells.len());
    if cols == 0 {
        return None;
    }

    let aligns: Vec<ColAlign> = (0..cols)
        .map(|i| {
            let s = sep_cells.get(i).map(|x| x.trim()).unwrap_or("---");
            match (s.starts_with(':'), s.ends_with(':')) {
                (true, true) => ColAlign::Center,
                (false, true) => ColAlign::Right,
                _ => ColAlign::Left,
            }
        })
        .collect();

    let mut data_rows: Vec<Vec<String>> = Vec::with_capacity(raw_rows.len().saturating_sub(2));
    for &row in &raw_rows[2..] {
        if is_table_separator(row) {
            continue;
        }
        let mut parsed = split_table_row(row);
        parsed.resize(cols, String::new());
        data_rows.push(parsed);
    }

    let mut col_w: Vec<usize> = (0..cols)
        .map(|c| {
            let h = header_cells.get(c).map(String::as_str).unwrap_or("");
            display_width(&visible_inline_text(h)).max(2)
        })
        .collect();
    for r in &data_rows {
        for (c, cell) in r.iter().enumerate().take(cols) {
            col_w[c] = col_w[c].max(display_width(&visible_inline_text(cell)));
        }
    }

    // Fit table cleanly within `max_width` (`│ cell │` adds `3 * cols + 1` border chars).
    let border_overhead = cols * 3 + 1;
    let avail = max_width.saturating_sub(border_overhead);
    if avail >= cols * 3 {
        while col_w.iter().sum::<usize>() > avail {
            if let Some((idx, &max_c)) = col_w.iter().enumerate().max_by_key(|&(_, &w)| w) {
                if max_c <= 3 {
                    break;
                }
                col_w[idx] -= 1;
            } else {
                break;
            }
        }
    }

    let border_style = Style::default().fg(HAIRLINE);
    let make_rule = |left: &str, mid: &str, right: &str| -> Line<'static> {
        let segs: Vec<String> = col_w.iter().map(|&w| "─".repeat(w + 2)).collect();
        Line::from(Span::styled(
            format!("{left}{}{right}", segs.join(mid)),
            border_style,
        ))
    };

    let render_row = |cells: &[String], is_header: bool| -> Line<'static> {
        let mut spans = vec![Span::styled("│", border_style)];
        let base_style = if is_header {
            Style::default()
                .fg(ACCENT_PEAK)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(IVORY)
        };
        for c in 0..cols {
            let raw_cell = cells.get(c).map(String::as_str).unwrap_or("");
            let vis = visible_inline_text(raw_cell);
            let target_w = col_w[c];
            let shown_spans = if display_width(&vis) > target_w {
                vec![Span::styled(clip_width(&vis, target_w), base_style)]
            } else if is_header {
                vec![Span::styled(vis.clone(), base_style)]
            } else {
                inline_fn(raw_cell, base_style)
            };
            let used: usize = shown_spans
                .iter()
                .map(|sp| display_width(sp.content.as_ref()))
                .sum();
            let pad = target_w.saturating_sub(used);
            let (lpad, rpad) = match aligns[c] {
                ColAlign::Left => (0, pad),
                ColAlign::Right => (pad, 0),
                ColAlign::Center => (pad / 2, pad - pad / 2),
            };
            spans.push(Span::raw(format!(" {}", " ".repeat(lpad))));
            spans.extend(shown_spans);
            spans.push(Span::raw(format!("{} ", " ".repeat(rpad))));
            spans.push(Span::styled("│", border_style));
        }
        Line::from(spans)
    };

    let mut out = Vec::with_capacity(data_rows.len() + 4);
    out.push(make_rule("┌", "┬", "┐"));
    let mut hdr = header_cells;
    hdr.resize(cols, String::new());
    out.push(render_row(&hdr, true));
    out.push(make_rule("├", "┼", "┤"));
    for r in data_rows {
        out.push(render_row(&r, false));
    }
    out.push(make_rule("└", "┴", "┘"));
    Some(out)
}

/// Render a display-math block (`$$ ... $$` or `\[ ... \]`) into indented styled terminal lines.
pub(super) fn render_display_math(raw_math: &str, max_width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let math_style = Style::default().fg(AMBER);
    let rail_style = Style::default().fg(HAIRLINE);
    for raw_line in raw_math.lines() {
        let t = raw_line.trim();
        if t.is_empty() {
            continue;
        }
        let pretty = format_latex_math(t);
        for (idx, wrapped) in wrap_width(&pretty, max_width.saturating_sub(4))
            .into_iter()
            .enumerate()
        {
            out.push(Line::from(vec![
                Span::styled(if idx == 0 { "  ∫ " } else { "    " }, rail_style),
                Span::styled(wrapped, math_style),
            ]));
        }
    }
    out
}


/// Convert LaTeX mathematical notation into clean Unicode mathematical symbols,
/// fractions, roots, Greek letters, operators, and superscripts/subscripts.
pub fn format_latex_math(input: &str) -> String {
    let mut s = input.trim().to_owned();
    // Strip harmless sizing / alignment tokens first.
    for tok in [
        "\\left", "\\right", "\\bigl", "\\bigr", "\\Bigl", "\\Bigr", "\\displaystyle", "\\textstyle",
        "\\!",
    ] {
        s = s.replace(tok, "");
    }
    s = s
        .replace("\\quad", "  ")
        .replace("\\qquad", "    ")
        .replace("\\,", " ")
        .replace("\\;", " ")
        .replace("\\:", " ")
        .replace("\\{", "{")
        .replace("\\}", "}")
        .replace("\\\\", " ; ")
        .replace('&', " ")
        .replace("^{\\circ}", "°")
        .replace("^\\circ", "°");

    // Unwrap text/font wrappers: `\text{...}`, `\mathrm{...}`, `\mathbf{...}`, `\mathcal{...}`
    for cmd in [
        "\\text",
        "\\textbf",
        "\\textit",
        "\\mathrm",
        "\\mathbf",
        "\\mathit",
        "\\mathsf",
        "\\mathtt",
        "\\operatorname",
    ] {
        s = replace_unary_brace(&s, cmd, |inner| inner.to_owned());
    }
    s = replace_unary_brace(&s, "\\mathbb", |inner| match inner.trim() {
        "R" => "ℝ".into(),
        "N" => "ℕ".into(),
        "Z" => "ℤ".into(),
        "Q" => "ℚ".into(),
        "C" => "ℂ".into(),
        "E" => "𝔼".into(),
        "P" => "ℙ".into(),
        other => other.to_owned(),
    });
    s = replace_unary_brace(&s, "\\hat", |inner| format!("{inner}\u{0302}"));
    s = replace_unary_brace(&s, "\\bar", |inner| format!("{inner}\u{0304}"));
    s = replace_unary_brace(&s, "\\vec", |inner| format!("{inner}\u{20D7}"));

    // Expand `\sqrt{...}`, `\binom{n}{k}`, and `\frac{a}{b}` inside-out up to 4 passes.
    for _ in 0..4 {
        let prev = s.clone();
        s = replace_unary_brace(&s, "\\sqrt", |inner| {
            let t = inner.trim();
            if t.chars().count() <= 2 && t.chars().all(|c| c.is_alphanumeric()) {
                format!("√{t}")
            } else {
                format!("√({t})")
            }
        });
        for fcmd in ["\\dfrac", "\\tfrac", "\\frac"] {
            s = replace_binary_brace(&s, fcmd, |a, b| {
                let fa = wrap_compound(a.trim());
                let fb = wrap_compound(b.trim());
                format!("{fa}/{fb}")
            });
        }
        s = replace_binary_brace(&s, "\\binom", |n, k| format!("C({}, {})", n.trim(), k.trim()));
        if s == prev {
            break;
        }
    }

    // Replace `\command` identifiers on exact ASCII-letter boundaries.
    s = replace_latex_commands(&s);

    // Convert `^{...}` / `^x` and `_{...}` / `_x` into Unicode super/subscripts where supported.
    s = convert_scripts(&s);
    s.trim().to_owned()
}

fn wrap_compound(expr: &str) -> String {
    let is_simple = !expr.is_empty()
        && expr
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '^' | '\\' | '.' | '√'));
    if is_simple {
        expr.to_owned()
    } else {
        format!("({expr})")
    }
}

fn find_balanced_brace(s: &str, open_byte: usize) -> Option<(usize, &str)> {
    if s.as_bytes().get(open_byte) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    for (rel, ch) in s[open_byte..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let inner = &s[open_byte + 1..open_byte + rel];
                    return Some((open_byte + rel + 1, inner));
                }
            }
            _ => {}
        }
    }
    None
}

fn replace_unary_brace(s: &str, cmd: &str, f: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pos = 0usize;
    while let Some(rel) = s[pos..].find(cmd) {
        let start = pos + rel;
        let after_cmd = start + cmd.len();
        if s[after_cmd..].chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            out.push_str(&s[pos..after_cmd]);
            pos = after_cmd;
            continue;
        }
        let ws_skip: usize = s[after_cmd..]
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(char::len_utf8)
            .sum();
        let brace_pos = after_cmd + ws_skip;
        if let Some((end_pos, inner)) = find_balanced_brace(s, brace_pos) {
            out.push_str(&s[pos..start]);
            out.push_str(&f(inner));
            pos = end_pos;
        } else {
            out.push_str(&s[pos..after_cmd]);
            pos = after_cmd;
        }
    }
    out.push_str(&s[pos..]);
    out
}


fn replace_binary_brace(s: &str, cmd: &str, f: impl Fn(&str, &str) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pos = 0usize;
    while let Some(rel) = s[pos..].find(cmd) {
        let start = pos + rel;
        let after_cmd = start + cmd.len();
        if s[after_cmd..].chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            out.push_str(&s[pos..after_cmd]);
            pos = after_cmd;
            continue;
        }
        let ws1: usize = s[after_cmd..]
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(char::len_utf8)
            .sum();
        if let Some((mid_pos, a)) = find_balanced_brace(s, after_cmd + ws1) {
            let ws2: usize = s[mid_pos..]
                .chars()
                .take_while(|c| c.is_whitespace())
                .map(char::len_utf8)
                .sum();
            if let Some((end_pos, b)) = find_balanced_brace(s, mid_pos + ws2) {
                out.push_str(&s[pos..start]);
                out.push_str(&f(a, b));
                pos = end_pos;
                continue;
            }
        }
        out.push_str(&s[pos..after_cmd]);
        pos = after_cmd;
    }
    out.push_str(&s[pos..]);
    out
}


fn lookup_latex_symbol(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" | "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" | "vartheta" => "θ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" | "varpi" => "π",
        "rho" | "varrho" => "ρ",
        "sigma" | "varsigma" => "σ",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" | "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "iint" => "∬",
        "oint" => "∮",
        "partial" => "∂",
        "nabla" => "∇",
        "infty" => "∞",
        "aleph" => "ℵ",
        "hbar" => "ℏ",
        "ell" => "ℓ",
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "cdot" => "·",
        "div" => "÷",
        "ast" | "star" => "∗",
        "circ" => "∘",
        "bullet" => "•",
        "oplus" => "⊕",
        "otimes" => "⊗",
        "leq" | "le" => "≤",
        "geq" | "ge" => "≥",
        "neq" | "ne" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "cong" => "≅",
        "sim" => "∼",
        "simeq" => "≃",
        "propto" => "∝",
        "ll" => "≪",
        "gg" => "≫",
        "to" | "rightarrow" | "longrightarrow" => "→",
        "leftarrow" | "gets" => "←",
        "leftrightarrow" => "↔",
        "Rightarrow" | "implies" | "Longrightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "Leftrightarrow" | "iff" => "⇔",
        "mapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "emptyset" | "varnothing" => "∅",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "neg" | "lnot" => "¬",
        "top" => "⊤",
        "bot" => "⊥",
        "perp" => "⊥",
        "parallel" => "∥",
        "angle" => "∠",
        "triangle" => "△",
        "dots" | "ldots" | "cdots" => "…",
        "sin" => "sin",
        "cos" => "cos",
        "tan" => "tan",
        "log" => "log",
        "ln" => "ln",
        "exp" => "exp",
        "lim" => "lim",
        "max" => "max",
        "min" => "min",
        "arg" => "arg",
        "sup" => "sup",
        "inf" => "inf",
        "det" => "det",
        "deg" => "°",
        _ => return None,
    })
}

fn replace_latex_commands(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let ch = s[i..].chars().next().unwrap();
        if ch == '\\' {
            let start = i + 1;
            let mut end = start;
            for c in s[start..].chars() {
                if c.is_ascii_alphabetic() {
                    end += 1;
                } else {
                    break;
                }
            }
            if end > start {
                let name = &s[start..end];
                if let Some(sym) = lookup_latex_symbol(name) {
                    out.push_str(sym);
                    i = end;
                    if s[i..].starts_with(' ')
                        && s[i + 1..]
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_alphabetic() || c.is_ascii_digit())
                        && sym.chars().all(|c| !c.is_ascii_alphabetic())
                    {
                        i += 1;
                    }
                    continue;
                }
            }
        }
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}


fn to_superscript_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        'T' => 'ᵀ',
        'k' => 'ᵏ',
        'm' => 'ᵐ',
        'x' => 'ˣ',
        _ => return None,
    })
}

fn to_subscript_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

fn convert_scripts(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let ch = s[i..].chars().next().unwrap();
        if ch == '^' || ch == '_' {
            let is_sup = ch == '^';
            let mapper = if is_sup {
                to_superscript_char
            } else {
                to_subscript_char
            };
            let next_pos = i + 1;
            if let Some((end_pos, inner)) = find_balanced_brace(s, next_pos) {
                let trimmed = inner.trim();
                let mapped: Option<String> = trimmed.chars().map(mapper).collect();
                if let Some(m) = mapped.filter(|m| !m.is_empty()) {
                    out.push_str(&m);
                } else {
                    out.push(ch);
                    out.push('(');
                    out.push_str(trimmed);
                    out.push(')');
                }
                i = end_pos;
                continue;
            } else if let Some(single) = s[next_pos..].chars().next() {
                if let Some(m) = mapper(single) {
                    out.push(m);
                    i = next_pos + single.len_utf8();
                    continue;
                }
            }
        }
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Render a fenced `mermaid` diagram (`flowchart`/`graph` or `sequenceDiagram`) as a
/// clean, width-aware Unicode box-and-arrow diagram directly in the terminal.
pub(super) fn render_mermaid_diagram(code: &str, max_width: usize) -> Vec<Line<'static>> {
    let lines: Vec<&str> = code
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"))
        .collect();
    let Some(&first) = lines.first() else {
        return Vec::new();
    };
    let lower = first.to_ascii_lowercase();
    if lower.starts_with("sequencediagram") {
        return render_sequence_diagram(&lines[1..], max_width);
    }
    if lower.starts_with("flowchart") || lower.starts_with("graph") {
        let horizontal = lower.contains(" lr") || lower.ends_with("lr") || lower.contains(" rl");
        return render_flowchart(&lines[1..], max_width, horizontal);
    }

    let mut out = vec![Line::from(Span::styled(
        format!("╭─ ◇ mermaid · {first}"),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))];
    for &ln in &lines[1..] {
        out.push(Line::from(vec![
            Span::styled("│  ", Style::default().fg(HAIRLINE)),
            Span::styled(clip_width(ln, max_width.saturating_sub(4)), Style::default().fg(IVORY)),
        ]));
    }
    out.push(Line::from(Span::styled(
        format!("╰{}", "─".repeat(max_width.min(44).saturating_sub(1))),
        Style::default().fg(HAIRLINE),
    )));
    out
}

fn parse_mermaid_node(raw: &str) -> (String, String) {
    let t = raw.trim().trim_end_matches(';').trim();
    for (open, close, prefix) in [
        ("[\"", "\"]", ""),
        ("[", "]", ""),
        ("(\"", "\")", ""),
        ("(", ")", ""),
        ("{\"", "\"}", "◇ "),
        ("{", "}", "◇ "),
    ] {
        if let Some(pos) = t.find(open) {
            if let Some(body) = t[pos + open.len()..].strip_suffix(close) {
                let id = t[..pos].trim().to_owned();
                let clean = body.trim().trim_matches('"');
                return (
                    if id.is_empty() { clean.to_owned() } else { id },
                    format!("{prefix}{clean}"),
                );
            }
        }
    }
    (t.to_owned(), t.trim_matches('"').to_owned())
}

fn split_mermaid_edge(line: &str) -> Option<(&str, Option<&str>, &str)> {
    // Match `A -->|label| B` or `A -- label --> B` or plain `A --> B`, `A -.-> B`, `A ==> B`
    for arrow in ["-->|", "-.->|", "==>|"] {
        if let Some(p) = line.find(arrow) {
            let after = &line[p + arrow.len()..];
            if let Some(q) = after.find('|') {
                return Some((&line[..p], Some(after[..q].trim()), after[q + 1..].trim()));
            }
        }
    }
    for arrow in ["-.->", "==>", "-->", "---", "->"] {
        if let Some(p) = line.find(arrow) {
            let lhs = line[..p].trim();
            let rhs = line[p + arrow.len()..].trim();
            if !lhs.is_empty() && !rhs.is_empty() {
                if let Some(dash) = lhs.find(" -- ") {
                    return Some((&lhs[..dash], Some(lhs[dash + 4..].trim()), rhs));
                }
                return Some((lhs, None, rhs));
            }
        }
    }
    None
}

fn render_flowchart(body: &[&str], max_width: usize, prefer_lr: bool) -> Vec<Line<'static>> {
    let mut order: Vec<String> = Vec::new();
    let mut labels: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut edges: Vec<(String, String, Option<String>)> = Vec::new();

    let mut reg_node = |id: String, lbl: String| {
        if !order.contains(&id) {
            order.push(id.clone());
        }
        let existing = labels.entry(id.clone()).or_insert_with(|| lbl.clone());
        if existing == &id && lbl != id {
            *existing = lbl;
        }
    };

    for &raw in body {
        if raw.starts_with("subgraph") || raw == "end" || raw.starts_with("style ") || raw.starts_with("classDef ") {
            continue;
        }
        if let Some((l, elbl, r)) = split_mermaid_edge(raw) {
            let (id1, lb1) = parse_mermaid_node(l);
            let (id2, lb2) = parse_mermaid_node(r);
            reg_node(id1.clone(), lb1);
            reg_node(id2.clone(), lb2);
            edges.push((id1, id2, elbl.map(str::to_owned)));
        } else {
            let (id, lb) = parse_mermaid_node(raw);
            if !id.is_empty() {
                reg_node(id, lb);
            }
        }
    }

    if order.is_empty() {
        return Vec::new();
    }

    let is_simple_chain = edges.len() + 1 == order.len()
        && (0..edges.len()).all(|i| edges[i].0 == order[i] && edges[i].1 == order[i + 1]);

    // Horizontal side-by-side boxes when `LR` chain fits terminal width.
    if prefer_lr && is_simple_chain {
        let box_widths: Vec<usize> = order
            .iter()
            .map(|id| display_width(&labels[id]).max(3) + 4)
            .collect();
        let arrows_w: usize = edges
            .iter()
            .map(|(_, _, el)| el.as_deref().map_or(5, |t| display_width(t) + 6))
            .sum();
        if box_widths.iter().sum::<usize>() + arrows_w + 2 <= max_width {
            let mut top = vec![Span::raw("  ")];
            let mut mid = vec![Span::raw("  ")];
            let mut bot = vec![Span::raw("  ")];
            let bdr = Style::default().fg(HAIRLINE);
            let txt = Style::default().fg(ACCENT_PEAK).add_modifier(Modifier::BOLD);
            for (idx, id) in order.iter().enumerate() {
                let lbl = &labels[id];
                let inner_w = display_width(lbl).max(3) + 2;
                let pad = inner_w.saturating_sub(display_width(lbl));
                let lpad = pad / 2;
                let rpad = pad - lpad;
                top.push(Span::styled(format!("╭{}╮", "─".repeat(inner_w)), bdr));
                mid.push(Span::styled("│", bdr));
                mid.push(Span::styled(
                    format!("{}{}{}", " ".repeat(lpad), lbl, " ".repeat(rpad)),
                    txt,
                ));
                mid.push(Span::styled("│", bdr));
                bot.push(Span::styled(format!("╰{}╯", "─".repeat(inner_w)), bdr));
                if idx < edges.len() {
                    let arrow_str = match &edges[idx].2 {
                        Some(el) => format!(" ─{el}─▶ "),
                        None => " ──▶ ".to_owned(),
                    };
                    let aw = display_width(&arrow_str);
                    top.push(Span::raw(" ".repeat(aw)));
                    mid.push(Span::styled(arrow_str, Style::default().fg(AMBER)));
                    bot.push(Span::raw(" ".repeat(aw)));
                }
            }
            return vec![Line::from(top), Line::from(mid), Line::from(bot)];
        }
    }

    // Top-down / branching Unicode diagram.
    let mut out = Vec::new();
    let bdr = Style::default().fg(HAIRLINE);
    let node_style = Style::default().fg(ACCENT_PEAK).add_modifier(Modifier::BOLD);
    for (idx, id) in order.iter().enumerate() {
        let lbl = clip_width(&labels[id], max_width.saturating_sub(8));
        let iw = display_width(&lbl).max(4) + 2;
        let pad = iw.saturating_sub(display_width(&lbl));
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("╭{}╮", "─".repeat(iw)), bdr),
        ]));
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("│", bdr),
            Span::styled(
                format!("{}{}{}", " ".repeat(pad / 2), lbl, " ".repeat(pad - pad / 2)),
                node_style,
            ),
            Span::styled("│", bdr),
        ]));
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("╰{}╯", "─".repeat(iw)), bdr),
        ]));

        let outgoing: Vec<&(String, String, Option<String>)> =
            edges.iter().filter(|(s, _, _)| s == id).collect();
        if outgoing.len() == 1 && idx + 1 < order.len() && outgoing[0].1 == order[idx + 1] {
            let note = outgoing[0]
                .2
                .as_deref()
                .map(|e| format!(" {e}"))
                .unwrap_or_default();
            out.push(Line::from(vec![
                Span::styled("     │", Style::default().fg(AMBER)),
                Span::styled(note, Style::default().fg(MUTED)),
            ]));
            out.push(Line::from(Span::styled("     ▼", Style::default().fg(AMBER))));
        } else if !outgoing.is_empty() {
            for (e_idx, (_, dst, elbl)) in outgoing.iter().enumerate() {
                let stem = if e_idx + 1 == outgoing.len() {
                    "   └─"
                } else {
                    "   ├─"
                };
                let dst_lbl = labels.get(dst).map(String::as_str).unwrap_or(dst);
                let tag = elbl
                    .as_deref()
                    .map(|e| format!("[{e}]─▶ "))
                    .unwrap_or_else(|| "▶ ".into());
                out.push(Line::from(vec![
                    Span::styled(format!("{stem}{tag}"), Style::default().fg(AMBER)),
                    Span::styled(format!("[{dst_lbl}]"), Style::default().fg(IVORY)),
                ]));
            }
        }
    }
    out
}

fn render_sequence_diagram(body: &[&str], max_width: usize) -> Vec<Line<'static>> {
    let mut actors: Vec<(String, String)> = Vec::new();
    let mut steps: Vec<(String, String, String, bool)> = Vec::new();
    let ensure_actor = |actors: &mut Vec<(String, String)>, id: &str, alias: Option<&str>| {
        if let Some(pos) = actors.iter().position(|(a, _)| a == id) {
            if let Some(al) = alias {
                actors[pos].1 = al.to_owned();
            }
        } else {
            actors.push((id.to_owned(), alias.unwrap_or(id).to_owned()));
        }
    };

    for &ln in body {
        if let Some(rest) = ln.strip_prefix("participant ").or_else(|| ln.strip_prefix("actor ")) {
            if let Some((id, al)) = rest.split_once(" as ") {
                ensure_actor(&mut actors, id.trim(), Some(al.trim()));
            } else {
                ensure_actor(&mut actors, rest.trim(), None);
            }
            continue;
        }
        for (tok, dashed) in [("-->>", true), ("->>", false), ("-->", true), ("->", false)] {
            if let Some(p) = ln.find(tok) {
                let src = ln[..p].trim();
                let rhs = ln[p + tok.len()..].trim();
                if let Some((dst, msg)) = rhs.split_once(':') {
                    let dst = dst.trim().trim_end_matches(['+', '-']);
                    ensure_actor(&mut actors, src, None);
                    ensure_actor(&mut actors, dst, None);
                    steps.push((src.to_owned(), dst.to_owned(), msg.trim().to_owned(), dashed));
                    break;
                }
            }
        }
    }

    if actors.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let bdr = Style::default().fg(HAIRLINE);
    let actor_line: Vec<Span<'static>> = actors
        .iter()
        .enumerate()
        .flat_map(|(i, (_, name))| {
            let mut v = Vec::new();
            if i == 0 {
                v.push(Span::raw("  "));
            } else {
                v.push(Span::styled(" ─── ", bdr));
            }
            v.push(Span::styled(
                format!("[{name}]"),
                Style::default().fg(ACCENT_PEAK).add_modifier(Modifier::BOLD),
            ));
            v
        })
        .collect();
    out.push(Line::from(actor_line));
    for (src, dst, msg, dashed) in steps {
        let s_name = actors
            .iter()
            .find(|(id, _)| id == &src)
            .map(|(_, n)| n.as_str())
            .unwrap_or(&src);
        let d_name = actors
            .iter()
            .find(|(id, _)| id == &dst)
            .map(|(_, n)| n.as_str())
            .unwrap_or(&dst);
        let arrow = if dashed { " ╌╌▶ " } else { " ──▶ " };
        let clipped_msg = clip_width(
            &msg,
            max_width.saturating_sub(display_width(s_name) + display_width(d_name) + 12),
        );
        out.push(Line::from(vec![
            Span::styled("  │ ", bdr),
            Span::styled(s_name.to_owned(), Style::default().fg(SAGE)),
            Span::styled(arrow, Style::default().fg(AMBER)),
            Span::styled(d_name.to_owned(), Style::default().fg(ACCENT_PEAK)),
            Span::styled(format!(" : {clipped_msg}"), Style::default().fg(IVORY)),
        ]));
    }
    out
}

/// Extract structure, dimensions, and text labels from an inline `<svg>` block so TUI users
/// see a meaningful visual summary card instead of raw XML path coordinates.
pub(super) fn render_svg_summary(svg: &str, max_width: usize) -> Vec<Line<'static>> {
    let extract_attr = |attr: &str| -> Option<String> {
        let pat = format!("{attr}=\"");
        let start = svg.find(&pat)? + pat.len();
        let end = svg[start..].find('"')?;
        Some(svg[start..start + end].trim().to_owned())
    };
    let dims = match (extract_attr("width"), extract_attr("height")) {
        (Some(w), Some(h)) => format!("{w}×{h}"),
        _ => extract_attr("viewBox")
            .map(|vb| {
                let parts: Vec<&str> = vb.split_whitespace().collect();
                if parts.len() == 4 {
                    format!("{}×{}", parts[2], parts[3])
                } else {
                    vb
                }
            })
            .unwrap_or_else(|| "vector".into()),
    };
    let shapes = ["<rect", "<circle", "<ellipse", "<path", "<line", "<polygon", "<polyline"]
        .iter()
        .map(|tag| svg.matches(tag).count())
        .sum::<usize>();

    let mut texts = Vec::new();
    let mut cursor = svg;
    while let Some(open) = cursor.find("<text") {
        let after = &cursor[open + 5..];
        let Some(gt) = after.find('>') else { break };
        let content_start = &after[gt + 1..];
        let Some(close) = content_start.find("</text>") else {
            break;
        };
        let raw_inner = &content_start[..close];
        // Strip nested `<tspan>` tags cleanly.
        let mut clean = String::new();
        let mut in_tag = false;
        for ch in raw_inner.chars() {
            match ch {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => clean.push(ch),
                _ => {}
            }
        }
        let t = clean.trim();
        if !t.is_empty() && !texts.iter().any(|x| x == t) {
            texts.push(t.to_owned());
        }
        cursor = &content_start[close + 7..];
    }

    let bdr = Style::default().fg(HAIRLINE);
    let mut out = vec![Line::from(vec![
        Span::styled("╭─ ", bdr),
        Span::styled(
            "◈ SVG ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{dims} · {shapes} shapes · Alt+Y copy XML · twig web vector view"),
            Style::default().fg(MUTED),
        ),
    ])];
    if !texts.is_empty() {
        let joined = texts
            .iter()
            .map(|t| format!("[{t}]"))
            .collect::<Vec<_>>()
            .join("  ·  ");
        for wrapped in wrap_width(&joined, max_width.saturating_sub(4)) {
            out.push(Line::from(vec![
                Span::styled("│  ", bdr),
                Span::styled(wrapped, Style::default().fg(ACCENT_PEAK)),
            ]));
        }
    }
    out.push(Line::from(Span::styled(
        format!("╰{}", "─".repeat(max_width.min(52).saturating_sub(1))),
        bdr,
    )));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_gfm_table_and_latex_math_cleanly() {
        let rows = [
            "| 日期 | 天气 | 温度 |",
            "|---|:---:|---:|",
            "| 11-27 | 晴 | 19℃ |",
            "| 11-28 | 多云 | 22℃ |",
        ];
        let rendered =
            render_gfm_table(&rows, 60, |s, st| vec![Span::styled(s.to_owned(), st)]).unwrap();
        assert_eq!(rendered.len(), 6);
        assert!(rendered[0].to_string().starts_with('┌'));
        assert!(rendered[1].to_string().contains("日期") && rendered[1].to_string().contains("温度"));
        assert!(rendered[5].to_string().starts_with('└'));

        let eq = format_latex_math(r"\Delta y_i = \sum_{k=1}^{n} \alpha_k x^2 + \frac{a+b}{\sqrt{t}} \leq \infty");
        assert_eq!(eq, "Δyᵢ = ∑ₖ₌₁ⁿ αₖ x² + (a+b)/√t ≤ ∞");
    }
}
