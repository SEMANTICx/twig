//! Streaming Markdown renderer and fenced code block extractor for `twig chat`:
//! GFM box tables, Mermaid/SVG diagrams, Unicode LaTeX math, line-numbered code blocks,
//! GitHub callout alerts, task checklists, and unified diff gutters.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::highlight::{highlight_code_line, DiffGutterState};
use super::theme::{
    wrap_plain, ACCENT, ACCENT_PEAK, AMBER, HAIRLINE, IVORY, MUTED, ROSE, SAGE, STONE, SURFACE,
};

/// Parse inline `` `code` ``, `**bold**`, `*italic*`, `$latex$` / `\(latex\)`, and `[text](url)` spans.
fn parse_inline_inner(
    text: &str,
    base: Style,
    mut links: Option<&mut super::math_table::LinkRegistry>,
    depth: u8,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let flush = |buf: &mut String, style: Style, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), style));
        }
    };

    while i < chars.len() {
        if chars[i] == '`' {
            if let Some(rel_end) = chars[i + 1..].iter().position(|&c| c == '`') {
                flush(&mut buf, base, &mut spans);
                let code: String = chars[i + 1..i + 1 + rel_end].iter().collect();
                spans.push(Span::styled(
                    format!(" {code} "),
                    base.fg(ACCENT_PEAK).bg(SURFACE),
                ));
                i += rel_end + 2;
                continue;
            }
        }
        // Inline LaTeX `\(...\)`
        if chars[i] == '\\' && chars.get(i + 1) == Some(&'(') {
            let mut end = None;
            let mut j = i + 2;
            while j + 1 < chars.len() {
                if chars[j] == '\\' && chars[j + 1] == ')' {
                    end = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end_idx) = end {
                flush(&mut buf, base, &mut spans);
                let raw_math: String = chars[i + 2..end_idx].iter().collect();
                spans.push(Span::styled(
                    super::math_table::format_latex_math(&raw_math),
                    base.fg(AMBER),
                ));
                i = end_idx + 2;
                continue;
            }
        }
        // Inline LaTeX `$...$` (ignores `$$`, `$100` currency, and `$skill` unclosed tokens).
        if chars[i] == '$' && chars.get(i + 1) != Some(&'$') && (i == 0 || chars[i - 1] != '\\') {
            let next_ch = chars.get(i + 1).copied();
            if next_ch.is_some_and(|c| !c.is_whitespace()) {
                let mut end = None;
                let mut j = i + 1;
                while j < chars.len() {
                    if chars[j] == '$' && chars[j - 1] != '\\' {
                        if !chars[j - 1].is_whitespace()
                            && !chars.get(j + 1).is_some_and(|c| c.is_ascii_alphanumeric())
                        {
                            end = Some(j);
                        }
                        break;
                    }
                    j += 1;
                }
                if let Some(end_idx) = end {
                    let raw_math: String = chars[i + 1..end_idx].iter().collect();
                    let looks_currency = raw_math
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '.' || c == ',');
                    if !looks_currency {
                        flush(&mut buf, base, &mut spans);
                        spans.push(Span::styled(
                            super::math_table::format_latex_math(&raw_math),
                            base.fg(AMBER),
                        ));
                        i = end_idx + 1;
                        continue;
                    }
                }
            }
        }
        // Markdown link `[label](url)` — compact inline reference `[n]` with clean footnote URLs
        if chars[i] == '[' {
            if let Some(rel_rb) = chars[i + 1..].iter().position(|&c| c == ']') {
                let rb = i + 1 + rel_rb;
                if chars.get(rb + 1) == Some(&'(') {
                    if let Some(rel_rp) = chars[rb + 2..].iter().position(|&c| c == ')') {
                        let rp = rb + 2 + rel_rp;
                        flush(&mut buf, base, &mut spans);
                        let label: String = chars[i + 1..rb].iter().collect();
                        let url: String = chars[rb + 2..rp].iter().collect();
                        let link_style = base.fg(ACCENT_PEAK).add_modifier(Modifier::UNDERLINED);
                        if depth < 2 {
                            spans.extend(parse_inline_inner(&label, link_style, None, depth + 1));
                        } else {
                            spans.push(Span::styled(label.clone(), link_style));
                        }
                        if let Some(suffix) =
                            super::math_table::format_link_suffix(&label, url.trim(), links.as_deref_mut())
                        {
                            spans.push(suffix);
                        }
                        i = rp + 1;
                        continue;
                    }
                }
            }
        }
        // `**bold**`
        if chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            let mut end = None;
            let mut j = i + 2;
            while j + 1 < chars.len() {
                if chars[j] == '*' && chars[j + 1] == '*' {
                    end = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end_idx) = end {
                flush(&mut buf, base, &mut spans);
                let bold: String = chars[i + 2..end_idx].iter().collect();
                let bold_style = base.fg(IVORY).add_modifier(Modifier::BOLD);
                if depth < 2 {
                    spans.extend(parse_inline_inner(
                        &bold,
                        bold_style,
                        links.as_deref_mut(),
                        depth + 1,
                    ));
                } else {
                    spans.push(Span::styled(bold, bold_style));
                }
                i = end_idx + 2;
                continue;
            }
        }
        // `*italic*`
        if chars[i] == '*' && chars.get(i + 1).is_some_and(|&c| c != '*' && !c.is_whitespace()) {
            if let Some(rel_end) = chars[i + 1..]
                .iter()
                .position(|&c| c == '*')
                .filter(|&pos| pos > 0 && !chars[i + pos].is_whitespace())
            {
                flush(&mut buf, base, &mut spans);
                let italic: String = chars[i + 1..i + 1 + rel_end].iter().collect();
                let it_style = base.add_modifier(Modifier::ITALIC);
                if depth < 2 {
                    spans.extend(parse_inline_inner(
                        &italic,
                        it_style,
                        links.as_deref_mut(),
                        depth + 1,
                    ));
                } else {
                    spans.push(Span::styled(italic, it_style));
                }
                i += rel_end + 2;
                continue;
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, base, &mut spans);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

/// Extract deduplicated `[label](url)` targets from `text` in the exact 1-based `[n]` order
/// assigned by `render_markdown`, so clicking `[n]` or running `/open n` resolves intact URLs.
pub fn extract_markdown_links(text: &str) -> Vec<String> {
    let mut reg = super::math_table::LinkRegistry::default();
    let mut in_fence = false;
    for raw_line in text.split('\n') {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let _ = parse_inline_inner(trimmed, Style::default(), Some(&mut reg), 0);
    }
    let _ = reg.drain_footnotes();
    reg.into_urls()
}

/// Render streaming assistant markdown into wrapped styled Ratatui lines.
pub fn render_markdown(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let width = max_width.max(12);
    let mut out = Vec::new();
    let body = Style::default().fg(IVORY);
    let links = std::cell::RefCell::new(super::math_table::LinkRegistry::default());

    let raw_lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .collect();
    let mut idx = 0usize;

    while idx < raw_lines.len() {
        let line = raw_lines[idx];
        let lead_cols: usize = line
            .chars()
            .take_while(|c| matches!(c, ' ' | '\t'))
            .map(|c| if c == '\t' { 2 } else { 1 })
            .sum();
        let trimmed = line.trim_start();

        if let Some(fence) = trimmed.strip_prefix("```") {
            let lang_raw = fence.trim();
            let code_lang = lang_raw.to_ascii_lowercase();
            idx += 1;
            let mut block_lines: Vec<&str> = Vec::new();
            while idx < raw_lines.len() && !raw_lines[idx].trim_start().starts_with("```") {
                block_lines.push(raw_lines[idx]);
                idx += 1;
            }
            if idx < raw_lines.len() {
                idx += 1;
            }
            out.extend(render_fenced_block(lang_raw, &code_lang, &block_lines, width));
            continue;
        }

        if trimmed.starts_with("$$") || trimmed.starts_with("\\[") {
            let close_tok = if trimmed.starts_with("$$") { "$$" } else { "\\]" };
            let after_open = &trimmed[2..];
            if let Some(close_pos) = after_open.find(close_tok) {
                let single_math = &after_open[..close_pos];
                out.extend(super::math_table::render_display_math(single_math, width));
                idx += 1;
                continue;
            }
            let mut math_buf = Vec::new();
            if !after_open.trim().is_empty() {
                math_buf.push(after_open);
            }
            idx += 1;
            while idx < raw_lines.len() {
                let cur = raw_lines[idx].trim();
                if let Some(before_close) = cur.strip_suffix(close_tok) {
                    if !before_close.trim().is_empty() {
                        math_buf.push(before_close);
                    }
                    idx += 1;
                    break;
                }
                if cur == close_tok {
                    idx += 1;
                    break;
                }
                math_buf.push(raw_lines[idx]);
                idx += 1;
            }
            out.extend(super::math_table::render_display_math(
                &math_buf.join("\n"),
                width,
            ));
            continue;
        }

        if trimmed.contains('|')
            && idx + 1 < raw_lines.len()
            && super::math_table::is_table_separator(raw_lines[idx + 1])
        {
            let start = idx;
            idx += 2;
            while idx < raw_lines.len() {
                let r = raw_lines[idx].trim();
                if r.is_empty() || !r.contains('|') {
                    break;
                }
                idx += 1;
            }
            if let Some(tbl_lines) = super::math_table::render_gfm_table(
                &raw_lines[start..idx],
                width,
                |s, st| parse_inline_inner(s, st, Some(&mut links.borrow_mut()), 0),
            ) {
                out.extend(tbl_lines);
                out.extend(links.borrow_mut().drain_footnotes());
                continue;
            }
            idx = start;
        }

        if trimmed.is_empty() {
            out.extend(links.borrow_mut().drain_footnotes());
            out.push(Line::from(""));
            idx += 1;
            continue;
        }

        if trimmed.len() >= 3
            && (trimmed.chars().all(|c| c == '-')
                || trimmed.chars().all(|c| c == '*')
                || trimmed.chars().all(|c| c == '_'))
        {
            out.push(Line::from(Span::styled(
                "─".repeat(width.min(48)),
                Style::default().fg(HAIRLINE),
            )));
            idx += 1;
            continue;
        }

        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
            out.extend(links.borrow_mut().drain_footnotes());
            let rest = trimmed[hashes + 1..].trim();
            let heading = if hashes <= 2 {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ACCENT_PEAK).add_modifier(Modifier::BOLD)
            };
            let parsed = parse_inline_inner(rest, heading, Some(&mut links.borrow_mut()), 0);
            for row_spans in wrap_spans(parsed, width) {
                out.push(Line::from(row_spans));
            }
            out.extend(links.borrow_mut().drain_footnotes());
            idx += 1;
            continue;
        }

        if trimmed.starts_with('>') {
            out.extend(links.borrow_mut().drain_footnotes());
            let mut quote_lines: Vec<&str> = Vec::new();
            while idx < raw_lines.len() {
                let qt = raw_lines[idx].trim_start();
                if let Some(rest) = qt.strip_prefix("> ").or_else(|| qt.strip_prefix('>')) {
                    quote_lines.push(rest);
                    idx += 1;
                } else {
                    break;
                }
            }
            let first_q = quote_lines.first().copied().unwrap_or("").trim();
            let alert = [
                ("[!NOTE]", "◈ Note", ACCENT_PEAK),
                ("[!TIP]", "✦ Tip", SAGE),
                ("[!IMPORTANT]", "◈ Important", ACCENT),
                ("[!WARNING]", "▲ Warning", AMBER),
                ("[!CAUTION]", "✕ Caution", ROSE),
            ]
            .into_iter()
            .find_map(|(tag, badge, col)| {
                first_q
                    .strip_prefix(tag)
                    .map(|rem| (badge, col, rem.trim()))
            });

            let push_quote_line =
                |out: &mut Vec<Line<'static>>, qtext: &str, bar_col: Color, text_style: Style| {
                    let parsed =
                        parse_inline_inner(qtext, text_style, Some(&mut links.borrow_mut()), 0);
                    for row_spans in wrap_spans(parsed, width.saturating_sub(2)) {
                        let mut spans = vec![Span::styled("▎ ", Style::default().fg(bar_col))];
                        spans.extend(row_spans);
                        out.push(Line::from(spans));
                    }
                };
            if let Some((badge, col, first_rem)) = alert {
                out.push(Line::from(vec![
                    Span::styled("▎ ", Style::default().fg(col)),
                    Span::styled(badge, Style::default().fg(col).add_modifier(Modifier::BOLD)),
                ]));
                if !first_rem.is_empty() {
                    push_quote_line(&mut out, first_rem, col, Style::default().fg(IVORY));
                }
                for &ql in &quote_lines[1..] {
                    push_quote_line(&mut out, ql, col, Style::default().fg(IVORY));
                }
            } else {
                for ql in quote_lines {
                    push_quote_line(&mut out, ql, ACCENT, Style::default().fg(STONE));
                }
            }
            out.extend(links.borrow_mut().drain_footnotes());
            continue;
        }

        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let depth = (lead_cols / 2).min(4);
            let nest = "  ".repeat(depth);
            let (bullet, bullet_color, item_rest) = if let Some(r) =
                rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] "))
            {
                ("✓ ", SAGE, r)
            } else if let Some(r) = rest.strip_prefix("[ ] ") {
                ("☐ ", MUTED, r)
            } else {
                match depth {
                    0 => ("• ", ACCENT, rest),
                    1 => ("◦ ", ACCENT_PEAK, rest),
                    _ => ("▪ ", STONE, rest),
                }
            };
            let indent_width = width.saturating_sub(nest.len() + 2);
            let parsed = parse_inline_inner(item_rest, body, Some(&mut links.borrow_mut()), 0);
            for (w_idx, row_spans) in wrap_spans(parsed, indent_width).into_iter().enumerate() {
                let mut spans = vec![
                    Span::raw(nest.clone()),
                    Span::styled(
                        if w_idx == 0 { bullet } else { "  " },
                        Style::default().fg(bullet_color),
                    ),
                ];
                spans.extend(row_spans);
                out.push(Line::from(spans));
            }
            idx += 1;
            continue;
        }

        let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
        if (1..=3).contains(&digits)
            && matches!(trimmed.as_bytes().get(digits), Some(b'.' | b')'))
            && trimmed.as_bytes().get(digits + 1) == Some(&b' ')
        {
            let depth = (lead_cols / 2).min(4);
            let nest = "  ".repeat(depth);
            let marker = format!("{} ", &trimmed[..=digits]);
            let m_width = marker.len();
            let rest = &trimmed[digits + 2..];
            let indent_w = width.saturating_sub(nest.len() + m_width);
            let parsed = parse_inline_inner(rest, body, Some(&mut links.borrow_mut()), 0);
            for (w_idx, row_spans) in wrap_spans(parsed, indent_w).into_iter().enumerate() {
                let mut spans = vec![
                    Span::raw(nest.clone()),
                    Span::styled(
                        if w_idx == 0 {
                            marker.clone()
                        } else {
                            " ".repeat(m_width)
                        },
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                ];
                spans.extend(row_spans);
                out.push(Line::from(spans));
            }
            idx += 1;
            continue;
        }

        let cont_pad = lead_cols.min(12);
        let parsed = parse_inline_inner(trimmed, body, Some(&mut links.borrow_mut()), 0);
        for mut row_spans in wrap_spans(parsed, width.saturating_sub(cont_pad)) {
            if cont_pad > 0 {
                row_spans.insert(0, Span::raw(" ".repeat(cont_pad)));
            }
            out.push(Line::from(row_spans));
        }
        idx += 1;
    }
    out.extend(links.borrow_mut().drain_footnotes());

    out
}

fn render_fenced_block(
    lang_raw: &str,
    code_lang: &str,
    block_lines: &[&str],
    width: usize,
) -> Vec<Line<'static>> {
    let joined = block_lines.join("\n");
    if matches!(code_lang, "math" | "latex" | "tex") {
        return super::math_table::render_display_math(&joined, width);
    }
    if matches!(code_lang, "mermaid" | "mmd") {
        return super::math_table::render_mermaid_diagram(&joined, width);
    }
    if code_lang == "svg"
        || (matches!(code_lang, "xml" | "html" | "") && joined.trim_start().starts_with("<svg"))
    {
        return super::math_table::render_svg_summary(&joined, width);
    }

    let bdr = Style::default().fg(HAIRLINE);
    let mut out = Vec::new();
    let label = if lang_raw.is_empty() { "code" } else { lang_raw };
    let total_lines = block_lines.len();
    let meta = if total_lines >= 3 {
        format!(" · {total_lines} lines · Alt+Y copy")
    } else {
        String::new()
    };
    out.push(Line::from(vec![
        Span::styled("╭─ ", bdr),
        Span::styled(
            label.to_owned(),
            Style::default().fg(ACCENT_PEAK).add_modifier(Modifier::BOLD),
        ),
        Span::styled(meta, Style::default().fg(MUTED)),
    ]));

    if code_lang == "diff" || code_lang == "patch" {
        let mut diff_gutter = DiffGutterState::default();
        for &line in block_lines {
            for row in diff_gutter.render_line(line, width.saturating_sub(2)) {
                let mut spans = vec![Span::styled("│ ", bdr)];
                spans.extend(row);
                out.push(Line::from(spans));
            }
        }
    } else {
        let show_nums = total_lines >= 3 && width >= 36;
        let num_w = if show_nums {
            total_lines.to_string().len().max(2)
        } else {
            0
        };
        let code_w = width.saturating_sub(if show_nums { num_w + 4 } else { 2 });
        for (line_idx, &line) in block_lines.iter().enumerate() {
            for (wrap_idx, wrapped) in wrap_plain(line, code_w).into_iter().enumerate() {
                let mut spans = if show_nums {
                    let gutter = if wrap_idx == 0 {
                        format!("{:>w$} │ ", line_idx + 1, w = num_w)
                    } else {
                        format!("{} · ", " ".repeat(num_w))
                    };
                    vec![Span::styled(gutter, bdr)]
                } else {
                    vec![Span::styled("│ ", bdr)]
                };
                spans.extend(highlight_code_line(&wrapped, code_lang));
                out.push(Line::from(spans));
            }
        }
    }
    out.push(Line::from(Span::styled(
        format!("╰{}", "─".repeat(width.min(48).saturating_sub(1))),
        bdr,
    )));
    out
}

fn wrap_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Vec<Span<'static>>> {
    super::math_table::wrap_styled_spans(spans, max_width.max(4))
}

/// Extract all fenced code blocks from markdown text (used by `Alt+Y` copy shortcut).
pub fn extract_code_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_code = false;
    let mut current = Vec::new();
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        if line.trim_start().starts_with("```") {
            if in_code {
                let joined = current.join("\n");
                if !joined.trim().is_empty() {
                    blocks.push(joined);
                }
                current.clear();
            }
            in_code = !in_code;
        } else if in_code {
            current.push(line);
        }
    }
    if in_code && !current.is_empty() {
        let joined = current.join("\n");
        if !joined.trim().is_empty() {
            blocks.push(joined);
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_rounded_code_blocks_hunk_diff_gutter_and_extracts_blocks() {
        let md = "# Title\n- check `foo` and **bar**\n```diff\n@@ -10,2 +10,2 @@\n-old\n+new\n```\n```rust\nlet x = 1;\n```";
        let rendered = render_markdown(md, 52);
        assert!(rendered[0].to_string().contains("Title"));
        assert!(rendered[1].to_string().contains("• check"));
        assert!(rendered.iter().any(|l| l.to_string().contains("diff")));
        assert!(rendered.iter().any(|l| l.to_string().contains("10 - old")));
        assert!(rendered.iter().any(|l| l.to_string().contains("10 + new")));

        let blocks = extract_code_blocks(md);
        assert_eq!(blocks, vec!["@@ -10,2 +10,2 @@\n-old\n+new", "let x = 1;"]);
    }

    #[test]
    fn renders_gfm_tables_and_latex_math_while_preserving_currency_and_skills() {
        let md = "价格 $100 与技能 $qws-zoom-out 保留，公式 $v_0 = \\frac{d}{t}$ 渲染：\n\n| 日期 | 天气 | 温度 |\n|---|---|---:|\n| 11-27 | 晴 | 19°C |\n\n$$\nE = mc^2\n$$";
        let rendered = render_markdown(md, 64);
        let text: String = rendered
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("$100"), "currency kept: {text}");
        assert!(text.contains("$qws-zoom-out"), "skill kept: {text}");
        assert!(text.contains("v₀ = d/t"), "inline latex rendered: {text}");
        assert!(!text.contains("\\frac"), "latex command converted: {text}");
        assert!(text.contains('┌') && text.contains("11-27"), "gfm box table: {text}");
        assert!(text.contains("E = mc²"), "display math rendered: {text}");

        let rich = "## 标题 [规范](https://ui-sdk.dev/docs/agents)\n> 引用 [源码](/workspace/twig/src/tui/markdown.rs:814)\n\n**重点核对 `H-01` 的 [市场判断](https://example.com/post/12345)**\n\n- 一级要点\n  - 二级要点 [web.rs](file:///home/u/src/web.rs#L10-L20)\n    - 三级要点";
        let rich_out: String = render_markdown(rich, 80)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rich_out.contains("标题 规范 [1]")
                && rich_out.contains("  [1] https://ui-sdk.dev/docs/agents")
        );
        assert!(
            rich_out.contains("▎ 引用 源码 [2]")
                && rich_out.contains("  [2] /workspace/twig/src/tui/markdown.rs:814")
        );
        assert!(
            rich_out.contains("市场判断 [3]")
                && rich_out.contains("  [3] https://example.com/post/12345")
        );
        assert!(rich_out.contains(" H-01 ") && rich_out.contains("web.rs (#L10-L20)"));
        assert!(
            rich_out.contains("• 一级要点")
                && rich_out.contains("  ◦ 二级要点")
                && rich_out.contains("    ▪ 三级要点")
        );
    }
}
