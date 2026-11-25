//! Rendering & motion primitives combining:
//! - Claude Code warm terracotta palette (`#D97757`) & typography
//! - OpenAI Codex (`codex-rs/tui`) upstream algorithms:
//!   - `color.rs`: RGB linear alpha `blend()` and BT.601 `is_light()`
//!   - `summary_shimmer.rs`: cadenced 600ms-delay / 1s-sweep / 4s-interval column-centered cosine shimmer
//!   - `diff_render.rs`: dark TrueColor diff backgrounds (`#213A2B` / `#4A221D`) & line-number gutter parsing
//!   - `motion.rs`: explicit `MotionMode` (`Animated` / `Reduced`) and 200ms spinner delay

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Claude Code warm terracotta minimalist palette (RGB tuples for `blend()` math).
pub const ACCENT_RGB: (u8, u8, u8) = (217, 119, 87); // #D97757 terracotta
pub const ACCENT_PEAK_RGB: (u8, u8, u8) = (248, 206, 178); // #F8CEB2 warm shimmer peak
pub const BASE_SHIMMER_RGB: (u8, u8, u8) = (156, 118, 100); // warm muted terracotta base
pub const AMBER_RGB: (u8, u8, u8) = (230, 173, 95); // #E6AD5F warn / steer / approval
pub const SAGE_RGB: (u8, u8, u8) = (126, 189, 134); // #7EBD86 success / diff +
pub const ROSE_RGB: (u8, u8, u8) = (224, 108, 117); // #E06C75 error / diff -
pub const IVORY_RGB: (u8, u8, u8) = (236, 232, 224); // #ECE8E0 primary text
pub const STONE_RGB: (u8, u8, u8) = (166, 158, 148); // #A69E94 secondary text
pub const MUTED_RGB: (u8, u8, u8) = (112, 106, 99); // #706A63 hints / dim connectors
pub const HAIRLINE_RGB: (u8, u8, u8) = (74, 70, 66); // #4A4642 hairline horizontal rules
pub const SURFACE_RGB: (u8, u8, u8) = (38, 35, 32); // #262320 subtle warm surface
pub const SURFACE_HI_RGB: (u8, u8, u8) = (54, 48, 43); // #36302B active row surface

/// Codex `diff_render.rs` exact dark TrueColor diff background tints.
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
pub const SURFACE: Color = Color::Rgb(SURFACE_RGB.0, SURFACE_RGB.1, SURFACE_RGB.2);
pub const SURFACE_HI: Color = Color::Rgb(SURFACE_HI_RGB.0, SURFACE_HI_RGB.1, SURFACE_HI_RGB.2);
pub const SEARCH_BG: Color = Color::Rgb(92, 66, 32);
pub const SEARCH_ACTIVE_BG: Color = ACCENT;

/// Codex `summary_shimmer.rs` exact desktop-matched cadence timings.
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

/// Ported directly from `codex-rs/tui/src/color.rs`: alpha blend `fg` over `bg`.
pub fn blend(fg: (u8, u8, u8), bg: (u8, u8, u8), alpha: f32) -> (u8, u8, u8) {
    let a = alpha.clamp(0.0, 1.0);
    let r = (fg.0 as f32 * a + bg.0 as f32 * (1.0 - a)).round() as u8;
    let g = (fg.1 as f32 * a + bg.1 as f32 * (1.0 - a)).round() as u8;
    let b = (fg.2 as f32 * a + bg.2 as f32 * (1.0 - a)).round() as u8;
    (r, g, b)
}

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Ported directly from `codex-rs/tui/src/status_indicator_widget.rs`:
/// compact human elapsed formatting (`0s`, `59s`, `1m 00s`, `1h 02m 03s`).
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

/// Returns `true` when the cadenced shimmer wave is actively sweeping (so the event loop
/// can run at 32ms during the 1s sweep and drop to calmer polling during the 3s rest).
pub fn shimmer_is_sweeping(elapsed: Duration, motion: MotionMode) -> bool {
    if motion == MotionMode::Reduced || elapsed < SHIMMER_START_DELAY {
        return false;
    }
    let cycle = elapsed.saturating_sub(SHIMMER_START_DELAY).as_secs_f64() % SHIMMER_INTERVAL_SECS;
    cycle <= SHIMMER_SWEEP_SECS + 0.08
}

/// Ported directly from `codex-rs/tui/src/summary_shimmer.rs` with warm terracotta palette:
/// - Waits 600ms after a new status header appears
/// - Sweeps for 1.0s every 4.0s (calm rest in between, zero agent-fatigue jitter)
/// - Uses display-column centers (`UnicodeWidthChar`) so CJK sweeps at identical visual speed
/// - Minimum half-width of 3.0 columns (6-col wave) so short words never flicker per-letter
pub fn summary_shimmer(text: &str, elapsed: Duration, motion: MotionMode) -> Vec<Span<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    if motion == MotionMode::Reduced {
        return vec![Span::styled(text.to_owned(), Style::default().fg(ACCENT))];
    }
    let width = display_width(text) as f64;
    let half_width = (width * 0.1).max(3.0);
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
    /// Render one line from tool output or a `diff` block with optional right-aligned line numbers
    /// and Codex's exact `#213A2B` / `#4A221D` TrueColor diff backgrounds.
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

/// Render streaming assistant markdown into wrapped styled Ratatui lines.
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
    fn codex_shimmer_cadence_and_elapsed_formatting_match_upstream() {
        assert_eq!(fmt_elapsed_compact(0), "0s");
        assert_eq!(fmt_elapsed_compact(59), "59s");
        assert_eq!(fmt_elapsed_compact(65), "1m 05s");
        assert_eq!(fmt_elapsed_compact(3661), "1h 01m 01s");

        // Before 600ms start delay, shimmer rests calmly.
        assert!(!shimmer_is_sweeping(
            Duration::from_millis(200),
            MotionMode::Animated
        ));
        // Inside 1s sweep window after 600ms delay, shimmer actively sweeps.
        assert!(shimmer_is_sweeping(
            Duration::from_millis(900),
            MotionMode::Animated
        ));
        // During 3s rest window (e.g. 2.5s elapsed -> 1.9s into cycle), shimmer rests.
        assert!(!shimmer_is_sweeping(
            Duration::from_millis(2500),
            MotionMode::Animated
        ));
        // Reduced motion returns a single static span.
        assert_eq!(
            summary_shimmer("Reasoning…", Duration::from_secs(1), MotionMode::Reduced).len(),
            1
        );
        assert_eq!(
            summary_shimmer("Reasoning…", Duration::from_secs(1), MotionMode::Animated).len(),
            "Reasoning…".chars().count()
        );
    }

    #[test]
    fn renders_code_blocks_hunk_diff_gutter_and_extracts_blocks() {
        let md = "# Title\n- check `foo` and **bar**\n```diff\n@@ -10,2 +10,2 @@\n-old\n+new\n```\n```rust\nlet x = 1;\n```";
        let rendered = render_markdown(md, 52);
        assert!(rendered[0].to_string().contains("Title"));
        assert!(rendered[1].to_string().contains("• check"));
        assert!(rendered.iter().any(|l| l.to_string().contains("10 - old")));
        assert!(rendered.iter().any(|l| l.to_string().contains("10 + new")));

        let blocks = extract_code_blocks(md);
        assert_eq!(blocks, vec!["@@ -10,2 +10,2 @@\n-old\n+new", "let x = 1;"]);
    }
}
