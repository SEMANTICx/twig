//! Deep transcript module owning conversation cells, history replay, smooth scrolling,
//! tool tree rendering, inline `Ctrl+F` search, and `Alt+Y` / `Alt+Shift+Y` clipboard extraction.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;
use serde_json::Value;

use super::highlight::{highlight_search, style_output_line, DiffGutterState};
use super::markdown::{extract_code_blocks, render_markdown};
use super::theme::{
    breathing_star, clip_width, display_width, fmt_elapsed_compact, horizontal_pad,
    render_welcome_card, streaming_caret, summarize_tool_args, title_tool_name, tool_glyph,
    wrap_plain, MotionMode, ACCENT, AMBER, IVORY, MUTED, ON_ACCENT_FG, ROSE, SAGE, STONE, SURFACE,
};

struct SmoothScroll {
    pos: f64,
    target: f64,
    max: usize,
    auto_follow: bool,
}

impl SmoothScroll {
    fn new() -> Self {
        Self {
            pos: 0.0,
            target: 0.0,
            max: 0,
            auto_follow: true,
        }
    }

    fn reset(&mut self) {
        self.pos = 0.0;
        self.target = 0.0;
        self.max = 0;
        self.auto_follow = true;
    }

    fn follow_tail(&mut self) {
        self.auto_follow = true;
        self.target = self.max as f64;
    }

    fn scroll_by(&mut self, delta: i32) {
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

    fn jump_to(&mut self, line: usize) {
        self.auto_follow = false;
        self.target = (line as f64).min(self.max as f64);
    }

    fn sync_bounds(&mut self, max_scroll: usize, motion: MotionMode) {
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

    fn step(&mut self, dt_secs: f32, motion: MotionMode) {
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

    fn is_animating(&self, motion: MotionMode) -> bool {
        motion == MotionMode::Animated && (self.target - self.pos).abs() >= 0.25
    }

    fn current_offset(&self) -> usize {
        self.pos.round().clamp(0.0, self.max as f64) as usize
    }

    fn show_tail_badge(&self) -> bool {
        !self.auto_follow && self.current_offset() < self.max
    }
}

pub struct RenderOpts<'a> {
    pub repo_path: &'a str,
    pub env_summary: &'a str,
    pub live_streaming: bool,
    pub run_elapsed: Duration,
    pub dt: f32,
    pub motion: MotionMode,
}

#[derive(Debug, Clone, Copy)]
pub enum NoticeKind {
    Playbook,
    Info,
    Warn,
    Error,
    Success,
}

#[derive(Debug, Clone)]
struct ToolCell {
    id: String,
    name: String,
    summary: String,
    tier: String,
    output: String,
    started_at: Instant,
    elapsed: Option<Duration>,
    exit_code: Option<i32>,
    finished: bool,
}

#[derive(Debug, Clone)]
enum Cell {
    User(String),
    Steering(String),
    Reasoning {
        text: String,
        started_at: Instant,
        elapsed: Duration,
    },
    Assistant(String),
    Tool(ToolCell),
    Notice { kind: NoticeKind, text: String },
}

pub struct Transcript {
    cells: Vec<Cell>,
    scroll: SmoothScroll,
    expand_tools: bool,
    search_open: bool,
    search_query: String,
    search_match_idx: usize,
    search_matches: Vec<usize>,
    copy_cycle: usize,
    last_copy_at: Option<Instant>,
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            scroll: SmoothScroll::new(),
            expand_tools: false,
            search_open: false,
            search_query: String::new(),
            search_match_idx: 0,
            search_matches: Vec::new(),
            copy_cycle: 0,
            last_copy_at: None,
        }
    }

    pub fn clear(&mut self) {
        self.cells.clear();
        self.scroll.reset();
        self.search_matches.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn push_user(&mut self, text: String) {
        self.cells.push(Cell::User(text));
        self.scroll.follow_tail();
    }

    pub fn push_steering(&mut self, text: String) {
        self.cells.push(Cell::Steering(text));
        self.scroll.follow_tail();
    }

    fn seal_active_reasoning(&mut self) {
        if let Some(Cell::Reasoning {
            started_at,
            elapsed,
            ..
        }) = self.cells.last_mut()
        {
            let live = started_at.elapsed();
            if live > *elapsed {
                *elapsed = live;
            }
        }
    }

    pub fn push_assistant(&mut self, text: String) {
        self.seal_active_reasoning();
        self.cells.push(Cell::Assistant(text));
    }

    pub fn append_assistant_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(Cell::Assistant(buf)) = self.cells.last_mut() {
            buf.push_str(delta);
        } else {
            self.seal_active_reasoning();
            self.cells.push(Cell::Assistant(delta.to_owned()));
        }
    }

    pub fn append_reasoning_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(Cell::Reasoning {
            text,
            started_at,
            elapsed,
        }) = self.cells.last_mut()
        {
            text.push_str(delta);
            *elapsed = started_at.elapsed();
        } else {
            self.cells.push(Cell::Reasoning {
                text: delta.to_owned(),
                started_at: Instant::now(),
                elapsed: Duration::ZERO,
            });
        }
    }

    pub fn push_tool_started(&mut self, id: String, name: String, summary: String, tier: String) {
        self.seal_active_reasoning();
        self.cells.push(Cell::Tool(ToolCell {
            id,
            name,
            summary,
            tier,
            output: String::new(),
            started_at: Instant::now(),
            elapsed: None,
            exit_code: None,
            finished: false,
        }));
    }

    pub fn push_tool_snapshot(
        &mut self,
        id: String,
        name: String,
        summary: String,
        tier: String,
        output: String,
        exit_code: Option<i32>,
    ) {
        self.cells.push(Cell::Tool(ToolCell {
            id,
            name,
            summary,
            tier,
            output,
            started_at: Instant::now(),
            elapsed: Some(Duration::ZERO),
            exit_code,
            finished: true,
        }));
    }

    pub fn append_tool_output(&mut self, id: &str, chunk: &str) {
        if let Some(Cell::Tool(tool)) = self
            .cells
            .iter_mut()
            .rev()
            .find(|c| matches!(c, Cell::Tool(t) if t.id == id))
        {
            if tool.output.len() < 262_144 {
                tool.output.push_str(chunk);
            }
        }
    }

    pub fn finish_tool(
        &mut self,
        id: &str,
        output: Option<String>,
        exit_code: Option<i32>,
        elapsed: Option<Duration>,
    ) {
        if let Some(Cell::Tool(tool)) = self
            .cells
            .iter_mut()
            .rev()
            .find(|c| matches!(c, Cell::Tool(t) if t.id == id))
        {
            if let Some(out) = output {
                tool.output = out;
            }
            tool.finished = true;
            tool.elapsed = Some(elapsed.unwrap_or_else(|| tool.started_at.elapsed()));
            tool.exit_code = exit_code;
        }
    }

    pub fn push_notice(&mut self, kind: NoticeKind, text: impl Into<String>) {
        self.cells.push(Cell::Notice {
            kind,
            text: text.into(),
        });
    }

    pub fn active_tool_name(&self) -> Option<&str> {
        self.cells.iter().rev().find_map(|c| match c {
            Cell::Tool(t) if !t.finished => Some(t.name.as_str()),
            _ => None,
        })
    }

    pub fn toggle_expand(&mut self) -> bool {
        self.expand_tools = !self.expand_tools;
        self.expand_tools
    }

    pub fn is_expanded(&self) -> bool {
        self.expand_tools
    }

    pub fn scroll_by(&mut self, delta: i32) {
        self.scroll.scroll_by(delta);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll.jump_to(0);
    }

    pub fn follow_tail(&mut self) {
        self.scroll.follow_tail();
    }

    pub fn is_scroll_animating(&self, motion: MotionMode) -> bool {
        self.scroll.is_animating(motion)
    }

    /// Chronological list of user turn prompts in the current transcript (for `/rewind` picker).
    pub fn user_turns(&self) -> Vec<String> {
        self.cells
            .iter()
            .filter_map(|c| match c {
                Cell::User(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn replay_history(&mut self, messages: &[Value]) {
        if !self.cells.is_empty() || messages.is_empty() {
            return;
        }
        let mut tool_outputs: HashMap<String, String> = HashMap::new();
        for msg in messages {
            if msg["role"] == "tool" {
                if let Some(id) = msg["tool_call_id"].as_str() {
                    tool_outputs.insert(
                        id.to_owned(),
                        msg["content"].as_str().unwrap_or("").to_owned(),
                    );
                }
            }
        }
        for msg in messages {
            match msg["role"].as_str() {
                Some("user") => {
                    if let Some(content) = msg["content"].as_str() {
                        if !content.trim().is_empty() {
                            self.cells.push(Cell::User(content.to_owned()));
                        }
                    } else if let Some(parts) = msg["content"].as_array() {
                        let mut text_parts = Vec::new();
                        let mut img_count = 0usize;
                        for part in parts {
                            match part["type"].as_str() {
                                Some("text") => {
                                    if let Some(t) = part["text"].as_str() {
                                        if !t.trim().is_empty() {
                                            text_parts.push(t.to_owned());
                                        }
                                    }
                                }
                                Some("image_url") => img_count += 1,
                                _ => {}
                            }
                        }
                        if img_count > 0 {
                            text_parts.push(format!("[Image ×{img_count}]"));
                        }
                        let joined = text_parts.join(" ");
                        if !joined.trim().is_empty() {
                            self.cells.push(Cell::User(joined));
                        }
                    }
                }
                Some("assistant") => {
                    if let Some(content) = msg["content"].as_str() {
                        if !content.trim().is_empty() {
                            self.cells.push(Cell::Assistant(content.to_owned()));
                        }
                    }
                    if let Some(calls) = msg["tool_calls"].as_array() {
                        for call in calls {
                            let id = call["id"].as_str().unwrap_or("").to_owned();
                            let name = call["function"]["name"]
                                .as_str()
                                .unwrap_or("tool")
                                .to_owned();
                            let args_val = call["function"]["arguments"]
                                .as_str()
                                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                                .unwrap_or(Value::Null);
                            let output = tool_outputs.remove(&id).unwrap_or_default();
                            self.cells.push(Cell::Tool(ToolCell {
                                id,
                                summary: summarize_tool_args(&name, &args_val),
                                name,
                                tier: "auto".into(),
                                output,
                                started_at: Instant::now(),
                                elapsed: Some(Duration::ZERO),
                                exit_code: Some(0),
                                finished: true,
                            }));
                        }
                    }
                }
                _ => {}
            }
        }
        self.scroll.follow_tail();
    }

    pub fn copy_latest_code_block(&mut self) -> Result<(String, String), String> {
        let mut all_blocks = Vec::new();
        for cell in &self.cells {
            if let Cell::Assistant(text) = cell {
                all_blocks.extend(extract_code_blocks(text));
            }
        }
        if all_blocks.is_empty() {
            return Err("No assistant code blocks to copy".into());
        }
        let within_cycle = self
            .last_copy_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(8));
        if within_cycle {
            self.copy_cycle = (self.copy_cycle + 1) % all_blocks.len();
        } else {
            self.copy_cycle = 0;
        }
        self.last_copy_at = Some(Instant::now());
        let idx = all_blocks.len() - 1 - self.copy_cycle;
        let block = all_blocks[idx].clone();
        let line_n = block.lines().count();
        let label = format!(
            "Copied code block #{}/{} ({line_n} lines)",
            self.copy_cycle + 1,
            all_blocks.len()
        );
        Ok((block, label))
    }

    pub fn copy_latest_reply(&self) -> Result<(String, String), String> {
        let Some(text) = self.cells.iter().rev().find_map(|c| match c {
            Cell::Assistant(t) if !t.trim().is_empty() => Some(t.clone()),
            _ => None,
        }) else {
            return Err("No assistant reply to copy".into());
        };
        let label = format!("Copied full reply ({} chars)", text.chars().count());
        Ok((text, label))
    }

    pub fn search_is_open(&self) -> bool {
        self.search_open
    }

    #[cfg(test)]
    pub fn search_match_count(&self) -> usize {
        self.search_matches.len()
    }

    pub fn search_toggle(&mut self) {
        self.search_open = !self.search_open;
        self.search_match_idx = 0;
        if !self.search_open {
            self.search_query.clear();
            self.search_matches.clear();
        }
    }

    pub fn search_open_with(&mut self, query: &str) {
        self.search_open = true;
        self.search_query = query.to_owned();
        self.search_match_idx = 0;
    }

    pub fn search_close(&mut self) {
        self.search_open = false;
        self.search_query.clear();
        self.search_matches.clear();
    }

    pub fn search_paste(&mut self, pasted: &str) {
        self.search_query
            .push_str(&pasted.replace(['\r', '\n'], " "));
        self.search_match_idx = 0;
    }

    pub fn search_handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.search_close(),
            KeyCode::Backspace => {
                self.search_query.pop();
                self.search_match_idx = 0;
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if !self.search_matches.is_empty() {
                    self.search_match_idx = (self.search_match_idx + self.search_matches.len() - 1)
                        % self.search_matches.len();
                    self.jump_to_match();
                }
            }
            KeyCode::Enter => {
                if !self.search_matches.is_empty() {
                    self.search_match_idx = (self.search_match_idx + 1) % self.search_matches.len();
                    self.jump_to_match();
                }
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.search_match_idx = 0;
            }
            _ => {}
        }
    }

    fn jump_to_match(&mut self) {
        if let Some(&line_no) = self.search_matches.get(self.search_match_idx) {
            self.scroll.jump_to(line_no.saturating_sub(3));
        }
    }

    fn build_lines(
        &self,
        width: usize,
        repo_path: &str,
        env_summary: &str,
        live_streaming: bool,
        run_elapsed: Duration,
        motion: MotionMode,
    ) -> Vec<Line<'static>> {
        if self.cells.is_empty() {
            return render_welcome_card(repo_path, env_summary, width);
        }
        let mut lines: Vec<Line<'static>> = Vec::new();
        let pad_x = horizontal_pad(width);
        let left_margin = " ".repeat(pad_x);
        let inner = width.saturating_sub(pad_x * 2).max(14);
        let prose_w = inner.saturating_sub(2).max(14);
        let last_idx = self.cells.len().saturating_sub(1);

        let mut cell_idx = 0usize;
        while cell_idx < self.cells.len() {
            // In default result-focused mode (`!self.expand_tools`), fold finished non-local tool
            // runs, intermediate reasoning, and routine process notices into a single calm trace
            // line so only Requests, Results, and currently-running tools stay prominent.
            // Pressing `Ctrl+O` toggles full verbose tool output expansion.
            if !self.expand_tools {
                // Active `subagent` calls are already rendered live in the dedicated
                // below-composer Subagents strip, so skip duplicating them in the transcript.
                if matches!(
                    &self.cells[cell_idx],
                    Cell::Tool(t) if t.name == "subagent" && !t.finished
                ) {
                    cell_idx += 1;
                    continue;
                }
                if let Some((end_idx, summary_line)) =
                    self.try_fold_turn_trace(cell_idx, last_idx, &left_margin, inner)
                {
                    if !lines.is_empty()
                        && !lines
                            .last()
                            .is_some_and(|l| l.spans.is_empty() || l.to_string().trim().is_empty())
                    {
                        lines.push(Line::from(""));
                    }
                    lines.push(summary_line);
                    lines.push(Line::from(""));
                    cell_idx = end_idx;
                    continue;
                }
            }
            let cell = &self.cells[cell_idx];
            let prev_non_empty = !lines.is_empty()
                && !lines
                    .last()
                    .is_some_and(|l| l.spans.is_empty() || l.to_string().trim().is_empty());
            match cell {
                Cell::User(text) => {
                    if prev_non_empty {
                        lines.push(Line::from(""));
                    }
                    let comfy = width >= 56;
                    let card_w = inner;
                    let content_w = card_w.saturating_sub(6).max(12);
                    let bar_style = Style::default().fg(ACCENT).bg(SURFACE);
                    let empty_bar = || {
                        Line::from(vec![
                            Span::raw(left_margin.clone()),
                            Span::styled("▎", bar_style),
                            Span::styled(
                                " ".repeat(card_w.saturating_sub(1)),
                                Style::default().bg(SURFACE),
                            ),
                        ])
                    };
                    if comfy {
                        lines.push(empty_bar());
                    }
                    for (idx, wrapped) in wrap_plain(text, content_w).into_iter().enumerate() {
                        let prefix = if idx == 0 { "  ❯ " } else { "    " };
                        let pad = card_w
                            .saturating_sub(1 + display_width(prefix) + display_width(&wrapped));
                        lines.push(Line::from(vec![
                            Span::raw(left_margin.clone()),
                            Span::styled("▎", bar_style),
                            Span::styled(
                                prefix,
                                Style::default()
                                    .fg(ACCENT)
                                    .bg(SURFACE)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(wrapped, Style::default().fg(IVORY).bg(SURFACE)),
                            Span::styled(" ".repeat(pad), Style::default().bg(SURFACE)),
                        ]));
                    }
                    if comfy {
                        lines.push(empty_bar());
                    }
                    lines.push(Line::from(""));
                }
                Cell::Steering(text) => {
                    for (idx, wrapped) in wrap_plain(text, inner.saturating_sub(11))
                        .into_iter()
                        .enumerate()
                    {
                        lines.push(Line::from(vec![
                            Span::raw(left_margin.clone()),
                            Span::styled(
                                if idx == 0 { "↳ steer  " } else { "         " },
                                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(wrapped, Style::default().fg(AMBER)),
                        ]));
                    }
                    lines.push(Line::from(""));
                }
                Cell::Assistant(text) => {
                    // Never render empty/whitespace-only ghost bullets before tool calls.
                    if text.trim().is_empty() && !(cell_idx == last_idx && live_streaming) {
                        cell_idx += 1;
                        continue;
                    }
                    if prev_non_empty {
                        lines.push(Line::from(""));
                    }
                    let mut md_lines = render_markdown(text, prose_w);
                    if cell_idx == last_idx && live_streaming {
                        let caret = streaming_caret(run_elapsed, motion);
                        if let Some(last) = md_lines.last_mut() {
                            last.spans.push(caret);
                        }
                    }
                    for (idx, line) in md_lines.into_iter().enumerate() {
                        let mut spans = vec![
                            Span::raw(left_margin.clone()),
                            if idx == 0 {
                                Span::styled(
                                    "◆ ",
                                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                                )
                            } else {
                                Span::raw("  ")
                            },
                        ];
                        spans.extend(line.spans);
                        lines.push(Line::from(spans));
                    }
                    lines.push(Line::from(""));
                }
                Cell::Tool(tool) => {
                    let prev_is_trace = cell_idx > 0
                        && matches!(
                            self.cells[cell_idx - 1],
                            Cell::Tool(_) | Cell::Reasoning { .. } | Cell::Notice { .. }
                        );
                    if prev_non_empty && !prev_is_trace {
                        lines.push(Line::from(""));
                    }
                    let is_edit_tool = matches!(tool.name.as_str(), "edit" | "write" | "apply_patch");
                    let elapsed_dur = tool.elapsed.unwrap_or_else(|| tool.started_at.elapsed());
                    let icon_span = if !tool.finished {
                        breathing_star(elapsed_dur, motion, true)
                    } else if tool.exit_code.is_none() || tool.exit_code == Some(0) {
                        Span::styled(
                            if is_edit_tool { "●" } else { "◦" },
                            Style::default().fg(SAGE).add_modifier(if is_edit_tool {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                        )
                    } else {
                        Span::styled("✕", Style::default().fg(ROSE).add_modifier(Modifier::BOLD))
                    };

                    let display_title = match tool.name.as_str() {
                        "edit" | "apply_patch" => "Update".to_owned(),
                        other => title_tool_name(other),
                    };
                    let summary_cap = inner.saturating_sub(34).clamp(16, 88);
                    let mut header = vec![
                        Span::raw(left_margin.clone()),
                        icon_span,
                        Span::raw(" "),
                    ];
                    if !is_edit_tool {
                        header.push(Span::styled(
                            format!("{} ", tool_glyph(&tool.name)),
                            Style::default().fg(MUTED),
                        ));
                    }
                    header.push(Span::styled(
                        display_title,
                        Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
                    ));
                    if !tool.summary.is_empty() {
                        header.push(Span::styled("(", Style::default().fg(MUTED)));
                        header.push(Span::styled(
                            clip_width(&tool.summary, summary_cap),
                            Style::default().fg(if is_edit_tool { IVORY } else { STONE }),
                        ));
                        header.push(Span::styled(")", Style::default().fg(MUTED)));
                    }
                    if is_edit_tool && tool.finished && tool.exit_code.unwrap_or(0) == 0 {
                        let (ins, del) = count_diff_changes(&tool.output);
                        if ins > 0 {
                            header.push(Span::styled(
                                format!("  +{ins}"),
                                Style::default().fg(SAGE).add_modifier(Modifier::BOLD),
                            ));
                        }
                        if del > 0 {
                            header.push(Span::styled(
                                format!(" -{del}"),
                                Style::default().fg(ROSE).add_modifier(Modifier::BOLD),
                            ));
                        }
                    } else if tool.tier != "auto" {
                        header.push(Span::styled(
                            format!(" · {}", tool.tier),
                            Style::default().fg(AMBER),
                        ));
                    }
                    let mut meta = format!("  {}", fmt_elapsed_compact(elapsed_dur.as_secs()));
                    if let Some(code) = tool.exit_code.filter(|&c| c != 0) {
                        meta.push_str(&format!(" · exit {code}"));
                    }
                    header.push(Span::styled(meta, Style::default().fg(MUTED)));
                    lines.push(Line::from(header));

                    let raw_lines: Vec<&str> = tool.output.lines().collect();
                    // Strip noisy `diff --git` / `--- a/` / `+++ b/` headers on compact `Update` blocks.
                    let out_lines: Vec<&str> = if is_edit_tool && !self.expand_tools {
                        raw_lines
                            .into_iter()
                            .filter(|l| {
                                !(l.starts_with("diff --git ")
                                    || l.starts_with("--- ")
                                    || l.starts_with("+++ "))
                            })
                            .collect()
                    } else {
                        raw_lines
                    };
                    if !out_lines.is_empty() {
                        let is_diff_like = tool.tier == "local"
                            || matches!(tool.name.as_str(), "diff" | "edit" | "write" | "apply_patch");
                        let show_lines = if self.expand_tools || tool.tier == "local" || tool.name == "diff" {
                            200
                        } else if is_edit_tool {
                            8
                        } else if !tool.finished {
                            5
                        } else if tool.exit_code.is_some_and(|c| c != 0) {
                            6
                        } else {
                            1
                        };
                        let total = out_lines.len();
                        let is_compact_summary = !self.expand_tools
                            && !is_diff_like
                            && tool.finished
                            && tool.exit_code.unwrap_or(0) == 0;

                        if is_compact_summary {
                            let first_preview = out_lines
                                .iter()
                                .find(|l| !l.trim().is_empty())
                                .copied()
                                .unwrap_or("");
                            let clipped =
                                clip_width(first_preview.trim(), inner.saturating_sub(34).min(96));
                            let mut spans = vec![
                                Span::raw(left_margin.clone()),
                                Span::styled("  ⎿  ", Style::default().fg(MUTED)),
                                Span::styled(clipped, style_output_line(first_preview)),
                            ];
                            if total > 1 {
                                spans.push(Span::styled(
                                    format!(
                                        "  … +{total} {} (^O expand)",
                                        if total == 1 { "line" } else { "lines" }
                                    ),
                                    Style::default().fg(MUTED),
                                ));
                            }
                            lines.push(Line::from(spans));
                        } else if is_edit_tool && !self.expand_tools {
                            // Claude Code compact edit hunk: show first `show_lines` lines (preserving `@@` header) + `… +N more lines` tail!
                            let take_n = total.min(show_lines);
                            let rem = total.saturating_sub(take_n);
                            let mut first_row = true;
                            let mut gutter = DiffGutterState::default();
                            for row_spans in gutter.render_block(&out_lines[..take_n], inner.saturating_sub(8)) {
                                let branch = if first_row { "  ⎿  " } else { "     " };
                                first_row = false;
                                let mut spans = vec![
                                    Span::raw(left_margin.clone()),
                                    Span::styled(branch, Style::default().fg(MUTED)),
                                ];
                                spans.extend(row_spans);
                                lines.push(Line::from(spans));
                            }
                            if rem > 0 {
                                lines.push(Line::from(vec![
                                    Span::raw(left_margin.clone()),
                                    Span::raw("     "),
                                    Span::styled(
                                        format!(
                                            "… +{rem} {} (^O expand)",
                                            if rem == 1 { "line" } else { "lines" }
                                        ),
                                        Style::default().fg(MUTED),
                                    ),
                                ]));
                            }
                        } else {
                            let skip = total.saturating_sub(show_lines);
                            let mut first_row = true;
                            let mut gutter = DiffGutterState::default();
                            // Feed skipped `@@` headers so line numbers in visible tail stay accurate!
                            for &skipped in &out_lines[..skip] {
                                if skipped.starts_with("@@ ") {
                                    let _ = gutter.render_line(skipped, inner.saturating_sub(8));
                                }
                            }
                            if skip > 0 {
                                lines.push(Line::from(vec![
                                    Span::raw(left_margin.clone()),
                                    Span::styled("  ⎿  ", Style::default().fg(MUTED)),
                                    Span::styled(
                                        format!("… ({skip} earlier lines hidden · ^O to expand)"),
                                        Style::default().fg(MUTED),
                                    ),
                                ]));
                                first_row = false;
                            }
                            for row_spans in gutter.render_block(&out_lines[skip..], inner.saturating_sub(8)) {
                                let branch = if first_row { "  ⎿  " } else { "     " };
                                first_row = false;
                                let mut spans = vec![
                                    Span::raw(left_margin.clone()),
                                    Span::styled(branch, Style::default().fg(MUTED)),
                                ];
                                spans.extend(row_spans);
                                lines.push(Line::from(spans));
                            }
                        }
                    }
                }
                Cell::Reasoning {
                    text,
                    started_at,
                    elapsed,
                } => {
                    let prev_is_trace = cell_idx > 0
                        && matches!(
                            self.cells[cell_idx - 1],
                            Cell::Tool(_) | Cell::Reasoning { .. } | Cell::Notice { .. }
                        );
                    if prev_non_empty && !prev_is_trace {
                        lines.push(Line::from(""));
                    }
                    let wrapped = wrap_plain(text.trim(), prose_w.saturating_sub(2));
                    let total = wrapped.len();
                    let line_word = if total == 1 { "line" } else { "lines" };
                    let is_prior_step = !self.expand_tools && cell_idx < last_idx;
                    if is_prior_step {
                        let dur_secs = elapsed.as_secs();
                        let label = if dur_secs >= 1 {
                            format!("Thought for {}", fmt_elapsed_compact(dur_secs))
                        } else {
                            String::from("Thought")
                        };
                        let hint = if total > 0 {
                            format!(" · {total} {line_word} (^O expand)")
                        } else {
                            String::new()
                        };
                        lines.push(Line::from(vec![
                            Span::raw(left_margin.clone()),
                            Span::styled("∴ ", Style::default().fg(MUTED)),
                            Span::styled(
                                label,
                                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                            ),
                            Span::styled(hint, Style::default().fg(MUTED)),
                        ]));
                    } else {
                        let live_secs = if cell_idx == last_idx && live_streaming {
                            started_at.elapsed().as_secs()
                        } else {
                            elapsed.as_secs()
                        };
                        let show = if self.expand_tools { total } else { total.min(2) };
                        let mut header_hint = if live_secs >= 1 {
                            format!(" ({})", fmt_elapsed_compact(live_secs))
                        } else {
                            String::new()
                        };
                        if !self.expand_tools && total > 2 {
                            let rem = total - 2;
                            header_hint.push_str(&format!(
                                "  … +{rem} {} (^O expand)",
                                if rem == 1 { "line" } else { "lines" }
                            ));
                        }
                        lines.push(Line::from(vec![
                            Span::raw(left_margin.clone()),
                            Span::styled("∴ ", Style::default().fg(MUTED)),
                            Span::styled(
                                "Thinking",
                                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                            ),
                            Span::styled(header_hint, Style::default().fg(MUTED)),
                        ]));
                        for row in wrapped.into_iter().take(show) {
                            lines.push(Line::from(vec![
                                Span::raw(left_margin.clone()),
                                Span::raw("  "),
                                Span::styled(
                                    row,
                                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                                ),
                            ]));
                        }
                    }
                }
                Cell::Notice { kind, text } => {
                    if prev_non_empty
                        && cell_idx > 0
                        && !matches!(self.cells[cell_idx - 1], Cell::Notice { .. })
                    {
                        lines.push(Line::from(""));
                    }
                    let (prefix, color) = match kind {
                        NoticeKind::Playbook => ("✦ ", ACCENT),
                        NoticeKind::Info => ("◈ ", MUTED),
                        NoticeKind::Warn => ("▲ ", AMBER),
                        NoticeKind::Error => ("✕ ", ROSE),
                        NoticeKind::Success => ("✓ ", SAGE),
                    };
                    for (idx, wrapped) in wrap_plain(text, inner.saturating_sub(4))
                        .into_iter()
                        .enumerate()
                    {
                        lines.push(Line::from(vec![
                            Span::raw(left_margin.clone()),
                            Span::styled(
                                if idx == 0 { prefix } else { "  " },
                                Style::default().fg(color),
                            ),
                            Span::styled(wrapped, Style::default().fg(color)),
                        ]));
                    }
                }
            }
            cell_idx += 1;
        }
        if !lines.is_empty()
            && !lines
                .last()
                .is_some_and(|l| l.spans.is_empty() || l.to_string().trim().is_empty())
        {
            lines.push(Line::from(""));
        }
        lines
    }

    fn try_fold_turn_trace(
        &self,
        start: usize,
        last_idx: usize,
        left_margin: &str,
        inner: usize,
    ) -> Option<(usize, Line<'static>)> {
        // Keep file-mutating tools (`edit`, `write`, `apply_patch`) visible as crisp `Update(...)`
        // cards with word-level diff hunks rather than swallowing them into exploration folds.
        let is_foldable_tool = |t: &ToolCell| -> bool {
            t.finished
                && t.tier != "local"
                && !matches!(t.name.as_str(), "edit" | "write" | "apply_patch")
        };
        let is_foldable_notice = |kind: NoticeKind, text: &str| -> bool {
            match kind {
                NoticeKind::Error => false,
                NoticeKind::Info | NoticeKind::Playbook => true,
                NoticeKind::Warn => {
                    text.starts_with("guard tripped")
                        || text.starts_with("loop-breaker")
                        || text.starts_with("subagent[")
                        || text.starts_with("task ")
                        || text.starts_with("approval ")
                }
                NoticeKind::Success => {
                    text.starts_with("session rule added")
                        || text.starts_with("approval ")
                        || text.starts_with("subagent[")
                        || text.starts_with("task ")
                }
            }
        };

        // Trigger folding when a finished non-local tool appears (optionally preceded by a prior reasoning step).
        match &self.cells[start] {
            Cell::Tool(t) if is_foldable_tool(t) => {}
            Cell::Reasoning { .. } if start < last_idx => {
                let next_is_tool = matches!(
                    self.cells.get(start + 1),
                    Some(Cell::Tool(t)) if is_foldable_tool(t)
                );
                if !next_is_tool {
                    return None;
                }
            }
            _ => return None,
        }

        let mut idx = start;
        let mut last_folded_end = start;
        let mut tools: Vec<&ToolCell> = Vec::new();
        let mut thoughts = 0usize;

        while idx < self.cells.len() {
            match &self.cells[idx] {
                Cell::Tool(t) if is_foldable_tool(t) => {
                    tools.push(t);
                    idx += 1;
                    last_folded_end = idx;
                }
                Cell::Reasoning { .. } if idx < last_idx => {
                    thoughts += 1;
                    idx += 1;
                    last_folded_end = idx;
                }
                Cell::Assistant(text) if text.trim().is_empty() && idx < last_idx => {
                    idx += 1;
                    last_folded_end = idx;
                }
                Cell::Notice { kind, text } if is_foldable_notice(*kind, text) => {
                    idx += 1;
                    last_folded_end = idx;
                }
                _ => break,
            }
        }
        if tools.is_empty() {
            return None;
        }

        // Merge overlapping `[started_at, started_at + elapsed]` intervals so 6 parallel
        // 2-minute subagents report real wall time (`2m 00s`) instead of `12m 00s`.
        let mut intervals: Vec<(Instant, Instant)> = tools
            .iter()
            .filter_map(|t| t.elapsed.map(|d| (t.started_at, t.started_at + d)))
            .collect();
        intervals.sort_by_key(|(s, _)| *s);
        let mut total_dur = Duration::ZERO;
        let mut cur_span: Option<(Instant, Instant)> = None;
        for (s, e) in intervals {
            match cur_span {
                None => cur_span = Some((s, e)),
                Some((cs, ce)) if s <= ce => {
                    cur_span = Some((cs, ce.max(e)));
                }
                Some((cs, ce)) => {
                    total_dur += ce.saturating_duration_since(cs);
                    cur_span = Some((s, e));
                }
            }
        }
        if let Some((cs, ce)) = cur_span {
            total_dur += ce.saturating_duration_since(cs);
        }
        let dur_label = if total_dur.as_millis() > 0 && total_dur.as_secs() == 0 {
            "<1s".to_owned()
        } else {
            fmt_elapsed_compact(total_dur.as_secs())
        };
        let failed = tools
            .iter()
            .filter(|t| t.exit_code.is_some_and(|c| c != 0))
            .count();

        if tools.len() == 1 {
            let t = tools[0];
            let out_n = t.output.lines().count();
            let sum_cap = inner.saturating_sub(38).clamp(14, 72);
            let mut spans = vec![
                Span::raw(left_margin.to_owned()),
                Span::styled(
                    "▸ ",
                    Style::default().fg(if failed > 0 { ROSE } else { MUTED }),
                ),
                Span::styled(
                    format!("{} ", tool_glyph(&t.name)),
                    Style::default().fg(MUTED),
                ),
                Span::styled(title_tool_name(&t.name), Style::default().fg(STONE)),
            ];
            if !t.summary.is_empty() {
                spans.push(Span::styled(
                    format!("({})", clip_width(&t.summary, sum_cap)),
                    Style::default().fg(MUTED),
                ));
            }
            let mut meta = format!("  {dur_label}");
            if out_n > 0 {
                meta.push_str(&format!(" · {out_n}L hidden"));
            }
            if failed > 0 {
                meta.push_str(" · failed");
            }
            meta.push_str(" · ^O expand");
            spans.push(Span::styled(meta, Style::default().fg(MUTED)));
            return Some((last_folded_end, Line::from(spans)));
        }

        let mut counts: Vec<(&str, usize)> = Vec::new();
        for t in &tools {
            let bucket = match t.name.as_str() {
                "read" | "read_file" | "outline" | "inspect" => "read",
                "grep" | "glob" => "search",
                "edit" | "write" | "apply_patch" => "edit",
                "bash" => "bash",
                "subagent" => "subagent",
                "task" => "task",
                "skill" => "skill",
                other => other,
            };
            if let Some((_, c)) = counts.iter_mut().find(|(k, _)| *k == bucket) {
                *c += 1;
            } else {
                counts.push((bucket, 1));
            }
        }
        counts.sort_by_key(|a| std::cmp::Reverse(a.1));
        let shown_tools: usize = counts.iter().take(4).map(|(_, c)| *c).sum();
        let rem_tools = tools.len().saturating_sub(shown_tools);
        let mut breakdown: Vec<String> = counts
            .into_iter()
            .take(4)
            .map(|(k, c)| format!("{c} {k}"))
            .collect();
        if rem_tools > 0 {
            breakdown.push(format!("+{rem_tools} other"));
        } else if thoughts > 0 && breakdown.len() < 4 {
            breakdown.push(format!(
                "{thoughts} {}",
                if thoughts == 1 { "thought" } else { "thoughts" }
            ));
        }
        if failed > 0 {
            breakdown.push(format!("{failed} failed"));
        }
        let line = Line::from(vec![
            Span::raw(left_margin.to_owned()),
            Span::styled(
                "▸ ",
                Style::default()
                    .fg(if failed > 0 { AMBER } else { SAGE })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} tools", tools.len()),
                Style::default().fg(STONE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" ({})", breakdown.join(" · ")),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!("  {dur_label} · ^O expand"),
                Style::default().fg(MUTED),
            ),
        ]);
        Some((last_folded_end, line))
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, opts: RenderOpts<'_>) {
        let mut lines = self.build_lines(
            area.width as usize,
            opts.repo_path,
            opts.env_summary,
            opts.live_streaming,
            opts.run_elapsed,
            opts.motion,
        );

        if self.search_open && !self.search_query.is_empty() {
            let needle = self.search_query.to_lowercase();
            self.search_matches = lines
                .iter()
                .enumerate()
                .filter_map(|(idx, line)| {
                    line.to_string()
                        .to_lowercase()
                        .contains(&needle)
                        .then_some(idx)
                })
                .collect();
            if !self.search_matches.is_empty() {
                self.search_match_idx = self.search_match_idx.min(self.search_matches.len() - 1);
                let active_line = self.search_matches[self.search_match_idx];
                lines = lines
                    .into_iter()
                    .enumerate()
                    .map(|(i, l)| highlight_search(l, &self.search_query, i == active_line))
                    .collect();
            }
        } else {
            self.search_matches.clear();
        }

        let view_h = area.height as usize;
        let max_scroll = lines.len().saturating_sub(view_h);
        self.scroll.sync_bounds(max_scroll, opts.motion);
        self.scroll.step(opts.dt, opts.motion);
        let top = self.scroll.current_offset().min(max_scroll);
        let bottom = (top + view_h).min(lines.len());
        let visible: Vec<Line<'static>> = lines.into_iter().skip(top).take(bottom - top).collect();

        frame.render_widget(Paragraph::new(visible), area);

        if self.scroll.show_tail_badge() && area.width > 24 {
            let badge = " ↓ live tail (End) ";
            let badge_w = display_width(badge) as u16;
            let badge_area = Rect {
                x: area.x + area.width.saturating_sub(badge_w + 2),
                y: area.y + area.height.saturating_sub(1),
                width: badge_w,
                height: 1,
            };
            frame.render_widget(Clear, badge_area);
            frame.render_widget(
                Paragraph::new(badge).style(
                    Style::default()
                        .fg(ON_ACCENT_FG)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                badge_area,
            );
        }
    }

    pub fn render_search_bar(&self, frame: &mut Frame, area: Rect) {
        if !self.search_open {
            return;
        }
        let count_str = if self.search_query.is_empty() {
            "type to search".to_owned()
        } else if self.search_matches.is_empty() {
            "0 matches".to_owned()
        } else {
            format!("{}/{}", self.search_match_idx + 1, self.search_matches.len())
        };
        let bar = Line::from(vec![
            Span::styled(
                "  Find: ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(self.search_query.clone(), Style::default().fg(IVORY)),
            Span::styled(
                format!("  [{count_str} · Enter next · Shift+Enter prev · Esc close]"),
                Style::default().fg(MUTED),
            ),
        ]);
        frame.render_widget(Paragraph::new(bar).style(Style::default().bg(SURFACE)), area);
        let cur_x = area.x + 8 + display_width(&self.search_query) as u16;
        if cur_x < area.x + area.width {
            frame.set_cursor_position((cur_x, area.y));
        }
    }

    /// Format the full transcript as clean Markdown for `/export`.
    pub fn export_markdown(
        &self,
        sid: &str,
        title: Option<&str>,
        model: &str,
        repo_path: &str,
    ) -> String {
        let heading = title
            .map(|t| format!("# {t}"))
            .unwrap_or_else(|| "# twig session export".to_owned());
        let mut out = format!(
            "{heading}\n\n- **Session:** `{}`\n- **Model:** `{model}`\n- **Workspace:** `{repo_path}`\n\n---\n\n",
            if sid.is_empty() { "unknown" } else { sid }
        );
        for cell in &self.cells {
            match cell {
                Cell::User(text) => {
                    out.push_str("## User\n\n");
                    out.push_str(text.trim_end());
                    out.push_str("\n\n");
                }
                Cell::Steering(text) => {
                    out.push_str(&format!("> **Steering:** {}\n\n", text.trim()));
                }
                Cell::Reasoning { text, .. } => {
                    out.push_str("<details>\n<summary>Thinking</summary>\n\n");
                    out.push_str(text.trim_end());
                    out.push_str("\n\n</details>\n\n");
                }
                Cell::Assistant(text) => {
                    out.push_str("## Assistant\n\n");
                    out.push_str(text.trim_end());
                    out.push_str("\n\n");
                }
                Cell::Tool(tool) => {
                    let exit = tool
                        .exit_code
                        .map(|c| format!(" (exit {c})"))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "### Tool: {} `{}`{exit}\n\n",
                        title_tool_name(&tool.name),
                        tool.summary
                    ));
                    if !tool.output.trim().is_empty() {
                        out.push_str("```text\n");
                        out.push_str(tool.output.trim_end());
                        out.push_str("\n```\n\n");
                    }
                }
                Cell::Notice { text, .. } => {
                    out.push_str(&format!("> *{}*\n\n", text.trim()));
                }
            }
        }
        out
    }
}

fn count_diff_changes(diff_text: &str) -> (usize, usize) {
    let mut ins = 0usize;
    let mut del = 0usize;
    for l in diff_text.lines() {
        if l.starts_with('+') && !l.starts_with("+++ ") {
            ins += 1;
        } else if l.starts_with('-') && !l.starts_with("--- ") {
            del += 1;
        }
    }
    (ins, del)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn export_markdown_formats_conversation_and_tools() {
        let mut t = Transcript::new();
        t.push_user("Explain architecture".into());
        t.push_assistant("Five deep modules.".into());
        t.push_tool_snapshot(
            "t1".into(),
            "bash".into(),
            "cargo test".into(),
            "auto".into(),
            "ok".into(),
            Some(0),
        );
        let md = t.export_markdown("sess-42", Some("Audit"), "magpie", "~/twig");
        assert!(md.contains("# Audit"));
        assert!(md.contains("Explain architecture"));
        assert!(md.contains("Five deep modules."));
        assert!(md.contains("Tool: Bash `cargo test` (exit 0)"));
    }

    #[test]
    fn copy_extracts_code_blocks_and_full_reply_and_replays_history() {
        let mut t = Transcript::new();
        t.push_assistant(
            "First:\n```rust\nlet a = 1;\n```\nSecond:\n```rust\nlet b = 2;\n```".into(),
        );
        assert_eq!(t.copy_latest_code_block().unwrap().0, "let b = 2;");
        assert_eq!(t.copy_latest_code_block().unwrap().0, "let a = 1;");
        assert!(t.copy_latest_reply().unwrap().0.contains("let a = 1;"));

        t.clear();
        t.replay_history(&[
            json!({"role": "user", "content": "check build"}),
            json!({
                "role": "assistant",
                "content": "Running build now.",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"command\":\"cargo check\"}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "Finished"}),
        ]);
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn renders_1cell_bullets_collapses_prior_thinking_and_formats_tables() {
        let mut t = Transcript::new();
        t.push_user("上海天气".into());
        t.append_reasoning_delta("First check wttr.in endpoint\nand fallback to cn");
        t.push_tool_snapshot(
            "t1".into(),
            "bash".into(),
            "curl weather".into(),
            "audit".into(),
            "raw curl output line".into(),
            Some(0),
        );
        t.push_assistant("   \n".into());
        t.push_assistant("| 日期 | 天气 |\n|---|---|\n| 11-27 | 晴 $19^\\circ$ |".into());

        // Collapsed (default): hides tool output & empty ghost replies, keeps Request (❯) & Result (◆) distinct.
        let collapsed: String = t
            .build_lines(
                72,
                "~/twig",
                "",
                false,
                Duration::from_secs(1),
                MotionMode::Reduced,
            )
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !collapsed.contains('\u{23fa}'),
            "must not emit wide emoji U+23FA: {collapsed}"
        );
        assert!(collapsed.contains("❯ 上海天气") && collapsed.contains("◆ "));
        assert!(
            collapsed.contains("▸ $ Bash(curl weather)") && collapsed.contains("^O expand"),
            "single finished tool folds output: {collapsed}"
        );
        assert!(!collapsed.contains("raw curl output line"));
        assert!(!collapsed.contains("First check wttr.in"));
        assert!(collapsed.contains('┌') && collapsed.contains("19°"));

        // Expanded (`Ctrl+O`): reveals full tool output & prior reasoning.
        t.toggle_expand();
        let expanded: String = t
            .build_lines(
                72,
                "~/twig",
                "",
                false,
                Duration::from_secs(1),
                MotionMode::Reduced,
            )
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains("raw curl output line"));
        assert!(expanded.contains("First check wttr.in"));
    }

    #[test]
    fn coalesces_explore_tool_bursts_and_renders_mermaid_svg_and_numbered_code() {
        let mut t = Transcript::new();
        t.push_user("梳理架构".into());
        t.push_tool_snapshot(
            "r1".into(),
            "read".into(),
            "src/main.rs".into(),
            "auto".into(),
            "fn main() {}".into(),
            Some(0),
        );
        t.append_reasoning_delta("Now grep router");
        t.push_tool_snapshot(
            "g1".into(),
            "grep".into(),
            "Router".into(),
            "auto".into(),
            "src/lib.rs:10".into(),
            Some(0),
        );
        t.push_assistant(
            "```mermaid\nflowchart LR\n  A[Client] --> B[Gateway]\n```\n```svg\n<svg width=\"400\" height=\"120\"><rect/><text>Gateway Node</text></svg>\n```\n```rust\nfn a() {\n    let x = 1;\n    println!(\"{x}\");\n}\n```\n> [!TIP]\n> Done\n- [x] Verified"
                .into(),
        );

        let lines = t.build_lines(
            76,
            "~/twig",
            "",
            false,
            Duration::from_secs(1),
            MotionMode::Reduced,
        );
        let joined: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("▸ 2 tools (1 read · 1 search"),
            "folded multi-tool trace: {joined}"
        );
        assert!(!joined.contains("fn main() {}"));
        assert!(joined.contains("Client") && joined.contains("──▶") && joined.contains("Gateway"));
        assert!(joined.contains("◈ SVG") && joined.contains("400×120") && joined.contains("[Gateway Node]"));
        assert!(joined.contains("4 lines · Alt+Y copy") && joined.contains(" 1 │ "));
        assert!(joined.contains("✦ Tip") && joined.contains("✓ Verified"));
    }
}
