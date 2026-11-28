//! Unified diff gutter, intra-line word diffing, warm syntax highlighting,
//! and Unicode case-folding transcript search highlights.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::{
    display_width, wrap_plain, ACCENT, ACCENT_PEAK, AMBER, DARK_TC_ADD_LINE_BG,
    DARK_TC_ADD_WORD_BG, DARK_TC_DEL_LINE_BG, DARK_TC_DEL_WORD_BG, IVORY, MUTED, ON_ACCENT_FG,
    ROSE, SAGE, SEARCH_ACTIVE_BG, SEARCH_BG, STONE,
};

/// Tracks hunk line numbers for unified diff rendering (inspired by Codex `diff_render.rs`).
#[derive(Default)]
pub struct DiffGutterState {
    old_line: Option<usize>,
    new_line: Option<usize>,
}

impl DiffGutterState {
    pub fn render_line(&mut self, raw: &str, max_width: usize) -> Vec<Vec<Span<'static>>> {
        self.render_line_paired(raw, None, max_width)
    }

    /// Render a slice of unified-diff lines, pairing consecutive `-` and `+` blocks
    /// so changed tokens within modified lines get crisp Claude-style intra-line highlights.
    pub fn render_block(&mut self, lines: &[&str], max_width: usize) -> Vec<Vec<Span<'static>>> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < lines.len() {
            if lines[i].starts_with('-') && !lines[i].starts_with("--- ") {
                let del_start = i;
                while i < lines.len() && lines[i].starts_with('-') && !lines[i].starts_with("--- ") {
                    i += 1;
                }
                let del_end = i;
                let add_start = i;
                while i < lines.len() && lines[i].starts_with('+') && !lines[i].starts_with("+++ ") {
                    i += 1;
                }
                let add_end = i;
                let del_slice = &lines[del_start..del_end];
                let add_slice = &lines[add_start..add_end];
                let paired = del_slice.len().min(add_slice.len());
                for (k, &dl) in del_slice.iter().enumerate() {
                    let peer = (k < paired).then(|| &add_slice[k][1..]);
                    out.extend(self.render_line_paired(dl, peer, max_width));
                }
                for (k, &al) in add_slice.iter().enumerate() {
                    let peer = (k < paired).then(|| &del_slice[k][1..]);
                    out.extend(self.render_line_paired(al, peer, max_width));
                }
            } else {
                out.extend(self.render_line_paired(lines[i], None, max_width));
                i += 1;
            }
        }
        out
    }

    fn render_line_paired(
        &mut self,
        raw: &str,
        peer_body: Option<&str>,
        max_width: usize,
    ) -> Vec<Vec<Span<'static>>> {
        if let Some((old_n, new_n)) = parse_hunk_header(raw) {
            self.old_line = Some(old_n);
            self.new_line = Some(new_n);
            return wrap_plain(raw, max_width)
                .into_iter()
                .map(|w| vec![Span::styled(w, Style::default().fg(AMBER))])
                .collect();
        }
        if raw.starts_with("diff --git ") || raw.starts_with("+++ ") || raw.starts_with("--- ") {
            self.old_line = None;
            self.new_line = None;
            return wrap_plain(raw, max_width)
                .into_iter()
                .map(|w| {
                    vec![Span::styled(
                        w,
                        Style::default().fg(STONE).add_modifier(Modifier::BOLD),
                    )]
                })
                .collect();
        }

        let in_hunk = self.old_line.is_some() && self.new_line.is_some();
        let (gutter_num, sign, body, line_style, word_bg) = if let Some(rest) = raw.strip_prefix('+') {
            let num = self.new_line.inspect(|&n| {
                self.new_line = Some(n + 1);
            });
            (
                num,
                Some('+'),
                rest,
                Style::default().fg(SAGE).bg(DARK_TC_ADD_LINE_BG),
                Some(DARK_TC_ADD_WORD_BG),
            )
        } else if let Some(rest) = raw.strip_prefix('-') {
            let num = self.old_line.inspect(|&n| {
                self.old_line = Some(n + 1);
            });
            (
                num,
                Some('-'),
                rest,
                Style::default().fg(ROSE).bg(DARK_TC_DEL_LINE_BG),
                Some(DARK_TC_DEL_WORD_BG),
            )
        } else if in_hunk && raw.starts_with(' ') {
            let num = self.new_line.inspect(|&n| {
                self.new_line = Some(n + 1);
            });
            if let Some(o) = self.old_line {
                self.old_line = Some(o + 1);
            }
            (num, Some(' '), &raw[1..], Style::default().fg(STONE), None)
        } else {
            return wrap_plain(raw, max_width)
                .into_iter()
                .map(|w| vec![Span::styled(w, Style::default().fg(STONE))])
                .collect();
        };

        let show_gutter = gutter_num.is_some() && max_width >= 44;
        let prefix_w = if show_gutter { 7 } else { 2 };
        let content_w = max_width.saturating_sub(prefix_w).max(8);
        let sign_ch = sign.unwrap_or(' ');
        let gutter_bg = line_style.bg.unwrap_or(Color::Reset);
        let gutter_style = Style::default().fg(MUTED).bg(gutter_bg);

        if let (Some(peer), Some(hi_bg)) = (peer_body, word_bg) {
            if display_width(body) <= content_w {
                let (pre, mid, suf) = split_intraline_change(body, peer);
                if !mid.is_empty() && (!pre.is_empty() || !suf.is_empty()) {
                    let mut spans = Vec::with_capacity(6);
                    if show_gutter {
                        if let Some(n) = gutter_num {
                            spans.push(Span::styled(format!("{n:>4} "), gutter_style));
                        }
                    }
                    spans.push(Span::styled(format!("{sign_ch} "), line_style));
                    if !pre.is_empty() {
                        spans.push(Span::styled(pre.to_owned(), line_style));
                    }
                    spans.push(Span::styled(
                        mid.to_owned(),
                        Style::default()
                            .fg(IVORY)
                            .bg(hi_bg)
                            .add_modifier(Modifier::BOLD),
                    ));
                    let pad = content_w.saturating_sub(display_width(body));
                    spans.push(Span::styled(
                        format!("{suf}{}", " ".repeat(pad)),
                        line_style,
                    ));
                    return vec![spans];
                }
            }
        }

        wrap_plain(body, content_w)
            .into_iter()
            .enumerate()
            .map(|(idx, segment)| {
                let mut spans = Vec::with_capacity(3);
                if show_gutter {
                    if let Some(n) = gutter_num {
                        let num_str = if idx == 0 {
                            format!("{n:>4} ")
                        } else {
                            "     ".to_owned()
                        };
                        spans.push(Span::styled(num_str, gutter_style));
                    }
                }
                spans.push(Span::styled(
                    if idx == 0 {
                        format!("{sign_ch} ")
                    } else {
                        "  ".to_owned()
                    },
                    line_style,
                ));
                let pad = content_w.saturating_sub(display_width(&segment));
                spans.push(Span::styled(
                    if line_style.bg.is_some() {
                        format!("{segment}{}", " ".repeat(pad))
                    } else {
                        segment
                    },
                    line_style,
                ));
                spans
            })
            .collect()
    }
}

fn split_intraline_change<'a>(line: &'a str, peer: &str) -> (&'a str, &'a str, &'a str) {
    let lc: Vec<(usize, char)> = line.char_indices().collect();
    let pc: Vec<char> = peer.chars().collect();
    let mut pre_n = 0usize;
    while pre_n < lc.len() && pre_n < pc.len() && lc[pre_n].1 == pc[pre_n] {
        pre_n += 1;
    }
    let mut suf_n = 0usize;
    while pre_n + suf_n < lc.len()
        && pre_n + suf_n < pc.len()
        && lc[lc.len() - 1 - suf_n].1 == pc[pc.len() - 1 - suf_n]
    {
        suf_n += 1;
    }
    let start_b = lc.get(pre_n).map_or(line.len(), |&(b, _)| b);
    let end_b = if suf_n == 0 {
        line.len()
    } else {
        lc[lc.len() - suf_n].0
    };
    (&line[..start_b], &line[start_b..end_b], &line[end_b..])
}

fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_part, after_old) = rest.split_once(" +")?;
    let new_part = after_old.split_once(" @@")?.0;
    let old_start = old_part.split(',').next()?.parse().ok()?;
    let new_start = new_part.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

/// Style a single compact preview line with unified-diff colorization.
pub fn style_output_line(line: &str) -> Style {
    if line.starts_with("+++ ") || line.starts_with("--- ") || line.starts_with("diff --git ") {
        Style::default().fg(STONE).add_modifier(Modifier::BOLD)
    } else if line.starts_with("@@ ") {
        Style::default().fg(AMBER)
    } else if line.starts_with('+') {
        Style::default().fg(SAGE).bg(DARK_TC_ADD_LINE_BG)
    } else if line.starts_with('-') {
        Style::default().fg(ROSE).bg(DARK_TC_DEL_LINE_BG)
    } else {
        Style::default().fg(STONE)
    }
}

/// Zero-dependency warm syntax highlighter for Rust/Python/TS/JS/Go/Bash/JSON/TOML.
pub fn highlight_code_line(line: &str, lang: &str) -> Vec<Span<'static>> {
    let hash_comments = matches!(
        lang,
        "py" | "python" | "sh" | "bash" | "zsh" | "shell" | "toml" | "yaml" | "yml"
    );
    let slash_comments = !hash_comments && lang != "json";
    let kw_style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let str_style = Style::default().fg(SAGE);
    let num_style = Style::default().fg(AMBER);
    let cmt_style = Style::default().fg(MUTED);
    let base_style = Style::default().fg(IVORY);
    let type_style = Style::default().fg(ACCENT_PEAK);

    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut plain = String::new();
    let flush = |plain: &mut String, out: &mut Vec<Span<'static>>| {
        if !plain.is_empty() {
            out.push(Span::styled(std::mem::take(plain), base_style));
        }
    };

    while i < chars.len() {
        let ch = chars[i];
        if (slash_comments && ch == '/' && chars.get(i + 1) == Some(&'/'))
            || (hash_comments && ch == '#')
        {
            flush(&mut plain, &mut out);
            let rest: String = chars[i..].iter().collect();
            out.push(Span::styled(rest, cmt_style));
            break;
        }
        if ch == '"' || (ch == '\'' && lang != "rust" && lang != "rs") {
            flush(&mut plain, &mut out);
            let quote = ch;
            let start = i;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                if chars[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let lit: String = chars[start..i].iter().collect();
            out.push(Span::styled(lit, str_style));
            continue;
        }
        if ch.is_ascii_digit()
            && (i == 0 || !chars[i - 1].is_ascii_alphanumeric() && chars[i - 1] != '_')
        {
            flush(&mut plain, &mut out);
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '.' | '_'))
            {
                i += 1;
            }
            let num: String = chars[start..i].iter().collect();
            out.push(Span::styled(num, num_style));
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if is_keyword(&word) {
                flush(&mut plain, &mut out);
                out.push(Span::styled(word, kw_style));
            } else if word.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && word.chars().any(|c| c.is_ascii_lowercase())
            {
                flush(&mut plain, &mut out);
                out.push(Span::styled(word, type_style));
            } else {
                plain.push_str(&word);
            }
            continue;
        }
        plain.push(ch);
        i += 1;
    }
    flush(&mut plain, &mut out);
    if out.is_empty() {
        out.push(Span::styled(String::new(), base_style));
    }
    out
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "fn" | "let"
            | "mut"
            | "pub"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "use"
            | "mod"
            | "match"
            | "if"
            | "else"
            | "for"
            | "while"
            | "loop"
            | "return"
            | "break"
            | "continue"
            | "async"
            | "await"
            | "const"
            | "static"
            | "type"
            | "where"
            | "self"
            | "Self"
            | "true"
            | "false"
            | "None"
            | "Some"
            | "Ok"
            | "Err"
            | "def"
            | "class"
            | "import"
            | "from"
            | "as"
            | "with"
            | "try"
            | "except"
            | "raise"
            | "lambda"
            | "yield"
            | "pass"
            | "function"
            | "export"
            | "default"
            | "interface"
            | "var"
            | "new"
            | "null"
            | "undefined"
            | "func"
            | "package"
            | "defer"
            | "go"
            | "select"
            | "case"
            | "then"
            | "fi"
            | "do"
            | "done"
            | "local"
    )
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
            .fg(ON_ACCENT_FG)
            .bg(SEARCH_ACTIVE_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(IVORY).bg(SEARCH_BG)
    };
    let mut new_spans = Vec::new();
    for span in line.spans {
        let content = span.content.as_ref();
        let mut lower = String::with_capacity(content.len());
        let mut char_map: Vec<(usize, usize, usize, usize)> = Vec::new();
        for (orig_start, ch) in content.char_indices() {
            let orig_end = orig_start + ch.len_utf8();
            let low_start = lower.len();
            for lc in ch.to_lowercase() {
                lower.push(lc);
            }
            let low_end = lower.len();
            char_map.push((low_start, low_end, orig_start, orig_end));
        }
        let mut low_cursor = 0usize;
        let mut orig_cursor = 0usize;
        let mut matched_any = false;
        while let Some(rel) = lower[low_cursor..].find(&needle) {
            let abs_low = low_cursor + rel;
            let end_low = abs_low + needle.len();
            let Some(first_idx) = char_map
                .iter()
                .position(|&(ls, le, _, _)| abs_low >= ls && abs_low < le)
            else {
                break;
            };
            let Some(last_idx) = char_map[first_idx..]
                .iter()
                .position(|&(_, le, _, _)| end_low <= le)
                .map(|off| first_idx + off)
            else {
                break;
            };
            let orig_abs = char_map[first_idx].2;
            let orig_end = char_map[last_idx].3;
            let next_low = char_map[last_idx].1;
            if orig_abs > orig_cursor {
                new_spans.push(Span::styled(
                    content[orig_cursor..orig_abs].to_owned(),
                    span.style,
                ));
            }
            new_spans.push(Span::styled(content[orig_abs..orig_end].to_owned(), hi_style));
            orig_cursor = orig_end;
            low_cursor = next_low.max(abs_low + 1);
            matched_any = true;
        }
        if !matched_any {
            new_spans.push(span);
        } else if orig_cursor < content.len() {
            new_spans.push(Span::styled(content[orig_cursor..].to_owned(), span.style));
        }
    }
    Line::from(new_spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_line_handles_case_folding_length_changes_and_cjk() {
        let line = Line::from("İtest! Kelvin 中文搜索");
        let hi_test = highlight_search(line.clone(), "test", true);
        assert_eq!(hi_test.to_string(), "İtest! Kelvin 中文搜索");
        assert!(hi_test.spans.iter().any(|s| s.content.as_ref() == "test"));

        let hi_kelvin = highlight_search(line.clone(), "kelvin", false);
        assert_eq!(hi_kelvin.to_string(), "İtest! Kelvin 中文搜索");
        assert!(hi_kelvin.spans.iter().any(|s| s.content.as_ref() == "Kelvin"));

        let hi_cjk = highlight_search(line, "搜索", true);
        assert!(hi_cjk.spans.iter().any(|s| s.content.as_ref() == "搜索"));
    }
}
