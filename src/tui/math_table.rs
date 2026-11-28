//! Zero-dependency GFM pipe-table layout engine and Unicode LaTeX mathematical
//! pretty-printer for terminal TUI rendering (`$...$`, `$$...$$`, `\(...\)`, `\[...\]`).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme::{ACCENT, ACCENT_PEAK, AMBER, HAIRLINE, IVORY, MUTED, SAGE};

#[derive(Default)]
pub(super) struct LinkRegistry {
    urls: Vec<String>,
    flushed: usize,
}

impl LinkRegistry {
    pub(super) fn register(&mut self, url: &str) -> usize {
        if let Some(pos) = self.urls.iter().position(|u| u == url) {
            pos + 1
        } else {
            self.urls.push(url.to_owned());
            self.urls.len()
        }
    }

    pub(super) fn drain_footnotes(&mut self) -> Vec<Line<'static>> {
        if self.flushed >= self.urls.len() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.urls.len() - self.flushed);
        while self.flushed < self.urls.len() {
            let idx = self.flushed + 1;
            let url = self.urls[self.flushed].clone();
            self.flushed += 1;
            let shown = format_footnote_target(&url);
            out.push(Line::from(vec![
                Span::styled(format!("  [{idx}] "), Style::default().fg(ACCENT)),
                Span::styled(shown, Style::default().fg(MUTED).add_modifier(Modifier::UNDERLINED)),
            ]));
        }
        out
    }

    pub(super) fn into_urls(self) -> Vec<String> {
        self.urls
    }
}

fn format_footnote_target(url: &str) -> String {
    if let Some(local) = url.strip_prefix("file://") {
        if let Some((path_part, anchor)) = local.split_once('#') {
            let line = anchor
                .trim_start_matches(['L', 'l'])
                .split('-')
                .next()
                .unwrap_or("");
            if !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()) {
                return format!("{path_part}:{line}");
            }
        }
        return local.to_owned();
    }
    url.to_owned()
}

pub(super) fn format_link_suffix(
    label: &str,
    url: &str,
    links: Option<&mut LinkRegistry>,
) -> Option<Span<'static>> {
    if url.is_empty() || url == label || url.starts_with('#') {
        return None;
    }
    if let Some(local) = url.strip_prefix("file://") {
        let (path_part, anchor) = match local.split_once('#') {
            Some((p, a)) => (p, Some(a)),
            None => (local, None),
        };
        let base = path_part.rsplit('/').next().unwrap_or(path_part);
        if let Some(reg) = links {
            let idx = reg.register(url);
            let extra = match anchor {
                Some(a) if !label.contains('#') && !label.contains(':') => format!(" (#{a}) [{idx}]"),
                _ if label.contains(base) => format!(" [{idx}]"),
                Some(a) => format!(" ({base}#{a}) [{idx}]"),
                None => format!(" ({base}) [{idx}]"),
            };
            return Some(Span::styled(extra, Style::default().fg(ACCENT)));
        }
        if label.contains(base) {
            return anchor.map(|a| Span::styled(format!(" (#{a})"), Style::default().fg(MUTED)));
        }
        let short = match anchor {
            Some(a) => format!(" ({base}#{a})"),
            None => format!(" ({base})"),
        };
        return Some(Span::styled(short, Style::default().fg(MUTED)));
    }
    if !url.starts_with("http://") && !url.starts_with("https://") && url.chars().count() <= 28 {
        return Some(Span::styled(
            format!(" ({url})"),
            Style::default().fg(MUTED),
        ));
    }
    if let Some(reg) = links {
        let idx = reg.register(url);
        Some(Span::styled(
            format!(" [{idx}]"),
            Style::default().fg(ACCENT),
        ))
    } else {
        Some(Span::styled(
            format!(" ({url})"),
            Style::default().fg(MUTED),
        ))
    }
}

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

fn normalize_cell_breaks(raw: &str) -> String {
    raw.replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("<BR/>", "\n")
        .replace("<BR>", "\n")
        .replace("<br>", "\n")
}

pub(super) fn wrap_styled_spans(spans: Vec<Span<'static>>, max_w: usize) -> Vec<Vec<Span<'static>>> {
    let limit = max_w.max(2);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur_line: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    for span in spans {
        let style = span.style;
        let mut seg = String::new();
        let mut chars = span.content.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch.is_ascii() && !ch.is_ascii_whitespace() {
                let mut word = String::from(ch);
                while let Some(&nx) = chars.peek() {
                    if nx.is_ascii() && !nx.is_ascii_whitespace() {
                        word.push(nx);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let word_w = display_width(&word);
                if cur_w > 0 && cur_w + word_w > limit && word_w <= limit {
                    if !seg.is_empty() {
                        cur_line.push(Span::styled(std::mem::take(&mut seg), style));
                    }
                    lines.push(std::mem::take(&mut cur_line));
                    cur_w = 0;
                }
                for wch in word.chars() {
                    let w = UnicodeWidthChar::width(wch).unwrap_or(0);
                    if cur_w + w > limit && (cur_w > 0 || !seg.is_empty()) {
                        if !seg.is_empty() {
                            cur_line.push(Span::styled(std::mem::take(&mut seg), style));
                        }
                        lines.push(std::mem::take(&mut cur_line));
                        cur_w = 0;
                    }
                    seg.push(wch);
                    cur_w += w;
                }
                continue;
            }
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + w > limit && (cur_w > 0 || !seg.is_empty()) {
                if !seg.is_empty() {
                    cur_line.push(Span::styled(std::mem::take(&mut seg), style));
                }
                lines.push(std::mem::take(&mut cur_line));
                cur_w = 0;
                if ch == ' ' {
                    continue;
                }
            }
            seg.push(ch);
            cur_w += w;
        }
        if !seg.is_empty() {
            cur_line.push(Span::styled(seg, style));
        }
    }
    if !cur_line.is_empty() || lines.is_empty() {
        lines.push(cur_line);
    }
    lines
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

/// Render consecutive GFM pipe-table lines into a Claude-CLI-style multi-line CJK-aligned
/// Unicode box table (with automatic stacked vertical cards when columns exceed narrow viewports),
/// never truncating cell prose with `…`.
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
    let visible_max_line_width = |raw: &str| -> usize {
        normalize_cell_breaks(raw)
            .split('\n')
            .map(|seg| {
                inline_fn(seg.trim(), Style::default().fg(IVORY))
                    .into_iter()
                    .map(|sp| display_width(sp.content.as_ref()))
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0)
    };
    let mut header_cells = split_table_row(raw_rows[0]);
    let sep_cells = split_table_row(raw_rows[1]);
    let cols = header_cells.len().max(sep_cells.len());
    if cols == 0 {
        return None;
    }
    header_cells.resize(cols, String::new());

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

    let mut natural_w: Vec<usize> = (0..cols)
        .map(|c| visible_max_line_width(&header_cells[c]).max(2))
        .collect();
    for r in &data_rows {
        for (c, cell) in r.iter().enumerate().take(cols) {
            natural_w[c] = natural_w[c].max(visible_max_line_width(cell));
        }
    }

    // Short identifier columns (`ID`, status codes <= 8 cols) keep their exact width;
    // prose columns get at least 10 display cols (5 CJK chars) before falling back to stacked cards.
    let min_w: Vec<usize> = natural_w
        .iter()
        .map(|&nw| if nw <= 8 { nw } else { 10 })
        .collect();
    let border_overhead = cols * 3 + 1;
    let avail = max_width.saturating_sub(border_overhead);
    let min_sum: usize = min_w.iter().sum();

    let border_style = Style::default().fg(HAIRLINE);
    let hdr_style = Style::default()
        .fg(ACCENT_PEAK)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(IVORY);

    // Narrow viewport / ultra-wide column count fallback: render zero-loss stacked vertical cards.
    if cols >= 3 && avail < min_sum {
        let card_inner = max_width.saturating_sub(4).max(8);
        let top_rule = format!("╭{}╮", "─".repeat(card_inner + 2));
        let mid_rule = format!("├{}┤", "─".repeat(card_inner + 2));
        let bot_rule = format!("╰{}╯", "─".repeat(card_inner + 2));
        let mut out = vec![Line::from(Span::styled(top_rule, border_style))];
        for (r_idx, row) in data_rows.iter().enumerate() {
            if r_idx > 0 {
                out.push(Line::from(Span::styled(mid_rule.clone(), border_style)));
            }
            for c in 0..cols {
                let raw_val = row[c].trim();
                if raw_val.is_empty() {
                    continue;
                }
                let h_label = normalize_cell_breaks(&header_cells[c]).replace('\n', " ");
                let prefix = format!("{}: ", h_label.trim());
                let mut combined = vec![Span::styled(prefix, hdr_style)];
                for (seg_i, seg) in normalize_cell_breaks(raw_val).split('\n').enumerate() {
                    if seg_i > 0 {
                        combined.push(Span::styled(" / ".to_owned(), Style::default().fg(MUTED)));
                    }
                    combined.extend(inline_fn(seg.trim(), body_style));
                }
                for wrapped in wrap_styled_spans(combined, card_inner) {
                    let used: usize = wrapped
                        .iter()
                        .map(|sp| display_width(sp.content.as_ref()))
                        .sum();
                    let rpad = card_inner.saturating_sub(used);
                    let mut line_spans = vec![Span::styled("│ ", border_style)];
                    line_spans.extend(wrapped);
                    line_spans.push(Span::raw(format!("{} ", " ".repeat(rpad))));
                    line_spans.push(Span::styled("│", border_style));
                    out.push(Line::from(line_spans));
                }
            }
        }
        out.push(Line::from(Span::styled(bot_rule, border_style)));
        return Some(out);
    }

    // Proportional multi-line column width allocation (`sqrt`-weighted above `min_w`).
    let nat_sum: usize = natural_w.iter().sum();
    let mut col_w = if nat_sum <= avail {
        natural_w.clone()
    } else if avail <= cols * 2 {
        vec![2; cols]
    } else {
        let mut w = min_w
            .iter()
            .map(|&mw| mw.min((avail / cols).max(2)))
            .collect::<Vec<_>>();
        let mut used: usize = w.iter().sum();
        if used < min_sum && min_sum <= avail {
            w = min_w.clone();
            used = min_sum;
        }
        let rem = avail.saturating_sub(used);
        if rem > 0 {
            let weights: Vec<f64> = (0..cols)
                .map(|c| {
                    if natural_w[c] > w[c] {
                        ((natural_w[c] - w[c]) as f64).sqrt()
                    } else {
                        0.0
                    }
                })
                .collect();
            let w_sum: f64 = weights.iter().sum();
            if w_sum > 0.0 {
                for c in 0..cols {
                    let add = ((rem as f64) * (weights[c] / w_sum)).floor() as usize;
                    w[c] = (w[c] + add).min(natural_w[c]);
                }
            }
            while w.iter().sum::<usize>() < avail {
                if let Some(best) = (0..cols)
                    .filter(|&c| w[c] < natural_w[c])
                    .max_by_key(|&c| natural_w[c] - w[c])
                {
                    w[best] += 1;
                } else {
                    break;
                }
            }
        }
        w
    };
    while col_w.iter().sum::<usize>() > avail && avail >= cols * 2 {
        if let Some((idx, _)) = col_w.iter().enumerate().max_by_key(|&(_, &cw)| cw) {
            if col_w[idx] <= 2 {
                break;
            }
            col_w[idx] -= 1;
        } else {
            break;
        }
    }

    let wrap_cell = |raw: &str, style: Style, width: usize| -> Vec<Vec<Span<'static>>> {
        let norm = normalize_cell_breaks(raw);
        let mut cell_lines = Vec::new();
        for seg in norm.split('\n') {
            let spans = inline_fn(seg.trim(), style);
            cell_lines.extend(wrap_styled_spans(spans, width));
        }
        if cell_lines.is_empty() {
            cell_lines.push(Vec::new());
        }
        cell_lines
    };

    let make_rule = |left: &str, mid: &str, right: &str| -> Line<'static> {
        let segs: Vec<String> = col_w.iter().map(|&w| "─".repeat(w + 2)).collect();
        Line::from(Span::styled(
            format!("{left}{}{right}", segs.join(mid)),
            border_style,
        ))
    };

    let render_multiline_row =
        |wrapped_cols: &[Vec<Vec<Span<'static>>>], is_header: bool| -> Vec<Line<'static>> {
            let row_h = wrapped_cols.iter().map(Vec::len).max().unwrap_or(1).max(1);
            let mut lines = Vec::with_capacity(row_h);
            for sub in 0..row_h {
                let mut spans = vec![Span::styled("│", border_style)];
                for c in 0..cols {
                    let target_w = col_w[c];
                    let cell_spans = wrapped_cols[c].get(sub).cloned().unwrap_or_default();
                    let used: usize = cell_spans
                        .iter()
                        .map(|sp| display_width(sp.content.as_ref()))
                        .sum();
                    let pad = target_w.saturating_sub(used);
                    let align = if is_header { ColAlign::Left } else { aligns[c] };
                    let (lpad, rpad) = match align {
                        ColAlign::Left => (0, pad),
                        ColAlign::Right => (pad, 0),
                        ColAlign::Center => (pad / 2, pad - pad / 2),
                    };
                    spans.push(Span::raw(format!(" {}", " ".repeat(lpad))));
                    spans.extend(cell_spans);
                    spans.push(Span::raw(format!("{} ", " ".repeat(rpad))));
                    spans.push(Span::styled("│", border_style));
                }
                lines.push(Line::from(spans));
            }
            lines
        };

    let wrapped_hdr: Vec<Vec<Vec<Span<'static>>>> = (0..cols)
        .map(|c| wrap_cell(&header_cells[c], hdr_style, col_w[c]))
        .collect();
    let wrapped_body: Vec<Vec<Vec<Vec<Span<'static>>>>> = data_rows
        .iter()
        .map(|r| {
            (0..cols)
                .map(|c| wrap_cell(&r[c], body_style, col_w[c]))
                .collect()
        })
        .collect();
    let any_multiline = wrapped_hdr.iter().any(|c| c.len() > 1)
        || wrapped_body
            .iter()
            .any(|row| row.iter().any(|c| c.len() > 1));

    let mut out = Vec::with_capacity(data_rows.len() * 2 + 4);
    out.push(make_rule("┌", "┬", "┐"));
    out.extend(render_multiline_row(&wrapped_hdr, true));
    out.push(make_rule("├", "┼", "┤"));
    for (r_idx, row_cols) in wrapped_body.iter().enumerate() {
        if r_idx > 0 && any_multiline {
            out.push(make_rule("├", "┼", "┤"));
        }
        out.extend(render_multiline_row(row_cols, false));
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

    #[test]
    fn wide_7col_cjk_table_wraps_multiline_and_falls_back_to_cards_without_ellipsis() {
        let rows = [
            "| ID | 推文中的观点 | 可检验的主张 | 已披露信息 | 尚缺信息 | 后续怎么验证 | 状态 |",
            "|---|---|---|---|---|---|---|",
            "| H-01 | 因“英伟达可能超预期”在低点买入 | 条件判断：若财报超预期则走强 | 说自己在低点建仓 | 买入的具体标的与仓位 | 先定义观察资产与窗口 | 待验证；交易细节未披露 |",
            "| H-02 | “加息十二月一定落地”宏观判断 | 宏观预测：十二月央行决议加息 | 有明确的加息时间点 | 推文所指年份与幅度 | 核对对应央行决议公告 | 不可直接评分 |",
        ];

        // 1. Desktop width (132 cols): proportional multi-line box table, zero `…` truncation!
        let box_lines =
            render_gfm_table(&rows, 132, |s, st| vec![Span::styled(s.to_owned(), st)]).unwrap();
        let box_text: String = box_lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(box_lines[0].to_string().starts_with('┌'));
        assert!(!box_text.contains('…'), "table must wrap instead of truncating with …:\n{box_text}");
        for phrase in [
            "因“英伟达可能超预期”",
            "在低点买入",
            "说自己在低点建仓",
            "买入的具体标的与",
            "仓位",
            "待验证；交易细节",
            "未披露",
            "核对对应央行决议",
            "公告",
        ] {
            assert!(
                box_text.contains(phrase),
                "missing `{phrase}` in wrapped table:\n{box_text}"
            );
        }
        // Every rendered row must have identical display width <= 132.
        let w0 = display_width(&box_lines[0].to_string());
        assert!(w0 <= 132);
        for line in &box_lines {
            assert_eq!(display_width(&line.to_string()), w0);
        }

        // 2. Narrow width (72 cols): stacked vertical cards, zero `…` truncation!
        let card_lines =
            render_gfm_table(&rows, 72, |s, st| vec![Span::styled(s.to_owned(), st)]).unwrap();
        let card_text: String = card_lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(card_lines[0].to_string().starts_with('╭'));
        assert!(!card_text.contains('…'));
        assert!(card_text.contains("ID: H-01"));
        assert!(card_text.replace([' ', '│', '\n'], "").contains("具体标的与仓位"));
    }
}
