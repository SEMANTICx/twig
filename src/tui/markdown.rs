//! Visual design system, icon grammar, and motion primitives for `twig chat`:
//! - Claude Code warm terracotta palette (`#D77757`) + organic floral star breathing
//! - OpenAI Codex (`codex-rs/tui`) cadenced column-width shimmer, RGB `blend()`, and diff gutter
//! - Semantic tool & status glyphs, unified diff colorization, and transcript search highlights

use std::time::Duration;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Claude Code warm terracotta minimalist palette (RGB tuples for continuous `blend()`).
pub const ACCENT_RGB: (u8, u8, u8) = (215, 119, 87); // #D77757 Claude terracotta
pub const ACCENT_DIM_RGB: (u8, u8, u8) = (158, 92, 70); // deep terracotta breath trough
pub const ACCENT_PEAK_RGB: (u8, u8, u8) = (248, 206, 178); // #F8CEB2 luminous shimmer peak
pub const BASE_SHIMMER_RGB: (u8, u8, u8) = (162, 124, 106); // warm stone-terracotta shimmer base
pub const AMBER_RGB: (u8, u8, u8) = (232, 177, 98); // #E8B162 steer / approval / warn
pub const SAGE_RGB: (u8, u8, u8) = (128, 192, 138); // #80C08A success / diff +
pub const ROSE_RGB: (u8, u8, u8) = (224, 110, 118); // #E06E76 error / diff -
pub const IVORY_RGB: (u8, u8, u8) = (240, 236, 229); // #F0ECE5 crisp primary text
pub const STONE_RGB: (u8, u8, u8) = (192, 185, 176); // #C0B9B0 high-contrast secondary text (readable on transparent terminals)
pub const MUTED_RGB: (u8, u8, u8) = (150, 143, 134); // #968F86 legible hints / tree connectors over bright wallpapers
pub const HAIRLINE_RGB: (u8, u8, u8) = (104, 96, 89); // #686059 crisp resting hairline rule on dark & transparent terminals
pub const HAIRLINE_FOCUS_RGB: (u8, u8, u8) = (154, 122, 104); // warm active hairline
pub const SURFACE_RGB: (u8, u8, u8) = (38, 35, 32); // #262320 warm subtle surface
pub const SURFACE_HI_RGB: (u8, u8, u8) = (56, 49, 44); // #38312C selected row highlight

/// Codex/Claude dark TrueColor diff line & intra-line word-diff background tints.
const DARK_TC_ADD_LINE_BG: Color = Color::Rgb(33, 58, 43); // #213A2B
const DARK_TC_DEL_LINE_BG: Color = Color::Rgb(74, 34, 29); // #4A221D
const DARK_TC_ADD_WORD_BG: Color = Color::Rgb(46, 108, 66); // #2E6C42 intra-line added token
const DARK_TC_DEL_WORD_BG: Color = Color::Rgb(132, 48, 42); // #84302A intra-line removed token
pub const BASH_MODE_COLOR: Color = Color::Rgb(110, 198, 216); // Claude `!` bash mode cyan
pub const MEMORY_MODE_COLOR: Color = Color::Rgb(186, 148, 236); // Claude `#` memory mode lavender

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
        "edit" | "apply_patch" => "✎",
        "write" => "+",
        "grep" => "⌕",
        "glob" => "◌",
        "outline" => "≡",
        "diff" => "±",
        "todo" => "☑",
        "mcp" => "⌘",
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

        // Single-line intra-line word highlight when line fits within `content_w` and has a paired peer.
        if let (Some(peer), Some(hi_bg)) = (peer_body, word_bg) {
            if display_width(body) <= content_w {
                let (pre, mid, suf) = split_intraline_change(body, peer);
                if !mid.is_empty() && ( !pre.is_empty() || !suf.is_empty() ) {
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

/// Parse inline `` `code` ``, `**bold**`, `*italic*`, `$latex$` / `\(latex\)`, and `[text](url)` spans.
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
                    Style::default().fg(AMBER),
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
                            Style::default().fg(AMBER),
                        ));
                        i = end_idx + 1;
                        continue;
                    }
                }
            }
        }
        // Markdown link `[label](url)`
        if chars[i] == '[' {
            if let Some(rel_rb) = chars[i + 1..].iter().position(|&c| c == ']') {
                let rb = i + 1 + rel_rb;
                if chars.get(rb + 1) == Some(&'(') {
                    if let Some(rel_rp) = chars[rb + 2..].iter().position(|&c| c == ')') {
                        let rp = rb + 2 + rel_rp;
                        flush(&mut buf, base, &mut spans);
                        let label: String = chars[i + 1..rb].iter().collect();
                        let url: String = chars[rb + 2..rp].iter().collect();
                        spans.push(Span::styled(
                            label.clone(),
                            Style::default()
                                .fg(ACCENT_PEAK)
                                .add_modifier(Modifier::UNDERLINED),
                        ));
                        if !url.is_empty() && url != label && !url.starts_with('#') {
                            spans.push(Span::styled(
                                format!(" ({url})"),
                                Style::default().fg(MUTED),
                            ));
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
                spans.push(Span::styled(
                    bold,
                    base.fg(IVORY).add_modifier(Modifier::BOLD),
                ));
                i = end_idx + 2;
                continue;
            }
        }
        // `*italic*`
        if chars[i] == '*'
            && chars.get(i + 1).is_some_and(|&c| c != '*' && !c.is_whitespace())
        {
            if let Some(rel_end) = chars[i + 1..]
                .iter()
                .position(|&c| c == '*')
                .filter(|&pos| pos > 0 && !chars[i + pos].is_whitespace())
            {
                flush(&mut buf, base, &mut spans);
                let italic: String = chars[i + 1..i + 1 + rel_end].iter().collect();
                spans.push(Span::styled(
                    italic,
                    base.add_modifier(Modifier::ITALIC),
                ));
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

/// Render streaming assistant markdown into wrapped styled Ratatui lines with
/// GFM box tables, Mermaid/SVG diagrams, Unicode LaTeX math, line-numbered code blocks,
/// GitHub callout alerts, task checklists, and unified diff gutters.
pub fn render_markdown(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let width = max_width.max(12);
    let mut out = Vec::new();
    let body = Style::default().fg(IVORY);

    let raw_lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .collect();
    let mut idx = 0usize;

    while idx < raw_lines.len() {
        let line = raw_lines[idx];
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
                idx += 1; // consume closing ```
            }
            out.extend(render_fenced_block(lang_raw, &code_lang, &block_lines, width));
            continue;
        }

        // Display LaTeX math block (`$$ ... $$` or `\[ ... \]`, single-line or multi-line).
        if trimmed.starts_with("$$") || trimmed.starts_with("\\[") {
            let close_tok = if trimmed.starts_with("$$") { "$$" } else { "\\]" };
            let open_len = 2;
            let after_open = &trimmed[open_len..];
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

        // GFM pipe table (`| col | col |` followed by `|---|---|` separator).
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
            if let Some(tbl_lines) =
                super::math_table::render_gfm_table(&raw_lines[start..idx], width, parse_inline)
            {
                out.extend(tbl_lines);
                continue;
            }
            idx = start;
        }

        if trimmed.is_empty() {
            out.push(Line::from(""));
            idx += 1;
            continue;
        }

        // Horizontal rule (`---`, `***`, `___`)
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

        // Markdown headings `#` .. `######`
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
            let rest = trimmed[hashes + 1..].trim();
            let heading = if hashes <= 2 {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ACCENT_PEAK).add_modifier(Modifier::BOLD)
            };
            for row_spans in wrap_spans(parse_inline(rest, heading), width) {
                out.push(Line::from(row_spans));
            }
            idx += 1;
            continue;
        }

        if trimmed.starts_with('>') {
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

            if let Some((badge, col, first_rem)) = alert {
                out.push(Line::from(vec![
                    Span::styled("▎ ", Style::default().fg(col)),
                    Span::styled(badge, Style::default().fg(col).add_modifier(Modifier::BOLD)),
                ]));
                if !first_rem.is_empty() {
                    for row_spans in wrap_spans(
                        parse_inline(first_rem, Style::default().fg(IVORY)),
                        width.saturating_sub(2),
                    ) {
                        let mut spans = vec![Span::styled("▎ ", Style::default().fg(col))];
                        spans.extend(row_spans);
                        out.push(Line::from(spans));
                    }
                }
                for &ql in &quote_lines[1..] {
                    for row_spans in wrap_spans(
                        parse_inline(ql, Style::default().fg(IVORY)),
                        width.saturating_sub(2),
                    ) {
                        let mut spans = vec![Span::styled("▎ ", Style::default().fg(col))];
                        spans.extend(row_spans);
                        out.push(Line::from(spans));
                    }
                }
            } else {
                for ql in quote_lines {
                    for row_spans in wrap_spans(
                        parse_inline(ql, Style::default().fg(STONE)),
                        width.saturating_sub(2),
                    ) {
                        let mut spans = vec![Span::styled("▎ ", Style::default().fg(ACCENT))];
                        spans.extend(row_spans);
                        out.push(Line::from(spans));
                    }
                }
            }
            continue;
        }

        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let (bullet, bullet_color, item_rest) = if let Some(r) =
                rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] "))
            {
                ("✓ ", SAGE, r)
            } else if let Some(r) = rest.strip_prefix("[ ] ") {
                ("☐ ", MUTED, r)
            } else {
                ("• ", ACCENT, rest)
            };
            let indent_width = width.saturating_sub(2);
            for (w_idx, row_spans) in wrap_spans(parse_inline(item_rest, body), indent_width)
                .into_iter()
                .enumerate()
            {
                let mut spans = vec![Span::styled(
                    if w_idx == 0 { bullet } else { "  " },
                    Style::default().fg(bullet_color),
                )];
                spans.extend(row_spans);
                out.push(Line::from(spans));
            }
            idx += 1;
            continue;
        }

        // Ordered list (`1. item`, `2) item`)
        let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
        if (1..=3).contains(&digits)
            && matches!(trimmed.as_bytes().get(digits), Some(b'.' | b')'))
            && trimmed.as_bytes().get(digits + 1) == Some(&b' ')
        {
            let marker = format!("{} ", &trimmed[..=digits]);
            let m_width = marker.len();
            let rest = &trimmed[digits + 2..];
            let indent_w = width.saturating_sub(m_width);
            for (w_idx, row_spans) in wrap_spans(parse_inline(rest, body), indent_w)
                .into_iter()
                .enumerate()
            {
                let mut spans = vec![Span::styled(
                    if w_idx == 0 {
                        marker.clone()
                    } else {
                        " ".repeat(m_width)
                    },
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )];
                spans.extend(row_spans);
                out.push(Line::from(spans));
            }
            idx += 1;
            continue;
        }

        for row_spans in wrap_spans(parse_inline(line, body), width) {
            out.push(Line::from(row_spans));
        }
        idx += 1;
    }

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

/// Wrap already-parsed styled inline spans by Unicode terminal display width so
/// inline Markdown (`**bold**`, `` `code` ``, `$latex$`, `[label](url)`) never breaks mid-delimiter.
fn wrap_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Vec<Span<'static>>> {
    let limit = max_width.max(4);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur_line: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    for span in spans {
        let style = span.style;
        let mut seg = String::new();
        for ch in span.content.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
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

/// Zero-dependency warm syntax highlighter for Rust/Python/TS/JS/Go/Bash/JSON/TOML.
fn highlight_code_line(line: &str, lang: &str) -> Vec<Span<'static>> {
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
        // Line comment (`//` or `#`)
        if (slash_comments && ch == '/' && chars.get(i + 1) == Some(&'/'))
            || (hash_comments && ch == '#')
        {
            flush(&mut plain, &mut out);
            let rest: String = chars[i..].iter().collect();
            out.push(Span::styled(rest, cmt_style));
            break;
        }
        // Quoted string (`"..."` or `'...'`)
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
        // Number literal
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
        // Identifier / keyword / PascalCase type
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
            let Some(first_idx) = char_map.iter().position(|&(ls, le, _, _)| abs_low >= ls && abs_low < le) else {
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

/// Critically-damped exponential scroll viewport (`usize`-indexed, immune to `u16` wraparound).
pub struct SmoothScroll {
    pos: f64,
    target: f64,
    max: usize,
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
        self.target = self.max as f64;
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let base = if delta < 0 && self.auto_follow {
            self.pos.min(self.target)
        } else {
            self.target
        };
        let next = (base + delta as f64).clamp(0.0, self.max as f64);
        self.target = next;
        if delta < 0 {
            self.auto_follow = false;
        } else if delta > 0 && next >= self.max as f64 {
            self.auto_follow = true;
        }
    }

    pub fn jump_to(&mut self, line: usize) {
        self.auto_follow = false;
        self.target = (line as f64).min(self.max as f64);
    }

    pub fn sync_bounds(&mut self, max_scroll: usize, motion: MotionMode) {
        self.max = max_scroll;
        let max_f = max_scroll as f64;
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
        let factor = 1.0 - (-(dt_secs.clamp(0.004, 0.1) as f64) * 18.0).exp();
        self.pos += diff * factor;
        if (self.target - self.pos).abs() < 0.25 {
            self.pos = self.target;
        }
    }

    pub fn is_animating(&self, motion: MotionMode) -> bool {
        motion == MotionMode::Animated && (self.target - self.pos).abs() >= 0.25
    }

    pub fn current_offset(&self) -> usize {
        self.pos.round().clamp(0.0, self.max as f64) as usize
    }

    pub fn show_tail_badge(&self) -> bool {
        !self.auto_follow && self.current_offset() < self.max
    }
}

/// Left-truncate a long path keeping the tail (`…/parent/repo`) for narrow/phone screens.
fn clip_path_tail(path: &str, max_chars: usize) -> String {
    let limit = max_chars.max(10);
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= limit {
        return path.to_owned();
    }
    let tail: String = chars[chars.len() - (limit - 1)..].iter().collect();
    format!("…{tail}")
}

/// Responsive borderless warm-terracotta welcome header adapting from phone (32+ cols)
/// to ultrawide desktop; zero duplication with composer prefixes or footer status.
pub fn render_welcome_card(repo_path: &str, env_summary: &str, width: usize) -> Vec<Line<'static>> {
    let compact = width < 56;
    let indent = if compact { " " } else { "  " };
    let sub_indent = if compact { "  " } else { "    " };
    let mut lines = vec![Line::from("")];
    if compact {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{indent}✻ "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "twig",
                Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(MUTED),
            ),
        ]));
        let path_w = width.saturating_sub(sub_indent.len() + 1);
        lines.push(Line::from(vec![
            Span::raw(sub_indent),
            Span::styled(clip_path_tail(repo_path, path_w), Style::default().fg(STONE)),
        ]));
    } else {
        let ver = format!(" {}  ", env!("CARGO_PKG_VERSION"));
        let path_w = width.saturating_sub(indent.len() + 6 + ver.len() + 1);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{indent}✻ "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "twig",
                Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
            ),
            Span::styled(ver, Style::default().fg(MUTED)),
            Span::styled(clip_path_tail(repo_path, path_w), Style::default().fg(STONE)),
        ]));
    }
    if !env_summary.is_empty() {
        let wrap_w = width.saturating_sub(sub_indent.len() + 1).max(16);
        for seg in wrap_plain(env_summary, wrap_w) {
            lines.push(Line::from(vec![
                Span::raw(sub_indent),
                Span::styled(seg, Style::default().fg(MUTED)),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines
}

/// Responsive shortcut reference rows for the `?` footer overlay (2-col desktop, 1-col phone).
pub fn shortcut_overlay_lines(width: usize) -> Vec<Line<'static>> {
    if width < 76 {
        let rows = [
            ("Enter · S-Tab", "send/steer · plan"),
            ("! @ $ /", "shell·file·skill·cmd"),
            ("^R · ^F · ^O", "hist · find · tools"),
            ("^G · Alt+Y/V", "editor · copy · img"),
            ("Esc×2 · ^C", "rewind/clear · quit"),
        ];
        let w = width.saturating_sub(2).max(16);
        return rows
            .into_iter()
            .map(|(k, d)| {
                let k_w = k.chars().count();
                let rem = w.saturating_sub(k_w + 2);
                Line::from(vec![
                    Span::styled(
                        format!(" {k} "),
                        Style::default()
                            .fg(ACCENT_PEAK)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(clip_str(d, rem), Style::default().fg(STONE)),
                ])
            })
            .collect();
    }
    let row = |k1: &str, d1: &str, k2: &str, d2: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("  {k1:<14}"),
                Style::default()
                    .fg(ACCENT_PEAK)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{d1:<25}"), Style::default().fg(STONE)),
            Span::styled(
                format!("{k2:<14}"),
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
            "Shift+Tab",
            "cycle edit / plan / gate",
            "! · # · @ · /",
            "bash · mem · file · cmd",
        ),
        row(
            "Ctrl+O / Ctrl+T",
            "fold tools / tasks drawer",
            "Ctrl+B / Ctrl+F",
            "detach bg / search chat",
        ),
        row(
            "Ctrl+G",
            "compose in $EDITOR",
            "Alt+Y / +Shift",
            "copy code / full reply",
        ),
        Line::from(Span::styled(
            "  Esc interrupt / ×2 clear or rewind · ^R history · Alt+V paste img · Alt+M mouse · ^C quit",
            Style::default().fg(MUTED),
        )),
    ]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FooterMode {
    Contextual,
    ShortcutOverlay,
    QuitReminder,
    EscHint,
    RewindHint,
}

pub struct FooterView<'a> {
    pub mode: FooterMode,
    pub flash: Option<&'a str>,
    pub repo_label: &'a str,
    pub git_branch: Option<&'a str>,
    pub git_stat: Option<(usize, usize)>,
    pub model: &'a str,
    pub effort: Option<&'a str>,
    pub title: Option<&'a str>,
    pub sid: &'a str,
    pub plan_mode: bool,
    pub gate_edits: bool,
    pub operator_mode: bool,
    pub pending_images: usize,
    pub running_tasks: usize,
    pub context_pct: Option<usize>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub playbook: Option<&'a str>,
    pub guard_level: Option<&'a str>,
    pub mouse_enabled: bool,
    pub right_hint: &'a str,
}

pub fn clip_str(s: &str, max_chars: usize) -> String {
    let limit = max_chars.max(8);
    let clipped: String = s.chars().take(limit).collect();
    if s.chars().count() > limit {
        format!("{clipped}…")
    } else {
        clipped
    }
}

pub fn fmt_tokens_compact(n: u64) -> String {
    if n < 1_000 {
        format!("{n} tok")
    } else if n < 1_000_000 {
        format!("{:.1}k tok", n as f64 / 1_000.0)
    } else {
        format!("{:.2}M tok", n as f64 / 1_000_000.0)
    }
}

pub fn format_status_notice(view: &FooterView<'_>, turns_used: u64, ctx_bytes: usize, ctx_max: usize) -> String {
    let ctx_pct = view
        .context_pct
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "—".into());
    let total_tok = view.input_tokens + view.output_tokens;
    let cache_pct = (view.cached_tokens * 100)
        .checked_div(view.input_tokens)
        .map(|p| format!(" · cache {p}%"))
        .unwrap_or_default();
    let title = view
        .title
        .map(|t| format!(" · title:{t}"))
        .unwrap_or_default();
    let effort = view.effort.unwrap_or("off");
    let mode = match (view.plan_mode, view.operator_mode) {
        (true, true) => "plan+op (RO)",
        (true, false) => "plan (RO)",
        (false, true) => "edit+op",
        (false, false) => "edit",
    };
    format!(
        "sid:{} · model:{} · effort:{effort} · mode:{mode} · turns:{turns_used} · ctx:{ctx_pct} ({ctx_bytes}/{ctx_max}B) · {}{cache_pct}{title}",
        if view.sid.is_empty() { "—" } else { view.sid },
        view.model,
        fmt_tokens_compact(total_tok)
    )
}

/// Borderless responsive footer adapting from phone (32+ cols) to ultrawide desktop.
pub fn render_footer(frame: &mut Frame, area: Rect, view: &FooterView<'_>) {
    match view.mode {
        FooterMode::ShortcutOverlay => {
            frame.render_widget(
                Paragraph::new(shortcut_overlay_lines(area.width as usize)),
                area,
            );
            return;
        }
        FooterMode::QuitReminder => {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "  Press Ctrl+C again to quit",
                    Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                )),
                area,
            );
            return;
        }
        FooterMode::EscHint => {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "  Press Esc again to clear draft",
                    Style::default().fg(AMBER),
                )),
                area,
            );
            return;
        }
        FooterMode::RewindHint => {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "  Press Esc again to rewind a previous turn",
                    Style::default().fg(AMBER),
                )),
                area,
            );
            return;
        }
        FooterMode::Contextual => {}
    }

    let width = area.width as usize;
    let compact = width < 58;
    let narrow = width < 88;
    let pad_x = if width >= 120 {
        4
    } else if width >= 80 {
        3
    } else if !compact {
        2
    } else {
        1
    };
    let pad_str = " ".repeat(pad_x);

    let left: Line<'static> = if let Some(msg) = view.flash {
        Line::from(Span::styled(
            format!("{pad_str}{msg}"),
            Style::default().fg(AMBER),
        ))
    } else {
        let mut spans = vec![
            Span::styled(
                format!("{pad_str}✻ "),
                Style::default()
                    .fg(if view.plan_mode { AMBER } else { ACCENT })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if compact {
                    clip_str(view.repo_label, 12)
                } else {
                    view.repo_label.to_owned()
                },
                Style::default().fg(STONE),
            ),
        ];
        if !compact {
            if let Some(branch) = view.git_branch {
                spans.push(Span::styled(
                    format!(" ({branch})"),
                    Style::default().fg(MUTED),
                ));
            }
            if !narrow {
                if let Some((ins, del)) = view.git_stat {
                    if ins > 0 {
                        spans.push(Span::styled(format!(" +{ins}"), Style::default().fg(SAGE)));
                    }
                    if del > 0 {
                        spans.push(Span::styled(format!(" -{del}"), Style::default().fg(ROSE)));
                    }
                }
            }
        }
        spans.push(Span::styled(
            format!(
                " · {}",
                if compact {
                    clip_str(view.model, 14)
                } else {
                    view.model.to_owned()
                }
            ),
            Style::default().fg(ACCENT_PEAK),
        ));
        if !compact {
            if let Some(effort) = view.effort {
                spans.push(Span::styled(
                    format!(" {effort}"),
                    Style::default().fg(if matches!(effort, "high" | "xhigh") {
                        AMBER
                    } else {
                        STONE
                    }),
                ));
            }
        }
        if view.plan_mode {
            spans.push(Span::styled(
                if compact { " · ◈" } else { " · ◈ plan" },
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ));
        } else if view.gate_edits {
            spans.push(Span::styled(
                if compact { " · 🔒" } else { " · 🔒 review-edits" },
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ));
        }
        if view.operator_mode {
            spans.push(Span::styled(
                if compact { " · ⬡" } else { " · ⬡ op" },
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        }
        if view.running_tasks > 0 {
            spans.push(Span::styled(
                if compact {
                    format!(" · ⚙{}", view.running_tasks)
                } else {
                    format!(
                        " · ⚙ {} task{}",
                        view.running_tasks,
                        if view.running_tasks == 1 { "" } else { "s" }
                    )
                },
                Style::default().fg(AMBER),
            ));
        }
        if view.pending_images > 0 {
            spans.push(Span::styled(
                format!(" · 📎 {}", view.pending_images),
                Style::default().fg(ACCENT),
            ));
        }
        if !compact {
            if let Some(title) = view.title {
                spans.push(Span::styled(
                    format!(" · {}", clip_str(title, 20)),
                    Style::default().fg(IVORY),
                ));
            }
        }
        if let Some(pct) = view.context_pct.filter(|&p| p > 0) {
            let color = if pct >= 85 {
                ROSE
            } else if pct >= 65 {
                AMBER
            } else {
                MUTED
            };
            spans.push(Span::styled(
                if compact {
                    format!(" · {pct}%")
                } else {
                    format!(" · ctx {pct}%")
                },
                Style::default().fg(color),
            ));
        }
        if !narrow {
            let total_tok = view.input_tokens + view.output_tokens;
            if total_tok > 0 {
                spans.push(Span::styled(
                    format!(" · {}", fmt_tokens_compact(total_tok)),
                    Style::default().fg(MUTED),
                ));
                if view.cached_tokens > 0 {
                    if let Some(hit) = (view.cached_tokens * 100).checked_div(view.input_tokens) {
                        spans.push(Span::styled(
                            format!(" · cache {hit}%"),
                            Style::default().fg(SAGE),
                        ));
                    }
                }
            }
            if let Some(book) = view.playbook {
                spans.push(Span::styled(
                    format!(" · ✦ {book}"),
                    Style::default().fg(AMBER),
                ));
            }
        }
        if let Some(level) = view.guard_level {
            spans.push(Span::styled(
                format!(" · ▲ {level}"),
                Style::default().fg(ROSE),
            ));
        }
        if !view.mouse_enabled && !compact {
            spans.push(Span::styled(" · select", Style::default().fg(AMBER)));
        }
        Line::from(spans)
    };

    if compact {
        frame.render_widget(Paragraph::new(left), area);
        return;
    }
    let hint_raw = if narrow {
        "? help · ^R hist"
    } else {
        view.right_hint.trim_end()
    };
    let hint = format!("{hint_raw}{pad_str}");
    let right_w = (display_width(&hint) as u16).min(area.width / 2);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(right_w)])
        .split(area);
    frame.render_widget(Paragraph::new(left), cols[0]);
    frame.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(MUTED))),
        cols[1],
    );
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
        let mid = scroll.current_offset();
        assert!(mid < 40 && mid > 20, "expected intermediate ease, got {mid}");

        scroll.step(0.016, MotionMode::Reduced);
        assert_eq!(scroll.current_offset(), 20);
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
        assert!(rendered.iter().any(|l| l.to_string().contains("diff")));
        assert!(rendered.iter().any(|l| l.to_string().contains("10 - old")));
        assert!(rendered.iter().any(|l| l.to_string().contains("10 + new")));

        let blocks = extract_code_blocks(md);
        assert_eq!(blocks, vec!["@@ -10,2 +10,2 @@\n-old\n+new", "let x = 1;"]);
    }

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
    }
}
