//! Deep transcript module owning conversation cells, history replay, smooth scrolling,
//! tool tree rendering, inline `Ctrl+F` search, and `Alt+Y` / `Alt+Shift+Y` clipboard extraction.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;
use serde_json::Value;

use super::markdown::{
    breathing_star, display_width, extract_code_blocks, fmt_elapsed_compact, highlight_search,
    render_markdown, render_welcome_card, streaming_caret, style_output_line, tool_glyph,
    wrap_plain, DiffGutterState, MotionMode, SmoothScroll, ACCENT, AMBER, IVORY, MUTED, ROSE, SAGE,
    STONE, SURFACE,
};

pub struct RenderOpts<'a> {
    pub repo_path: &'a str,
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

    pub fn push_assistant(&mut self, text: String) {
        self.cells.push(Cell::Assistant(text));
        self.scroll.follow_tail();
    }

    pub fn append_assistant_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(Cell::Assistant(buf)) = self.cells.last_mut() {
            buf.push_str(delta);
        } else {
            self.cells.push(Cell::Assistant(delta.to_owned()));
        }
    }

    pub fn push_tool_started(&mut self, id: String, name: String, summary: String, tier: String) {
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
        self.scroll.follow_tail();
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
        self.scroll.follow_tail();
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

    pub fn follow_tail(&mut self) {
        self.scroll.follow_tail();
    }

    pub fn is_scroll_animating(&self, motion: MotionMode) -> bool {
        self.scroll.is_animating(motion)
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
        live_streaming: bool,
        run_elapsed: Duration,
        motion: MotionMode,
    ) -> Vec<Line<'static>> {
        if self.cells.is_empty() {
            return render_welcome_card("", "", None, repo_path, width);
        }
        let mut lines = Vec::new();
        let inner = width.saturating_sub(4).max(16);
        let last_idx = self.cells.len().saturating_sub(1);

        for (cell_idx, cell) in self.cells.iter().enumerate() {
            let prev_is_tool = cell_idx > 0 && matches!(self.cells[cell_idx - 1], Cell::Tool(_));
            match cell {
                Cell::User(text) => {
                    lines.push(Line::from(""));
                    let content_w = inner.saturating_sub(4);
                    for (idx, wrapped) in wrap_plain(text, content_w).into_iter().enumerate() {
                        let prefix = if idx == 0 { " ❯ " } else { "   " };
                        let pad =
                            inner.saturating_sub(display_width(prefix) + display_width(&wrapped));
                        lines.push(Line::from(vec![
                            Span::raw("  "),
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
                    lines.push(Line::from(""));
                }
                Cell::Steering(text) => {
                    for (idx, wrapped) in wrap_plain(text, inner.saturating_sub(11))
                        .into_iter()
                        .enumerate()
                    {
                        lines.push(Line::from(vec![
                            Span::styled(
                                if idx == 0 {
                                    "  ↳ steer  "
                                } else {
                                    "           "
                                },
                                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(wrapped, Style::default().fg(AMBER)),
                        ]));
                    }
                    lines.push(Line::from(""));
                }
                Cell::Assistant(text) => {
                    if prev_is_tool {
                        lines.push(Line::from(""));
                    }
                    let mut md_lines = render_markdown(text, inner.saturating_sub(3));
                    if cell_idx == last_idx && live_streaming {
                        let caret = streaming_caret(run_elapsed, motion);
                        if let Some(last) = md_lines.last_mut() {
                            last.spans.push(caret);
                        }
                    }
                    for (idx, line) in md_lines.into_iter().enumerate() {
                        let mut spans = vec![if idx == 0 {
                            Span::styled("  ⏺ ", Style::default().fg(IVORY))
                        } else {
                            Span::raw("    ")
                        }];
                        spans.extend(line.spans);
                        lines.push(Line::from(spans));
                    }
                    lines.push(Line::from(""));
                }
                Cell::Tool(tool) => {
                    let elapsed_dur = tool.elapsed.unwrap_or_else(|| tool.started_at.elapsed());
                    let icon_span = if !tool.finished {
                        breathing_star(elapsed_dur, motion, true)
                    } else if tool.exit_code.is_none() || tool.exit_code == Some(0) {
                        Span::styled("⏺", Style::default().fg(SAGE))
                    } else {
                        Span::styled("⏺", Style::default().fg(ROSE))
                    };

                    let mut header = vec![
                        Span::raw("  "),
                        icon_span,
                        Span::raw(" "),
                        Span::styled(
                            format!("{} ", tool_glyph(&tool.name)),
                            Style::default().fg(MUTED),
                        ),
                        Span::styled(
                            title_tool_name(&tool.name),
                            Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if !tool.summary.is_empty() {
                        header.push(Span::styled("(", Style::default().fg(MUTED)));
                        header.push(Span::styled(
                            tool.summary.clone(),
                            Style::default().fg(STONE),
                        ));
                        header.push(Span::styled(")", Style::default().fg(MUTED)));
                    }
                    if tool.tier != "auto" {
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

                    let out_lines: Vec<&str> = tool.output.lines().collect();
                    if !out_lines.is_empty() {
                        let is_local_expanded = tool.tier == "local" || tool.name == "diff";
                        let show_lines = if self.expand_tools || is_local_expanded {
                            200
                        } else if !tool.finished {
                            5
                        } else if tool.exit_code.is_some_and(|c| c != 0) {
                            6
                        } else {
                            1
                        };
                        let total = out_lines.len();
                        let skip = total.saturating_sub(show_lines);
                        let is_compact_summary = !self.expand_tools
                            && !is_local_expanded
                            && tool.finished
                            && tool.exit_code.unwrap_or(0) == 0;

                        if is_compact_summary {
                            let first_preview = out_lines
                                .iter()
                                .find(|l| !l.trim().is_empty())
                                .copied()
                                .unwrap_or("");
                            let clipped = clip_str(first_preview.trim(), inner.saturating_sub(34));
                            let mut spans = vec![
                                Span::styled("    ⎿  ", Style::default().fg(MUTED)),
                                Span::styled(clipped, style_output_line(first_preview)),
                            ];
                            if total > 1 {
                                spans.push(Span::styled(
                                    format!("  … +{total} lines (^O expand)"),
                                    Style::default().fg(MUTED),
                                ));
                            }
                            lines.push(Line::from(spans));
                        } else {
                            let mut first_row = true;
                            let mut gutter = DiffGutterState::default();
                            if skip > 0 {
                                lines.push(Line::from(vec![
                                    Span::styled("    ⎿  ", Style::default().fg(MUTED)),
                                    Span::styled(
                                        format!("… ({skip} earlier lines hidden · ^O to expand)"),
                                        Style::default().fg(MUTED),
                                    ),
                                ]));
                                first_row = false;
                            }
                            for raw in &out_lines[skip..] {
                                for row_spans in gutter.render_line(raw, inner.saturating_sub(8)) {
                                    let branch = if first_row { "    ⎿  " } else { "       " };
                                    first_row = false;
                                    let mut spans =
                                        vec![Span::styled(branch, Style::default().fg(MUTED))];
                                    spans.extend(row_spans);
                                    lines.push(Line::from(spans));
                                }
                            }
                        }
                    }
                }
                Cell::Notice { kind, text } => {
                    let (prefix, color) = match kind {
                        NoticeKind::Playbook => ("  ✦ ", ACCENT),
                        NoticeKind::Info => ("  ◈ ", MUTED),
                        NoticeKind::Warn => ("  ▲ ", AMBER),
                        NoticeKind::Error => ("  ✕ ", ROSE),
                        NoticeKind::Success => ("  ✓ ", SAGE),
                    };
                    for (idx, wrapped) in wrap_plain(text, inner.saturating_sub(4))
                        .into_iter()
                        .enumerate()
                    {
                        lines.push(Line::from(vec![
                            Span::styled(
                                if idx == 0 { prefix } else { "    " },
                                Style::default().fg(color),
                            ),
                            Span::styled(wrapped, Style::default().fg(color)),
                        ]));
                    }
                }
            }
        }
        lines
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, opts: RenderOpts<'_>) {
        let mut lines = self.build_lines(
            area.width as usize,
            opts.repo_path,
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

        let max_scroll = lines.len().saturating_sub(area.height as usize) as u16;
        self.scroll.sync_bounds(max_scroll, opts.motion);
        self.scroll.step(opts.dt, opts.motion);

        frame.render_widget(
            Paragraph::new(lines).scroll((self.scroll.current_u16(), 0)),
            area,
        );

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
                        .fg(Color::Rgb(24, 20, 18))
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
}

/// Borderless Codex-style interactive session picker rendered above the footer for `/resume`.
pub fn render_resume_picker(
    frame: &mut Frame,
    area: Rect,
    sessions: &[super::client::SessionSummary],
    sel: usize,
) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let rule_len = area.width.saturating_sub(2) as usize;
    let rule = format!(" {}", "─".repeat(rule_len));
    let max_rows = (area.height.saturating_sub(3) as usize).max(1);
    let start = sel.saturating_sub(max_rows.saturating_sub(1));
    let end = (start + max_rows).min(sessions.len());
    let row_w = area.width.saturating_sub(2) as usize;
    let sel_fg = Color::Rgb(22, 19, 17);

    let mut rows = vec![
        Line::from(Span::styled(rule.clone(), Style::default().fg(ACCENT))),
        Line::from(Span::styled(
            "  Resume session  (↑/↓ select · Enter open · Esc cancel)",
            Style::default()
                .fg(super::markdown::ACCENT_PEAK)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    for (offset, s) in sessions[start..end].iter().enumerate() {
        let active = start + offset == sel;
        let bg = if active { ACCENT } else { Color::Reset };
        let marker = if active { " › " } else { "   " };
        let age_col = format!("{:<8}", s.age);
        let short_sid: String = s.sid.chars().take(10).collect();
        let meta = format!("  t{} · {short_sid}", s.turns);
        let used = display_width(marker) + display_width(&age_col) + display_width(&meta);
        let label = clip_str(&s.label, row_w.saturating_sub(used + 2));
        let pad = row_w.saturating_sub(used + display_width(&label));
        rows.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                marker,
                Style::default()
                    .fg(if active { sel_fg } else { ACCENT })
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                age_col,
                Style::default()
                    .fg(if active { sel_fg } else { MUTED })
                    .bg(bg),
            ),
            Span::styled(
                label,
                Style::default()
                    .fg(if active { sel_fg } else { IVORY })
                    .bg(bg)
                    .add_modifier(if active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(
                meta,
                Style::default()
                    .fg(if active { sel_fg } else { STONE })
                    .bg(bg),
            ),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        ]));
    }
    rows.push(Line::from(Span::styled(rule, Style::default().fg(ACCENT))));
    frame.render_widget(Paragraph::new(rows), area);
}

pub fn title_tool_name(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => "Tool".into(),
    }
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

pub fn summarize_tool_args(name: &str, args: &Value) -> String {
    let raw = match name {
        "bash" => args["command"].as_str().unwrap_or(""),
        "read" => args["path"].as_str().unwrap_or(""),
        "skill" => args["name"].as_str().unwrap_or("list"),
        "subagent" => args["task"].as_str().unwrap_or(""),
        "papercut" => args["component"].as_str().unwrap_or(""),
        _ => "",
    };
    clip_str(&raw.replace('\n', " "), 92)
}
