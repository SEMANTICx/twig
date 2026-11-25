//! Visual design system, icon grammar, and motion primitives for `twig chat`:
//! - Claude Code warm terracotta palette (`#D77757`) + organic floral star breathing
//! - OpenAI Codex (`codex-rs/tui`) cadenced column-width shimmer, RGB `blend()`, and diff gutter
//! - Semantic tool & status glyphs, unified diff colorization, and transcript search highlights

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Claude Code warm terracotta minimalist palette (RGB tuples for continuous `blend()`).
pub const ACCENT_RGB: (u8, u8, u8) = (215, 119, 87); // #D77757 Claude terracotta
pub const ACCENT_DIM_RGB: (u8, u8, u8) = (158, 92, 70); // deep terracotta breath trough
pub const ACCENT_PEAK_RGB: (u8, u8, u8) = (248, 206, 178); // #F8CEB2 luminous shimmer peak
pub const BASE_SHIMMER_RGB: (u8, u8, u8) = (162, 124, 106); // warm stone-terracotta shimmer base
pub const AMBER_RGB: (u8, u8, u8) = (232, 177, 98); // #E8B162 steer / approval / warn
pub const SAGE_RGB: (u8, u8, u8) = (128, 192, 138); // #80C08A success / diff +
pub const ROSE_RGB: (u8, u8, u8) = (224, 110, 118); // #E06E76 error / diff -
pub const IVORY_RGB: (u8, u8, u8) = (236, 232, 224); // #ECE8E0 primary text
pub const STONE_RGB: (u8, u8, u8) = (168, 160, 150); // #A8A096 secondary text
pub const MUTED_RGB: (u8, u8, u8) = (114, 108, 101); // #726C65 hints / tree connectors
pub const HAIRLINE_RGB: (u8, u8, u8) = (74, 70, 66); // #4A4642 resting hairline rule
pub const HAIRLINE_FOCUS_RGB: (u8, u8, u8) = (126, 102, 88); // warm active hairline
pub const SURFACE_RGB: (u8, u8, u8) = (38, 35, 32); // #262320 warm subtle surface
pub const SURFACE_HI_RGB: (u8, u8, u8) = (56, 49, 44); // #38312C selected row highlight

/// Codex `diff_render.rs` dark TrueColor diff background tints.
const DARK_TC_ADD_LINE_BG: Color = Color::Rgb(33, 58, 43); // #213A2B
const DARK_TC_DEL_LINE_BG: Color = Color::Rgb(74, 34, 29); // #4A221D

pub const ACCENT: Color = Color::Rgb(ACCENT_RGB.0, ACCENT_RGB.1, ACCENT_RGB.2);
pub const ACCENT_PEAK: Color = Color::Rgb(ACCENT_PEAK_RGB.0, ACCENT_PEAK_RGB.1, ACCENT_PEAK_RGB.2);
pub const AMBER: Color = Color::Rgb(AMBER_RGB.0, AMBER_RGB.1, AMBER_RGB.2);
pub const SAGE: Color = Color::Rgb(SAGE_RGB.0, SAGE_RGB.1, SAGE_RGB.2);
pub const ROSE: Color = Color::Rgb(ROSE_RGB.0, ROSE_RGB.1, ROSE_RGB.2);
pub const IVORY: Color = Color::Rgb(IVORY_RGB.0, IVORY_RGB.1, IVORY_RGB.2);
pub const STONE: Color = Color::Rgb(STONE_RGB.0, STONE_RGB.1, STONE_RGB.2);
pub const MUTED: Color = Color::Rgb(MUTED_RGB.0, MUTED_RGB.1, MUTED_RGB.2);
pub const HAIRLINE: Color = Color::Rgb(HAIRLINE_RGB.0, HAIRLINE_RGB.1, HAIRLINE_RGB.2);
pub const HAIRLINE_FOCUS: Color =
    Color::Rgb(HAIRLINE_FOCUS_RGB.0, HAIRLINE_FOCUS_RGB.1, HAIRLINE_FOCUS_RGB.2);
pub const SURFACE: Color = Color::Rgb(SURFACE_RGB.0, SURFACE_RGB.1, SURFACE_RGB.2);
pub const SURFACE_HI: Color = Color::Rgb(SURFACE_HI_RGB.0, SURFACE_HI_RGB.1, SURFACE_HI_RGB.2);
pub const SEARCH_BG: Color = Color::Rgb(92, 66, 32);
pub const SEARCH_ACTIVE_BG: Color = ACCENT;

/// Claude Code organic floral star sequence.
const STAR_FRAMES: &[&str] = &["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];
const SPINNER_DELAY: Duration = Duration::from_millis(200);
const SPINNER_FRAME_MS: u128 = 105;

/// Codex `summary_shimmer.rs` desktop-matched cadence timings.
const SHIMMER_START_DELAY: Duration = Duration::from_millis(600);
const SHIMMER_SWEEP_SECS: f64 = 1.0;
const SHIMMER_INTERVAL_SECS: f64 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionMode {
    Animated,
    Reduced,
}

impl MotionMode {
    pub fn from_env() -> Self {
        let reduced = std::env::var("TWIG_REDUCED_MOTION")
            .or_else(|_| std::env::var("REDUCED_MOTION"))
            .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"));
        if reduced {
            Self::Reduced
        } else {
            Self::Animated
        }
    }
}

/// Semantic crisp Unicode icon per tool (consistent width across all terminals, zero missing Nerd Font boxes).
pub fn tool_glyph(name: &str) -> &'static str {
    match name {
        "bash" => "$",
        "read" => "◇",
        "skill" => "✦",
        "subagent" => "◈",
        "papercut" => "⚑",
        _ => "◦",
    }
}

/// Ported from `codex-rs/tui/src/color.rs`: linear RGB alpha blend `fg` over `bg`.
pub fn blend(fg: (u8, u8, u8), bg: (u8, u8, u8), alpha: f32) -> (u8, u8, u8) {
    let a = alpha.clamp(0.0, 1.0);
    let r = (fg.0 as f32 * a + bg.0 as f32 * (1.0 - a)).round() as u8;
    let g = (fg.1 as f32 * a + bg.1 as f32 * (1.0 - a)).round() as u8;
    let b = (fg.2 as f32 * a + bg.2 as f32 * (1.0 - a)).round() as u8;
    (r, g, b)
}

/// Claude floral star with continuous sinusoidal warm-terracotta luminance breathing.
/// Respects Codex's 200ms onset delay when `respect_delay` is true (prevents fast-tool flicker).
pub fn breathing_star(elapsed: Duration, motion: MotionMode, respect_delay: bool) -> Span<'static> {
    if motion == MotionMode::Reduced {
        return Span::styled("✻", Style::default().fg(ACCENT));
    }
    if respect_delay && elapsed < SPINNER_DELAY {
        return Span::styled("·", Style::default().fg(MUTED));
    }
    let idx = (elapsed.as_millis() / SPINNER_FRAME_MS) as usize % STAR_FRAMES.len();
    let phase = elapsed.as_secs_f32() * 3.0;
    let breath = 0.5 * (1.0 + phase.sin());
    let (r, g, b) = if breath > 0.55 {
        blend(ACCENT_PEAK_RGB, ACCENT_RGB, (breath - 0.55) / 0.45)
    } else {
        blend(ACCENT_RGB, ACCENT_DIM_RGB, breath / 0.55)
    };
    let mut style = Style::default().fg(Color::Rgb(r, g, b));
    if breath > 0.65 {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled(STAR_FRAMES[idx], style)
}

/// Subtle terracotta streaming caret appended while assistant deltas are actively arriving.
pub fn streaming_caret(elapsed: Duration, motion: MotionMode) -> Span<'static> {
    if motion == MotionMode::Reduced {
        return Span::styled(" ▎", Style::default().fg(ACCENT));
    }
    let breath = 0.5 * (1.0 + (elapsed.as_secs_f32() * 5.0).sin());
    let (r, g, b) = blend(ACCENT_PEAK_RGB, SURFACE_RGB, 0.25 + 0.75 * breath);
    Span::styled(" ▎", Style::default().fg(Color::Rgb(r, g, b)))
}

/// Breathing warm terracotta rule color while a turn is active.
pub fn active_rule_color(elapsed: Duration, motion: MotionMode) -> Color {
    if motion == MotionMode::Reduced {
        return ACCENT;
    }
    let breath = 0.5 * (1.0 + (elapsed.as_secs_f32() * 2.1).sin());
    let (r, g, b) = blend(ACCENT_RGB, HAIRLINE_FOCUS_RGB, 0.35 + 0.65 * breath);
    Color::Rgb(r, g, b)
}

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Ported from `codex-rs/tui/src/status_indicator_widget.rs`: compact human elapsed time.
pub fn fmt_elapsed_compact(elapsed_secs: u64) -> String {
    if elapsed_secs < 60 {
        return format!("{elapsed_secs}s");
    }
    if elapsed_secs < 3600 {
        let minutes = elapsed_secs / 60;
        let seconds = elapsed_secs % 60;
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = elapsed_secs / 3600;
    let minutes = (elapsed_secs % 3600) / 60;
    let seconds = elapsed_secs % 60;
    format!("{hours}h {minutes:02}m {seconds:02}s")
}

/// Returns `true` during the active 1s shimmer sweep window (for 60fps adaptive pacing).
pub fn shimmer_is_sweeping(elapsed: Duration, motion: MotionMode) -> bool {
    if motion == MotionMode::Reduced || elapsed < SHIMMER_START_DELAY {
        return false;
    }
    let cycle = elapsed.saturating_sub(SHIMMER_START_DELAY).as_secs_f64() % SHIMMER_INTERVAL_SECS;
    cycle <= SHIMMER_SWEEP_SECS + 0.08
}

/// Ported from `codex-rs/tui/src/summary_shimmer.rs` with Claude warm terracotta glow:
/// 600ms delay → 1.0s smooth display-column cosine sweep every 4.0s.
pub fn summary_shimmer(text: &str, elapsed: Duration, motion: MotionMode) -> Vec<Span<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    if motion == MotionMode::Reduced {
        return vec![Span::styled(text.to_owned(), Style::default().fg(ACCENT))];
    }
    let width = display_width(text) as f64;
    let half_width = (width * 0.12).max(3.0);
    let sweep = if elapsed < SHIMMER_START_DELAY {
        0.0
    } else {
        (elapsed.saturating_sub(SHIMMER_START_DELAY).as_secs_f64() % SHIMMER_INTERVAL_SECS)
            .min(SHIMMER_SWEEP_SECS)
    };
    let active_band = elapsed >= SHIMMER_START_DELAY && sweep < SHIMMER_SWEEP_SECS;
    let position = sweep / SHIMMER_SWEEP_SECS * (width + 2.0 * half_width) - half_width;
    let mut column = 0.0f64;

    text.chars()
        .map(|ch| {
            let glyph_w = UnicodeWidthChar::width(ch).unwrap_or(1) as f64;
            let center = column + glyph_w / 2.0;
            column += glyph_w;
            let intensity = if active_band {
                let distance = ((center - position).abs() / half_width).min(1.0);
                0.5 * (1.0 + (std::f64::consts::PI * distance).cos())
            } else {
                0.0
            };
            let alpha = (0.35 + 0.65 * intensity) as f32;
            let (r, g, b) = blend(ACCENT_PEAK_RGB, BASE_SHIMMER_RGB, alpha);
            let mut style = Style::default().fg(Color::Rgb(r, g, b));
            if intensity > 0.55 {
                style = style.add_modifier(Modifier::BOLD);
            }
            Span::styled(ch.to_string(), style)
        })
        .collect()
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

/// Tracks hunk line numbers for unified diff rendering (inspired by Codex `diff_render.rs`).
#[derive(Default)]
pub struct DiffGutterState {
    old_line: Option<usize>,
    new_line: Option<usize>,
}

impl DiffGutterState {
    pub fn render_line(&mut self, raw: &str, max_width: usize) -> Vec<Vec<Span<'static>>> {
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
        let (gutter_num, sign, body, line_style) = if let Some(rest) = raw.strip_prefix('+') {
            let num = self.new_line.inspect(|&n| {
                self.new_line = Some(n + 1);
            });
            (
                num,
                Some('+'),
                rest,
                Style::default().fg(SAGE).bg(DARK_TC_ADD_LINE_BG),
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
            )
        } else if in_hunk && raw.starts_with(' ') {
            let num = self.new_line.inspect(|&n| {
                self.new_line = Some(n + 1);
            });
            if let Some(o) = self.old_line {
                self.old_line = Some(o + 1);
            }
            (num, Some(' '), &raw[1..], Style::default().fg(STONE))
        } else {
            return wrap_plain(raw, max_width)
                .into_iter()
                .map(|w| vec![Span::styled(w, Style::default().fg(STONE))])
                .collect();
        };

        let prefix_w = if gutter_num.is_some() { 7 } else { 2 };
        let content_w = max_width.saturating_sub(prefix_w).max(8);
        let sign_ch = sign.unwrap_or(' ');
        let gutter_bg = line_style.bg.unwrap_or(Color::Reset);
        let gutter_style = Style::default().fg(MUTED).bg(gutter_bg);

        wrap_plain(body, content_w)
            .into_iter()
            .enumerate()
            .map(|(idx, segment)| {
                let mut spans = Vec::with_capacity(3);
                if let Some(n) = gutter_num {
                    let num_str = if idx == 0 {
                        format!("{n:>4} ")
                    } else {
                        "     ".to_owned()
                    };
                    spans.push(Span::styled(num_str, gutter_style));
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

/// Render streaming assistant markdown into wrapped styled Ratatui lines with rounded code caps.
pub fn render_markdown(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let width = max_width.max(12);
    let mut out = Vec::new();
    let mut in_code = false;
    let mut is_diff_block = false;
    let mut diff_gutter = DiffGutterState::default();
    let body = Style::default().fg(IVORY);

    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim_start();

        if let Some(fence) = trimmed.strip_prefix("```") {
            in_code = !in_code;
            if in_code {
                let lang = fence.trim();
                is_diff_block =
                    lang.eq_ignore_ascii_case("diff") || lang.eq_ignore_ascii_case("patch");
                diff_gutter = DiffGutterState::default();
                let label = if lang.is_empty() {
                    "╭─ code ".to_string()
                } else {
                    format!("╭─ {lang} ")
                };
                let rule_len = width.saturating_sub(display_width(&label)).min(48);
                out.push(Line::from(vec![
                    Span::styled("╭─ ", Style::default().fg(HAIRLINE)),
                    Span::styled(
                        if lang.is_empty() { "code" } else { lang }.to_owned(),
                        Style::default().fg(STONE),
                    ),
                    Span::styled(
                        format!(" {}", "─".repeat(rule_len)),
                        Style::default().fg(HAIRLINE),
                    ),
                ]));
            } else {
                is_diff_block = false;
                let rule_len = width.saturating_sub(1).min(56);
                out.push(Line::from(Span::styled(
                    format!("╰{}", "─".repeat(rule_len)),
                    Style::default().fg(HAIRLINE),
                )));
            }
            continue;
        }

        if in_code {
            if is_diff_block {
                for row in diff_gutter.render_line(line, width.saturating_sub(2)) {
                    let mut spans = vec![Span::styled("│ ", Style::default().fg(HAIRLINE))];
                    spans.extend(row);
                    out.push(Line::from(spans));
                }
            } else {
                let code_style = Style::default().fg(ACCENT_PEAK);
                for wrapped in wrap_plain(line, width.saturating_sub(2)) {
                    out.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(HAIRLINE)),
                        Span::styled(wrapped, code_style),
                    ]));
                }
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

/// Critically-damped exponential scroll viewport for silky 60fps motion without teleporting.
pub struct SmoothScroll {
    pos: f32,
    target: f32,
    max: u16,
    auto_follow: bool,
}

impl SmoothScroll {
    pub fn new() -> Self {
        Self {
            pos: 0.0,
            target: 0.0,
            max: 0,
            auto_follow: true,
        }
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.target = 0.0;
        self.max = 0;
        self.auto_follow = true;
    }

    pub fn follow_tail(&mut self) {
        self.auto_follow = true;
        self.target = self.max as f32;
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let next = (self.target + delta as f32).clamp(0.0, self.max as f32);
        self.target = next;
        if delta < 0 {
            self.auto_follow = false;
        } else if next >= self.max as f32 {
            self.auto_follow = true;
        }
    }

    pub fn jump_to(&mut self, line: usize) {
        self.auto_follow = false;
        self.target = (line as f32).min(self.max as f32);
    }

    pub fn sync_bounds(&mut self, max_scroll: u16, motion: MotionMode) {
        self.max = max_scroll;
        let max_f = max_scroll as f32;
        if self.auto_follow {
            self.target = max_f;
            if motion == MotionMode::Reduced || (max_f - self.pos).abs() > 24.0 {
                self.pos = max_f;
            }
        } else {
            self.target = self.target.min(max_f);
            self.pos = self.pos.min(max_f);
        }
    }

    pub fn step(&mut self, dt_secs: f32, motion: MotionMode) {
        let diff = self.target - self.pos;
        if motion == MotionMode::Reduced || diff.abs() < 0.25 {
            self.pos = self.target;
            return;
        }
        let factor = 1.0 - (-dt_secs.clamp(0.004, 0.1) * 18.0).exp();
        self.pos += diff * factor;
        if (self.target - self.pos).abs() < 0.25 {
            self.pos = self.target;
        }
    }

    pub fn is_animating(&self, motion: MotionMode) -> bool {
        motion == MotionMode::Animated && (self.target - self.pos).abs() >= 0.25
    }

    pub fn current_u16(&self) -> u16 {
        self.pos.round().clamp(0.0, self.max as f32) as u16
    }

    pub fn show_tail_badge(&self) -> bool {
        !self.auto_follow && self.current_u16() < self.max
    }
}

/// Claude Code iconic rounded terracotta welcome card with responsive 2-column layout.
pub fn render_welcome_card(
    model: &str,
    repo_label: &str,
    git_branch: Option<&str>,
    repo_path: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let card_w = width.saturating_sub(4).clamp(32, 78);
    let inner = card_w.saturating_sub(2);
    let title = format!(" ✻ twig v{} ", env!("CARGO_PKG_VERSION"));
    let top_fill = inner.saturating_sub(display_width(&title) + 1);
    let border = Style::default().fg(ACCENT);
    let branch = git_branch.map(|b| format!(" ({b})")).unwrap_or_default();
    let repo_meta = format!("{repo_label}{branch}");
    let clip = |s: &str, max_c: usize| -> String {
        let lim = max_c.max(8);
        let out: String = s.chars().take(lim).collect();
        if s.chars().count() > lim {
            format!("{out}…")
        } else {
            out
        }
    };

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("╭─", border),
        Span::styled(
            title,
            Style::default().fg(ACCENT_PEAK).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{}╮", "─".repeat(top_fill)), border),
    ]));

    let blank_row = Line::from(vec![
        Span::raw("  "),
        Span::styled("│", border),
        Span::raw(" ".repeat(inner)),
        Span::styled("│", border),
    ]);
    lines.push(blank_row.clone());

    if inner >= 62 {
        let left_w = 32usize;
        let right_w = inner.saturating_sub(left_w + 3);
        let two_col = |l_spans: Vec<Span<'static>>, r_spans: Vec<Span<'static>>| {
            let lw: usize = l_spans.iter().map(|s| display_width(s.content.as_ref())).sum();
            let rw: usize = r_spans.iter().map(|s| display_width(s.content.as_ref())).sum();
            let mut row = vec![Span::raw("  "), Span::styled("│", border)];
            row.extend(l_spans);
            row.push(Span::raw(" ".repeat(left_w.saturating_sub(lw))));
            row.push(Span::styled(" │ ", Style::default().fg(HAIRLINE)));
            row.extend(r_spans);
            row.push(Span::raw(" ".repeat(right_w.saturating_sub(rw))));
            row.push(Span::styled("│", border));
            Line::from(row)
        };

        lines.push(two_col(
            vec![
                Span::styled("   ╭───╮  ", Style::default().fg(ACCENT)),
                Span::styled(
                    "Welcome back",
                    Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
                ),
            ],
            vec![Span::styled(
                "Tips for getting started",
                Style::default().fg(ACCENT_PEAK).add_modifier(Modifier::BOLD),
            )],
        ));
        lines.push(two_col(
            vec![
                Span::styled("   │ ", Style::default().fg(ACCENT)),
                Span::styled(
                    "✻",
                    Style::default().fg(ACCENT_PEAK).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" │  ", Style::default().fg(ACCENT)),
                Span::styled(
                    clip(&format!("{model} · {repo_meta}"), left_w - 11),
                    Style::default().fg(STONE),
                ),
            ],
            vec![Span::styled(
                clip("Enter send (steer while running)", right_w),
                Style::default().fg(STONE),
            )],
        ));
        lines.push(two_col(
            vec![
                Span::styled("   ╰───╯  ", Style::default().fg(ACCENT)),
                Span::styled(clip(repo_path, left_w - 11), Style::default().fg(MUTED)),
            ],
            vec![Span::styled(
                clip("@ file · / command · ? shortcuts", right_w),
                Style::default().fg(MUTED),
            )],
        ));
    } else {
        let one_col = |spans: Vec<Span<'static>>| {
            let w: usize = spans.iter().map(|s| display_width(s.content.as_ref())).sum();
            let mut row = vec![Span::raw("  "), Span::styled("│", border)];
            row.extend(spans);
            row.push(Span::raw(" ".repeat(inner.saturating_sub(w))));
            row.push(Span::styled("│", border));
            Line::from(row)
        };
        lines.push(one_col(vec![
            Span::styled("  ✻ ", Style::default().fg(ACCENT)),
            Span::styled(
                clip(&format!("{model} · {repo_meta}"), inner - 5),
                Style::default().fg(IVORY),
            ),
        ]));
        lines.push(one_col(vec![Span::styled(
            format!("    {}", clip(repo_path, inner - 5)),
            Style::default().fg(MUTED),
        )]));
    }

    lines.push(blank_row);
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("╰{}╯", "─".repeat(inner)), border),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Press ? for shortcuts · ^O tool tree · ^F find · ^G $EDITOR · Alt+Y copy code",
        Style::default().fg(MUTED),
    )));
    lines
}

/// 2-column Codex-style shortcut reference rows for the `?` footer overlay.
pub fn shortcut_overlay_lines() -> Vec<Line<'static>> {
    let row = |k1: &str, d1: &str, k2: &str, d2: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("  {k1:<13}"),
                Style::default()
                    .fg(ACCENT_PEAK)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{d1:<24}"), Style::default().fg(STONE)),
            Span::styled(
                format!("{k2:<13}"),
                Style::default()
                    .fg(ACCENT_PEAK)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(d2.to_owned(), Style::default().fg(STONE)),
        ])
    };
    vec![
        row(
            "Enter",
            "send / steer while busy",
            "Shift+Enter",
            "insert newline",
        ),
        row(
            "Esc",
            "interrupt / press×2 clear",
            "@  /  Tab",
            "file & slash completion",
        ),
        row(
            "Ctrl+O",
            "expand/collapse tools",
            "Ctrl+F",
            "search transcript",
        ),
        row(
            "Ctrl+G",
            "compose in $EDITOR",
            "Alt+Y",
            "copy latest code block",
        ),
        Line::from(Span::styled(
            "  ? or Esc to close · PgUp/PgDn/wheel smooth scroll · Ctrl+C quit",
            Style::default().fg(MUTED),
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smooth_scroll_eases_gradually_and_snaps_in_reduced_motion() {
        let mut scroll = SmoothScroll::new();
        scroll.sync_bounds(40, MotionMode::Animated);
        scroll.scroll_by(-20);
        assert!(scroll.is_animating(MotionMode::Animated));
        scroll.step(0.016, MotionMode::Animated);
        let mid = scroll.current_u16();
        assert!(mid < 40 && mid > 20, "expected intermediate ease, got {mid}");

        scroll.step(0.016, MotionMode::Reduced);
        assert_eq!(scroll.current_u16(), 20);
    }

    #[test]
    fn wraps_cjk_by_display_width() {
        let lines = wrap_plain("你好世界测试", 8);
        assert_eq!(lines, vec!["你好世界", "测试"]);
        assert_eq!(display_width("你好"), 4);
    }

    #[test]
    fn codex_shimmer_cadence_and_breathing_star_respect_delays() {
        assert_eq!(fmt_elapsed_compact(0), "0s");
        assert_eq!(fmt_elapsed_compact(59), "59s");
        assert_eq!(fmt_elapsed_compact(65), "1m 05s");
        assert_eq!(fmt_elapsed_compact(3661), "1h 01m 01s");

        assert!(!shimmer_is_sweeping(
            Duration::from_millis(200),
            MotionMode::Animated
        ));
        assert!(shimmer_is_sweeping(
            Duration::from_millis(900),
            MotionMode::Animated
        ));
        assert!(!shimmer_is_sweeping(
            Duration::from_millis(2500),
            MotionMode::Animated
        ));
        assert_eq!(
            breathing_star(Duration::from_millis(50), MotionMode::Animated, true).content,
            "·"
        );
        assert_eq!(
            breathing_star(Duration::from_secs(1), MotionMode::Reduced, true).content,
            "✻"
        );
        assert_eq!(
            summary_shimmer("Reasoning", Duration::from_secs(1), MotionMode::Reduced).len(),
            1
        );
    }

    #[test]
    fn renders_rounded_code_blocks_hunk_diff_gutter_and_extracts_blocks() {
        let md = "# Title\n- check `foo` and **bar**\n```diff\n@@ -10,2 +10,2 @@\n-old\n+new\n```\n```rust\nlet x = 1;\n```";
        let rendered = render_markdown(md, 52);
        assert!(rendered[0].to_string().contains("Title"));
        assert!(rendered[1].to_string().contains("• check"));
        assert!(rendered.iter().any(|l| l.to_string().contains("╭─ diff")));
        assert!(rendered.iter().any(|l| l.to_string().contains("10 - old")));
        assert!(rendered.iter().any(|l| l.to_string().contains("10 + new")));

        let blocks = extract_code_blocks(md);
        assert_eq!(blocks, vec!["@@ -10,2 +10,2 @@\n-old\n+new", "let x = 1;"]);
    }
}
