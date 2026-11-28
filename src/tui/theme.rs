//! Canonical TUI design system, motion primitives, cell-width layout helpers,
//! tool presentation grammar, and borderless welcome/footer chrome for `twig chat`:
//! - Single Source of Truth for the Claude warm-terracotta palette (`#D77757`) and TrueColor tokens
//! - OpenAI Codex (`codex-rs/tui`) cadenced column-width shimmer and RGB `blend()`
//! - CJK/emoji-safe `display_width`, `horizontal_pad`, `wrap_plain`, and `clip_width`

use std::time::Duration;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const ACCENT_RGB: (u8, u8, u8) = (215, 119, 87); // #D77757 Claude terracotta
const ACCENT_DIM_RGB: (u8, u8, u8) = (158, 92, 70); // deep terracotta breath trough
const ACCENT_PEAK_RGB: (u8, u8, u8) = (248, 206, 178); // #F8CEB2 luminous shimmer peak
const BASE_SHIMMER_RGB: (u8, u8, u8) = (162, 124, 106); // warm stone-terracotta shimmer base
const AMBER_RGB: (u8, u8, u8) = (232, 177, 98); // #E8B162 steer / approval / warn
const SAGE_RGB: (u8, u8, u8) = (128, 192, 138); // #80C08A success / diff +
const ROSE_RGB: (u8, u8, u8) = (224, 110, 118); // #E06E76 error / diff -
const IVORY_RGB: (u8, u8, u8) = (242, 238, 232); // #F2EEE8 crisp primary text
const STONE_RGB: (u8, u8, u8) = (200, 193, 184); // #C8C1B8 high-contrast secondary text
const MUTED_RGB: (u8, u8, u8) = (168, 160, 150); // #A8A096 legible hints over wallpapers
const HAIRLINE_RGB: (u8, u8, u8) = (128, 119, 110); // #80776E crisp table & rule borders
const HAIRLINE_FOCUS_RGB: (u8, u8, u8) = (154, 122, 104); // warm active hairline
const SURFACE_RGB: (u8, u8, u8) = (38, 35, 32); // #262320 warm subtle surface
const SURFACE_HI_RGB: (u8, u8, u8) = (56, 49, 44); // #38312C selected row highlight

pub(super) const DARK_TC_ADD_LINE_BG: Color = Color::Rgb(33, 58, 43);
pub(super) const DARK_TC_DEL_LINE_BG: Color = Color::Rgb(74, 34, 29);
pub(super) const DARK_TC_ADD_WORD_BG: Color = Color::Rgb(46, 108, 66);
pub(super) const DARK_TC_DEL_WORD_BG: Color = Color::Rgb(132, 48, 42);
pub(super) const BASH_MODE_COLOR: Color = Color::Rgb(110, 198, 216);
pub(super) const MEMORY_MODE_COLOR: Color = Color::Rgb(186, 148, 236);
pub(super) const ON_ACCENT_FG: Color = Color::Rgb(22, 19, 17);
pub(super) const SUBAGENT_BANNER_BG: Color = Color::Rgb(28, 36, 40);
pub(super) const SEL_BG: Color = Color::Rgb(228, 209, 167);
pub(super) const SEL_FG: Color = Color::Rgb(28, 32, 34);

pub(super) const ACCENT: Color = Color::Rgb(ACCENT_RGB.0, ACCENT_RGB.1, ACCENT_RGB.2);
pub(super) const ACCENT_PEAK: Color =
    Color::Rgb(ACCENT_PEAK_RGB.0, ACCENT_PEAK_RGB.1, ACCENT_PEAK_RGB.2);
pub(super) const AMBER: Color = Color::Rgb(AMBER_RGB.0, AMBER_RGB.1, AMBER_RGB.2);
pub(super) const SAGE: Color = Color::Rgb(SAGE_RGB.0, SAGE_RGB.1, SAGE_RGB.2);
pub(super) const ROSE: Color = Color::Rgb(ROSE_RGB.0, ROSE_RGB.1, ROSE_RGB.2);
pub(super) const IVORY: Color = Color::Rgb(IVORY_RGB.0, IVORY_RGB.1, IVORY_RGB.2);
pub(super) const STONE: Color = Color::Rgb(STONE_RGB.0, STONE_RGB.1, STONE_RGB.2);
pub(super) const MUTED: Color = Color::Rgb(MUTED_RGB.0, MUTED_RGB.1, MUTED_RGB.2);
pub(super) const HAIRLINE: Color = Color::Rgb(HAIRLINE_RGB.0, HAIRLINE_RGB.1, HAIRLINE_RGB.2);
pub(super) const HAIRLINE_FOCUS: Color =
    Color::Rgb(HAIRLINE_FOCUS_RGB.0, HAIRLINE_FOCUS_RGB.1, HAIRLINE_FOCUS_RGB.2);
pub(super) const SURFACE: Color = Color::Rgb(SURFACE_RGB.0, SURFACE_RGB.1, SURFACE_RGB.2);
pub(super) const SURFACE_HI: Color =
    Color::Rgb(SURFACE_HI_RGB.0, SURFACE_HI_RGB.1, SURFACE_HI_RGB.2);
pub(super) const SEARCH_BG: Color = Color::Rgb(92, 66, 32);
pub(super) const SEARCH_ACTIVE_BG: Color = ACCENT;

const STAR_FRAMES: &[&str] = &["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];
const SPINNER_DELAY: Duration = Duration::from_millis(200);
const SPINNER_FRAME_MS: u128 = 105;

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

fn blend(fg: (u8, u8, u8), bg: (u8, u8, u8), alpha: f32) -> (u8, u8, u8) {
    let a = alpha.clamp(0.0, 1.0);
    let r = (fg.0 as f32 * a + bg.0 as f32 * (1.0 - a)).round() as u8;
    let g = (fg.1 as f32 * a + bg.1 as f32 * (1.0 - a)).round() as u8;
    let b = (fg.2 as f32 * a + bg.2 as f32 * (1.0 - a)).round() as u8;
    (r, g, b)
}

/// Claude floral star with continuous sinusoidal warm-terracotta luminance breathing.
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

/// Returns `true` during the active 1s shimmer sweep window (for 60fps adaptive pacing).
pub fn shimmer_is_sweeping(elapsed: Duration, motion: MotionMode) -> bool {
    if motion == MotionMode::Reduced || elapsed < SHIMMER_START_DELAY {
        return false;
    }
    let cycle = elapsed.saturating_sub(SHIMMER_START_DELAY).as_secs_f64() % SHIMMER_INTERVAL_SECS;
    cycle <= SHIMMER_SWEEP_SECS + 0.08
}

/// Ported from `codex-rs/tui/src/summary_shimmer.rs` with Claude warm terracotta glow:
/// 600ms delay -> 1.0s smooth display-column cosine sweep every 4.0s.
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

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Canonical responsive horizontal outer margin across transcript, composer, strips, and footer.
pub fn horizontal_pad(width: usize) -> usize {
    if width >= 120 {
        4
    } else if width >= 80 {
        3
    } else if width >= 56 {
        2
    } else {
        1
    }
}

/// Truncate `s` by terminal display column width (CJK/emoji safe), appending `…` when clipped.
pub fn clip_width(s: &str, max_cols: usize) -> String {
    let limit = max_cols.max(4);
    if display_width(s) <= limit {
        return s.to_owned();
    }
    let budget = limit.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
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

/// Semantic crisp Unicode icon per tool (consistent width across all terminals).
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

pub fn title_tool_name(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => "Tool".into(),
    }
}

pub fn summarize_tool_args(name: &str, args: &Value) -> String {
    if name == "todo" {
        let n = args["items"].as_array().map_or(0, Vec::len);
        return format!("{n} items");
    }
    if name == "mcp" {
        let srv = args["server"].as_str().unwrap_or("");
        let action = args["action"].as_str().unwrap_or("call");
        let target = args["prompt"]
            .as_str()
            .or_else(|| args["uri"].as_str())
            .or_else(|| args["tool"].as_str())
            .unwrap_or(action);
        return clip_width(&format!("{srv}/{target}"), 92);
    }
    if name == "task" {
        let action = args["action"].as_str().unwrap_or("list");
        let detail = args["command"]
            .as_str()
            .or_else(|| args["id"].as_str())
            .unwrap_or("");
        let summary = if detail.is_empty() {
            action.to_owned()
        } else {
            format!("{action} {detail}")
        };
        return clip_width(&summary.replace('\n', " "), 92);
    }
    let raw = match name {
        "bash" => args["command"].as_str().unwrap_or(""),
        "read" | "edit" | "write" => args["path"].as_str().unwrap_or(""),
        "outline" => args["path"].as_str().unwrap_or("."),
        "apply_patch" => "patch",
        "grep" | "glob" => args["pattern"].as_str().unwrap_or(""),
        "skill" => args["name"].as_str().unwrap_or("list"),
        "subagent" => args["task"].as_str().unwrap_or(""),
        "papercut" => args["component"].as_str().unwrap_or(""),
        _ => "",
    };
    clip_width(&raw.replace('\n', " "), 92)
}

fn clip_path_tail(path: &str, max_cols: usize) -> String {
    let limit = max_cols.max(10);
    if display_width(path) <= limit {
        return path.to_owned();
    }
    let budget = limit.saturating_sub(1);
    let mut rev_chars = Vec::new();
    let mut used = 0usize;
    for ch in path.chars().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        rev_chars.push(ch);
        used += w;
    }
    rev_chars.reverse();
    let tail: String = rev_chars.into_iter().collect();
    format!("…{tail}")
}

/// Responsive borderless warm-terracotta welcome header adapting from phone (32+ cols) to ultrawide.
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

fn shortcut_overlay_lines(width: usize) -> Vec<Line<'static>> {
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
                let k_w = display_width(k);
                let rem = w.saturating_sub(k_w + 2);
                Line::from(vec![
                    Span::styled(
                        format!(" {k} "),
                        Style::default()
                            .fg(ACCENT_PEAK)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(clip_width(d, rem), Style::default().fg(STONE)),
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
            "  Esc interrupt / ×2 clear · ^R history · Shift+drag or Alt+M select text · ^C quit",
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

fn fmt_tokens_compact(n: u64) -> String {
    if n < 1_000 {
        format!("{n} tok")
    } else if n < 1_000_000 {
        format!("{:.1}k tok", n as f64 / 1_000.0)
    } else {
        format!("{:.2}M tok", n as f64 / 1_000_000.0)
    }
}

pub fn format_status_notice(
    view: &FooterView<'_>,
    turns_used: u64,
    ctx_bytes: usize,
    ctx_max: usize,
) -> String {
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
    let pad_x = horizontal_pad(width);
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
                    clip_width(view.repo_label, 12)
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
                    clip_width(view.model, 14)
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
                    format!(" · {}", clip_width(title, 20)),
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
    fn wraps_and_clips_cjk_by_terminal_display_width() {
        let lines = wrap_plain("你好世界测试", 8);
        assert_eq!(lines, vec!["你好世界", "测试"]);
        assert_eq!(display_width("你好"), 4);

        let clipped = clip_width("你好世界测试文本", 9);
        assert!(display_width(&clipped) <= 9, "clipped={clipped}");
        assert_eq!(clipped, "你好世界…");
        assert_eq!(clip_width("short", 10), "short");
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
}
