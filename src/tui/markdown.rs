//! Stateless Markdown and CJK-aware display-width wrapping for the Ratatui TUI.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
                    code,
                    Style::default().fg(Color::Cyan).bg(Color::Rgb(30, 34, 42)),
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
                spans.push(Span::styled(bold, base.add_modifier(Modifier::BOLD)));
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
pub fn render_markdown(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let width = max_width.max(12);
    let mut out = Vec::new();
    let mut in_code = false;

    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim_start();

        if let Some(fence) = trimmed.strip_prefix("```") {
            in_code = !in_code;
            if in_code {
                let lang = fence.trim();
                let header = if lang.is_empty() {
                    "─── code ───".to_string()
                } else {
                    format!("─── {lang} ───")
                };
                out.push(Line::from(Span::styled(
                    header,
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                out.push(Line::from(Span::styled(
                    "────────────",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            continue;
        }

        if in_code {
            let code_style = Style::default().fg(Color::Cyan);
            for wrapped in wrap_plain(line, width.saturating_sub(2)) {
                out.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(wrapped, code_style),
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
            let heading = Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD);
            for wrapped in wrap_plain(rest, width) {
                out.push(Line::from(Span::styled(wrapped, heading)));
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
                    Style::default().fg(Color::Yellow),
                )];
                spans.extend(parse_inline(&wrapped, Style::default()));
                out.push(Line::from(spans));
            }
            continue;
        }

        for wrapped in wrap_plain(line, width) {
            out.push(Line::from(parse_inline(&wrapped, Style::default())));
        }
    }

    out
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
    fn renders_code_blocks_and_inline_styles() {
        let rendered = render_markdown("# Title\n- check `foo` and **bar**\n```rust\nlet x = 1;\n```", 40);
        assert!(rendered.len() >= 5);
        assert!(rendered[0].to_string().contains("Title"));
        assert!(rendered[1].to_string().contains("• check foo and bar"));
        assert!(rendered.iter().any(|l| l.to_string().contains("let x = 1;")));
    }
}
