//! Stateless warm-terracotta rendering primitives for `twig chat`:
//! - Claude Code warm palette tokens
//! - Codex-style TrueColor cosine shimmer wave
//! - CJK/emoji display-width wrapping
//! - Markdown + unified diff highlighting + fenced code block extraction
//! - Transcript search highlighting

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Claude Code inspired warm terracotta minimalist palette.
pub const ACCENT: Color = Color::Rgb(217, 119, 87); // #D97757 terracotta
pub const ACCENT_PEAK: Color = Color::Rgb(246, 190, 158); // #F6BE9E shimmer highlight
pub const AMBER: Color = Color::Rgb(230, 173, 95); // #E6AD5F warn / steer / approval
pub const SAGE: Color = Color::Rgb(126, 189, 134); // #7EBD86 success / diff +
pub const ROSE: Color = Color::Rgb(224, 108, 117); // #E06C75 error / diff -
pub const IVORY: Color = Color::Rgb(236, 232, 224); // #ECE8E0 primary text
pub const STONE: Color = Color::Rgb(166, 158, 148); // #A69E94 secondary text
pub const MUTED: Color = Color::Rgb(112, 106, 99); // #706A63 hints / dim connectors
pub const HAIRLINE: Color = Color::Rgb(74, 70, 66); // #4A4642 hairline horizontal rules
pub const SURFACE: Color = Color::Rgb(38, 35, 32); // #262320 warm subtle surface
pub const SURFACE_HI: Color = Color::Rgb(54, 48, 43); // #36302B selected popup/option row
pub const DIFF_ADD_BG: Color = Color::Rgb(26, 46, 33); // subtle sage tint
pub const DIFF_DEL_BG: Color = Color::Rgb(54, 27, 30); // subtle rose tint
pub const SEARCH_BG: Color = Color::Rgb(92, 66, 32); // search match highlight
pub const SEARCH_ACTIVE_BG: Color = Color::Rgb(217, 119, 87);

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Wrap plain text by terminal cell width (CJK/emoji safe), preserving explicit newlines.
pub fn wrap_plain(text: &str, max_width: usize) -> Vec<String> {
    let width = max_width.max(8);
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for ch in line.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + w > width && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            cur.push(ch);
            cur_w += w;
        }
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Codex-style TrueColor cosine shimmer sweep across `text` in warm terracotta tones.
pub fn shimmer_spans(text: &str, phase: f32) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let span_len = chars.len() as f32;
    let center = (phase.rem_euclid(1.4) - 0.2) * (span_len + 6.0) - 3.0;
    let band = 4.2f32;

    chars
        .into_iter()
        .enumerate()
        .map(|(i, ch)| {
            let dist = ((i as f32) - center).abs();
            let t = if dist < band {
                0.5 * (1.0 + (std::f32::consts::PI * dist / band).cos())
            } else {
                0.0
            };
            // Base warm terracotta-stone blend -> luminous cream peak
            let r = lerp_u8(178, 248, t);
            let g = lerp_u8(126, 208, t);
            let b = lerp_u8(104, 178, t);
            let mut style = Style::default().fg(Color::Rgb(r, g, b));
            if t > 0.65 {
                style = style.add_modifier(Modifier::BOLD);
            }
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let clamped = t.clamp(0.0, 1.0);
    (a as f32 + (b as f32 - a as f32) * clamped).round() as u8
}

/// Style a single tool-output or diff line with automatic unified-diff colorization.
pub fn style_output_line(line: &str) -> Style {
    if line.starts_with("+++ ") || line.starts_with("--- ") || line.starts_with("diff --git ") {
        Style::default().fg(STONE).add_modifier(Modifier::BOLD)
    } else if line.starts_with("@@ ") {
        Style::default().fg(AMBER)
    } else if line.starts_with('+') {
        Style::default().fg(SAGE).bg(DIFF_ADD_BG)
    } else if line.starts_with('-') {
        Style::default().fg(ROSE).bg(DIFF_DEL_BG)
    } else {
        Style::default().fg(STONE)
    }
}

/// Parse inline `code` and **bold** spans within one already-wrapped line.
fn parse_inline(text: &str, base: Style) -> Vec<Span<'static>> {
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
                    Style::default().fg(ACCENT_PEAK).bg(SURFACE),
                ));
                i += rel_end + 2;
                continue;
            }
        }
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
                spans.push(Span::styled(
                    bold,
                    base.fg(IVORY).add_modifier(Modifier::BOLD),
                ));
                i = end_idx + 2;
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

/// Render streaming assistant markdown into wrapped styled Ratatui lines.
/// Applies Claude Code warm terracotta typography and unified-diff highlighting.
pub fn render_markdown(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let width = max_width.max(12);
    let mut out = Vec::new();
    let mut in_code = false;
    let mut is_diff_block = false;
    let body = Style::default().fg(IVORY);

    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim_start();

        if let Some(fence) = trimmed.strip_prefix("```") {
            in_code = !in_code;
            if in_code {
                let lang = fence.trim();
                is_diff_block = lang.eq_ignore_ascii_case("diff") || lang.eq_ignore_ascii_case("patch");
                let label = if lang.is_empty() {
                    "─── code ".to_string()
                } else {
                    format!("─── {lang} ")
                };
                let rule_len = width.saturating_sub(display_width(&label)).min(48);
                let header = format!("{label}{}", "─".repeat(rule_len));
                out.push(Line::from(Span::styled(header, Style::default().fg(HAIRLINE))));
            } else {
                is_diff_block = false;
                let rule_len = width.min(56);
                out.push(Line::from(Span::styled(
                    "─".repeat(rule_len),
                    Style::default().fg(HAIRLINE),
                )));
            }
            continue;
        }

        if in_code {
            let line_style = if is_diff_block {
                style_output_line(line)
            } else {
                Style::default().fg(ACCENT_PEAK)
            };
            for wrapped in wrap_plain(line, width.saturating_sub(2)) {
                out.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(HAIRLINE)),
                    Span::styled(wrapped, line_style),
                ]));
            }
            continue;
        }

        if trimmed.is_empty() {
            out.push(Line::from(""));
            continue;
        }

        if let Some(rest) = trimmed
            .strip_prefix("### ")
            .or_else(|| trimmed.strip_prefix("## "))
            .or_else(|| trimmed.strip_prefix("# "))
        {
            let heading = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
            for wrapped in wrap_plain(rest, width) {
                out.push(Line::from(Span::styled(wrapped, heading)));
            }
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("> ") {
            for wrapped in wrap_plain(rest, width.saturating_sub(2)) {
                out.push(Line::from(vec![
                    Span::styled("▎ ", Style::default().fg(ACCENT)),
                    Span::styled(wrapped, Style::default().fg(STONE)),
                ]));
            }
            continue;
        }

        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let indent_width = width.saturating_sub(2);
            for (idx, wrapped) in wrap_plain(rest, indent_width).into_iter().enumerate() {
                let mut spans = vec![Span::styled(
                    if idx == 0 { "• " } else { "  " },
                    Style::default().fg(ACCENT),
                )];
                spans.extend(parse_inline(&wrapped, body));
                out.push(Line::from(spans));
            }
            continue;
        }

        for wrapped in wrap_plain(line, width) {
            out.push(Line::from(parse_inline(&wrapped, body)));
        }
    }

    out
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

/// Highlight case-insensitive occurrences of `query` inside a rendered line.
pub fn highlight_search(line: Line<'static>, query: &str, active: bool) -> Line<'static> {
    if query.is_empty() {
        return line;
    }
    let needle = query.to_lowercase();
    let plain = line.to_string();
    if !plain.to_lowercase().contains(&needle) {
        return line;
    }
    let hi_style = if active {
        Style::default()
            .fg(Color::Rgb(20, 18, 16))
            .bg(SEARCH_ACTIVE_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(IVORY).bg(SEARCH_BG)
    };
    let mut new_spans = Vec::new();
    for span in line.spans {
        let content = span.content.as_ref();
        let lower = content.to_lowercase();
        let mut start = 0usize;
        let mut matched_any = false;
        while let Some(rel) = lower[start..].find(&needle) {
            let abs = start + rel;
            let end = abs + needle.len();
            if !content.is_char_boundary(abs) || !content.is_char_boundary(end) {
                break;
            }
            if abs > start {
                new_spans.push(Span::styled(content[start..abs].to_owned(), span.style));
            }
            new_spans.push(Span::styled(content[abs..end].to_owned(), hi_style));
            start = end;
            matched_any = true;
        }
        if !matched_any {
            new_spans.push(span);
        } else if start < content.len() {
            new_spans.push(Span::styled(content[start..].to_owned(), span.style));
        }
    }
    Line::from(new_spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_cjk_by_display_width() {
        let lines = wrap_plain("你好世界测试", 8);
        assert_eq!(lines, vec!["你好世界", "测试"]);
        assert_eq!(display_width("你好"), 4);
    }

    #[test]
    fn renders_code_blocks_diffs_and_extracts_blocks() {
        let md = "# Title\n- check `foo` and **bar**\n```diff\n-old\n+new\n```\n```rust\nlet x = 1;\n```";
        let rendered = render_markdown(md, 48);
        assert!(rendered[0].to_string().contains("Title"));
        assert!(rendered[1].to_string().contains("• check"));
        assert!(rendered.iter().any(|l| l.to_string().contains("+new")));

        let blocks = extract_code_blocks(md);
        assert_eq!(blocks, vec!["-old\n+new", "let x = 1;"]);
    }

    #[test]
    fn shimmer_produces_one_span_per_char() {
        let spans = shimmer_spans("Reasoning…", 0.5);
        assert_eq!(spans.len(), "Reasoning…".chars().count());
    }
}
