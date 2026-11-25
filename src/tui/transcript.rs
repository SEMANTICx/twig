//! Deep transcript module owning conversation cells, history replay, smooth scrolling,
//! tool tree rendering, inline `Ctrl+F` search, and `Alt+Y` / `Alt+Shift+Y` clipboard extraction.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;
use serde_json::Value;

use super::markdown::{
    breathing_star, clip_str, display_width, extract_code_blocks, fmt_elapsed_compact,
    highlight_search, render_markdown, render_welcome_card, streaming_caret, style_output_line,
    tool_glyph, wrap_plain, DiffGutterState, MotionMode, SmoothScroll, ACCENT, ACCENT_PEAK, AMBER,
    IVORY, MUTED, ROSE, SAGE, STONE, SURFACE, SURFACE_HI,
};

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
    Reasoning(String),
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

    pub fn append_reasoning_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(Cell::Reasoning(buf)) = self.cells.last_mut() {
            buf.push_str(delta);
        } else {
            self.cells.push(Cell::Reasoning(delta.to_owned()));
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
            return render_welcome_card(repo_path, env_summary);
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
                        let is_local_expanded =
                            tool.tier == "local" || matches!(tool.name.as_str(), "diff" | "edit");
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
                Cell::Reasoning(text) => {
                    let wrapped = wrap_plain(text.trim(), inner.saturating_sub(6));
                    let total = wrapped.len();
                    let show = if self.expand_tools { total } else { total.min(2) };
                    let header_hint = if !self.expand_tools && total > 2 {
                        format!("  … +{total} lines (^O expand)")
                    } else {
                        String::new()
                    };
                    lines.push(Line::from(vec![
                        Span::styled("  ∴ ", Style::default().fg(MUTED)),
                        Span::styled(
                            "Thinking",
                            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                        ),
                        Span::styled(header_hint, Style::default().fg(MUTED)),
                    ]));
                    for row in wrapped.into_iter().take(show) {
                        lines.push(Line::from(vec![
                            Span::raw("    "),
                            Span::styled(
                                row,
                                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                            ),
                        ]));
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
                Cell::Reasoning(text) => {
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

#[derive(Debug, Clone)]
struct PendingApproval {
    id: String,
    name: String,
    args: Value,
    recovered: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ApprovalKeyResult {
    Ignored,
    Consumed,
    Resolve {
        id: String,
        allow: bool,
        remember: bool,
    },
}

/// Self-contained pending approval queue with borderless 3-option warm-amber selector.
#[derive(Default)]
pub struct ApprovalQueue {
    items: VecDeque<PendingApproval>,
    sel: usize,
}

impl ApprovalQueue {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.sel = 0;
    }

    pub fn push_from_value(&mut self, v: &Value) -> Option<(String, String)> {
        let id = v["id"].as_str().unwrap_or("").to_owned();
        if id.is_empty() || self.items.iter().any(|a| a.id == id) {
            return None;
        }
        let name = v["name"].as_str().unwrap_or("bash").to_owned();
        let summary = summarize_tool_args(&name, &v["args"]);
        self.items.push_back(PendingApproval {
            id,
            name: name.clone(),
            args: v["args"].clone(),
            recovered: v["recovered"] == true,
        });
        self.sel = 0;
        Some((name, summary))
    }

    pub fn resolve_id(&mut self, id: &str) {
        self.items.retain(|a| a.id != id);
        self.sel = 0;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ApprovalKeyResult {
        let Some(pending) = self.items.front() else {
            return ApprovalKeyResult::Ignored;
        };
        let id = pending.id.clone();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = self.sel.saturating_sub(1);
                ApprovalKeyResult::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(2);
                ApprovalKeyResult::Consumed
            }
            KeyCode::Char('1') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                ApprovalKeyResult::Resolve {
                    id,
                    allow: true,
                    remember: false,
                }
            }
            KeyCode::Char('2') | KeyCode::Char('a') | KeyCode::Char('A') => {
                ApprovalKeyResult::Resolve {
                    id,
                    allow: true,
                    remember: true,
                }
            }
            KeyCode::Char('3') | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                ApprovalKeyResult::Resolve {
                    id,
                    allow: false,
                    remember: false,
                }
            }
            KeyCode::Enter => {
                let (allow, remember) = match self.sel {
                    0 => (true, false),
                    1 => (true, true),
                    _ => (false, false),
                };
                ApprovalKeyResult::Resolve {
                    id,
                    allow,
                    remember,
                }
            }
            _ => ApprovalKeyResult::Ignored,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let Some(pending) = self.items.front() else {
            return;
        };
        render_approval_panel(
            frame,
            area,
            &pending.id,
            &pending.name,
            &pending.args,
            pending.recovered,
            self.sel,
        );
    }
}

/// Borderless warm-amber hairline approval selector rendered above the footer.
fn render_approval_panel(
    frame: &mut Frame,
    area: Rect,
    id: &str,
    name: &str,
    args: &Value,
    recovered: bool,
    sel: usize,
) {
    if area.height < 6 || area.width < 18 {
        return;
    }
    let rule_len = area.width.saturating_sub(2) as usize;
    let rule = format!(" {}", "─".repeat(rule_len));
    let inner_w = area.width.saturating_sub(4) as usize;
    let cmd_preview = summarize_tool_args(name, args);
    let title = if recovered {
        format!("  ▲ Recovered approval ({id}) — NO replay")
    } else {
        format!("  ▲ Approve {}? ({id})", title_tool_name(name))
    };

    let opt = |idx: usize, label: &str| -> Line<'static> {
        let active = sel == idx;
        let bg = if active { SURFACE_HI } else { Color::Reset };
        let marker = if active { " ▸ " } else { "   " };
        let fg = if active { IVORY } else { STONE };
        let pad = inner_w.saturating_sub(display_width(marker) + display_width(label));
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                marker,
                Style::default()
                    .fg(ACCENT)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                label.to_owned(),
                Style::default().fg(fg).bg(bg).add_modifier(if active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        ])
    };

    let preview_line = format!(" $ {}", clip_str(&cmd_preview, inner_w.saturating_sub(4)));
    let preview_pad = inner_w.saturating_sub(display_width(&preview_line));

    let rows = vec![
        Line::from(Span::styled(rule.clone(), Style::default().fg(AMBER))),
        Line::from(Span::styled(
            title,
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(preview_line, Style::default().fg(ACCENT_PEAK).bg(SURFACE)),
            Span::styled(" ".repeat(preview_pad), Style::default().bg(SURFACE)),
        ]),
        Line::from(""),
        opt(0, "1. Yes, allow once (y / Enter)"),
        opt(1, "2. Always allow this pattern for this session (a)"),
        opt(2, "3. No, deny and tell agent (n / Esc)"),
        Line::from(Span::styled(rule, Style::default().fg(AMBER))),
    ];
    frame.render_widget(Paragraph::new(rows), area);
}

#[derive(Debug, Clone)]
pub struct TodoEntry {
    content: String,
    status: String,
}

pub fn parse_todos(items: &[Value]) -> Vec<TodoEntry> {
    items
        .iter()
        .filter_map(|v| {
            let content = v["content"].as_str()?.trim();
            (!content.is_empty()).then(|| TodoEntry {
                content: content.to_owned(),
                status: v["status"].as_str().unwrap_or("pending").to_owned(),
            })
        })
        .collect()
}

pub fn todo_strip_height(todos: &[TodoEntry]) -> u16 {
    if todos.is_empty() {
        return 0;
    }
    (todos.len().min(3) as u16) + 1
}

/// Compact borderless task progress strip rendered above the composer when `todo` items exist.
pub fn render_todo_strip(frame: &mut Frame, area: Rect, todos: &[TodoEntry]) {
    if todos.is_empty() || area.height == 0 || area.width < 16 {
        return;
    }
    let done = todos.iter().filter(|t| t.status == "completed").count();
    let mut rows = vec![Line::from(vec![
        Span::styled(
            "  ☑ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("Plan progress  {done}/{}", todos.len()),
            Style::default().fg(STONE).add_modifier(Modifier::BOLD),
        ),
    ])];
    let inner = area.width.saturating_sub(8) as usize;
    let max_items = area.height.saturating_sub(1) as usize;
    let focus = todos
        .iter()
        .position(|t| t.status == "in_progress")
        .or_else(|| todos.iter().position(|t| t.status != "completed"))
        .unwrap_or(0);
    let start = focus.saturating_sub(1).min(todos.len().saturating_sub(max_items));
    for item in todos.iter().skip(start).take(max_items) {
        let (glyph, color) = match item.status.as_str() {
            "completed" => ("✓", SAGE),
            "in_progress" => ("●", ACCENT),
            _ => ("○", MUTED),
        };
        rows.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(
                clip_str(&item.content, inner),
                Style::default().fg(if item.status == "in_progress" {
                    IVORY
                } else if item.status == "completed" {
                    MUTED
                } else {
                    STONE
                }),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(rows), area);
}

const EFFORT_LEVELS: &[(&str, &str)] = &[
    ("off", "model default reasoning"),
    ("low", "fast & concise reasoning"),
    ("medium", "balanced depth and latency"),
    ("high", "deep reasoning for complex tasks"),
    ("xhigh", "maximum reasoning depth"),
];

pub enum PickerOutcome {
    None,
    Close,
    SelectSession(String),
    SelectModel(String),
    SelectEffort(String),
    RewindTo { turns_back: usize, prompt: String },
    ForkSession,
    FillComposer(String),
}

/// Borderless Codex-style interactive overlay picker shared by `/resume`, `/model`, `/effort`, `/rewind`, and `Ctrl+R`.
pub enum OverlayPicker {
    Resume {
        sessions: Vec<super::client::SessionSummary>,
        sel: usize,
    },
    Model {
        all: Vec<String>,
        filter: String,
        active: String,
        sel: usize,
    },
    Effort {
        active: Option<String>,
        sel: usize,
    },
    Rewind {
        turns: Vec<String>,
        sel: usize,
    },
    History {
        all: Vec<String>,
        filter: String,
        sel: usize,
    },
}

impl OverlayPicker {
    pub fn resume(sessions: Vec<super::client::SessionSummary>) -> Self {
        Self::Resume { sessions, sel: 0 }
    }

    pub fn model(all: Vec<String>, active: String) -> Self {
        let sel = all.iter().position(|m| m == &active).unwrap_or(0);
        Self::Model {
            all,
            filter: String::new(),
            active,
            sel,
        }
    }

    pub fn effort(active: Option<String>) -> Self {
        let target = active.as_deref().unwrap_or("off");
        let sel = EFFORT_LEVELS
            .iter()
            .position(|(k, _)| *k == target)
            .unwrap_or(0);
        Self::Effort { active, sel }
    }

    pub fn rewind(turns: Vec<String>) -> Self {
        let sel = turns.len().saturating_sub(1);
        Self::Rewind { turns, sel }
    }

    pub fn history(entries: &[String]) -> Self {
        let mut rev: Vec<String> = entries.iter().rev().cloned().collect();
        rev.truncate(200);
        Self::History {
            all: rev,
            filter: String::new(),
            sel: 0,
        }
    }

    pub fn hint(&self) -> &'static str {
        match self {
            Self::Resume { .. } => "↑/↓ select · Enter open · Esc close  ",
            Self::Model { .. } => "type to filter · ↑/↓ select · Enter apply · Esc close  ",
            Self::Effort { .. } => "↑/↓ select · Enter apply · Esc close  ",
            Self::Rewind { .. } => "↑/↓ select · Enter rewind & edit · f fork · Esc close  ",
            Self::History { .. } => "type to filter · ↑/↓ select · Enter fill · Esc close  ",
        }
    }

    pub fn desired_height(&self) -> u16 {
        let count = match self {
            Self::Resume { sessions, .. } => sessions.len(),
            Self::Model { all, filter, .. } => filtered_models(all, filter).len(),
            Self::Effort { .. } => EFFORT_LEVELS.len(),
            Self::Rewind { turns, .. } => turns.len(),
            Self::History { all, filter, .. } => filtered_models(all, filter).len(),
        };
        (count.clamp(1, 8) as u16) + 3
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        match self {
            Self::Resume { sessions, sel } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => PickerOutcome::Close,
                KeyCode::Up | KeyCode::Char('k') if !sessions.is_empty() => {
                    *sel = sel.saturating_sub(1);
                    PickerOutcome::None
                }
                KeyCode::Down | KeyCode::Char('j') if !sessions.is_empty() => {
                    *sel = (*sel + 1).min(sessions.len() - 1);
                    PickerOutcome::None
                }
                KeyCode::Enter => sessions
                    .get(*sel)
                    .map(|s| PickerOutcome::SelectSession(s.sid.clone()))
                    .unwrap_or(PickerOutcome::Close),
                _ => PickerOutcome::None,
            },
            Self::Effort { sel, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => PickerOutcome::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    *sel = sel.saturating_sub(1);
                    PickerOutcome::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *sel = (*sel + 1).min(EFFORT_LEVELS.len() - 1);
                    PickerOutcome::None
                }
                KeyCode::Enter => PickerOutcome::SelectEffort(EFFORT_LEVELS[*sel].0.to_owned()),
                _ => PickerOutcome::None,
            },
            Self::Rewind { turns, sel } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => PickerOutcome::Close,
                KeyCode::Char('f') => PickerOutcome::ForkSession,
                KeyCode::Up | KeyCode::Char('k') if !turns.is_empty() => {
                    *sel = sel.saturating_sub(1);
                    PickerOutcome::None
                }
                KeyCode::Down | KeyCode::Char('j') if !turns.is_empty() => {
                    *sel = (*sel + 1).min(turns.len() - 1);
                    PickerOutcome::None
                }
                KeyCode::Enter => turns
                    .get(*sel)
                    .map(|prompt| PickerOutcome::RewindTo {
                        turns_back: turns.len() - *sel,
                        prompt: prompt.clone(),
                    })
                    .unwrap_or(PickerOutcome::Close),
                _ => PickerOutcome::None,
            },
            Self::Model {
                all, filter, sel, ..
            } => {
                let items = filtered_models(all, filter);
                match key.code {
                    KeyCode::Esc => PickerOutcome::Close,
                    KeyCode::Up if !items.is_empty() => {
                        *sel = sel.saturating_sub(1);
                        PickerOutcome::None
                    }
                    KeyCode::Down if !items.is_empty() => {
                        *sel = (*sel + 1).min(items.len() - 1);
                        PickerOutcome::None
                    }
                    KeyCode::Enter => items
                        .get(*sel)
                        .map(|m| PickerOutcome::SelectModel((*m).to_owned()))
                        .unwrap_or(PickerOutcome::Close),
                    KeyCode::Backspace => {
                        filter.pop();
                        *sel = 0;
                        PickerOutcome::None
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        filter.push(c);
                        *sel = 0;
                        PickerOutcome::None
                    }
                    _ => PickerOutcome::None,
                }
            }
            Self::History { all, filter, sel } => {
                let items = filtered_models(all, filter);
                match key.code {
                    KeyCode::Esc => PickerOutcome::Close,
                    KeyCode::Up if !items.is_empty() => {
                        *sel = sel.saturating_sub(1);
                        PickerOutcome::None
                    }
                    KeyCode::Down if !items.is_empty() => {
                        *sel = (*sel + 1).min(items.len() - 1);
                        PickerOutcome::None
                    }
                    KeyCode::Enter => items
                        .get(*sel)
                        .map(|entry| PickerOutcome::FillComposer((*entry).to_owned()))
                        .unwrap_or(PickerOutcome::Close),
                    KeyCode::Backspace => {
                        filter.pop();
                        *sel = 0;
                        PickerOutcome::None
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        filter.push(c);
                        *sel = 0;
                        PickerOutcome::None
                    }
                    _ => PickerOutcome::None,
                }
            }
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        match self {
            Self::Resume { sessions, sel } => render_resume_picker(frame, area, sessions, *sel),
            Self::Model {
                all,
                filter,
                active,
                sel,
            } => render_model_picker(frame, area, all, filter, active, *sel),
            Self::Effort { active, sel } => {
                render_effort_picker(frame, area, active.as_deref(), *sel);
            }
            Self::Rewind { turns, sel } => render_rewind_picker(frame, area, turns, *sel),
            Self::History { all, filter, sel } => {
                render_history_picker(frame, area, all, filter, *sel);
            }
        }
    }
}

fn render_effort_picker(frame: &mut Frame, area: Rect, active: Option<&str>, sel: usize) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let rule_len = area.width.saturating_sub(2) as usize;
    let rule = format!(" {}", "─".repeat(rule_len));
    let row_w = area.width.saturating_sub(2) as usize;
    let sel_fg = Color::Rgb(22, 19, 17);
    let current = active.unwrap_or("off");

    let mut rows = vec![
        Line::from(Span::styled(rule.clone(), Style::default().fg(ACCENT))),
        Line::from(Span::styled(
            "  Reasoning effort  (↑/↓ select · Enter apply · Esc cancel)",
            Style::default()
                .fg(ACCENT_PEAK)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    for (idx, (level, desc)) in EFFORT_LEVELS.iter().enumerate() {
        let is_sel = idx == sel;
        let bg = if is_sel { ACCENT } else { Color::Reset };
        let marker = if is_sel { " › " } else { "   " };
        let col = format!("{level:<8}");
        let badge = if *level == current { "  (active)" } else { "" };
        let used = display_width(marker) + display_width(&col) + display_width(badge);
        let d = clip_str(desc, row_w.saturating_sub(used + 2));
        let pad = row_w.saturating_sub(used + display_width(&d));
        rows.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                marker,
                Style::default()
                    .fg(if is_sel { sel_fg } else { ACCENT })
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                col,
                Style::default()
                    .fg(if is_sel { sel_fg } else { IVORY })
                    .bg(bg)
                    .add_modifier(if is_sel {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(
                d,
                Style::default()
                    .fg(if is_sel { sel_fg } else { STONE })
                    .bg(bg),
            ),
            Span::styled(
                badge,
                Style::default()
                    .fg(if is_sel { sel_fg } else { SAGE })
                    .bg(bg),
            ),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        ]));
    }
    rows.push(Line::from(Span::styled(rule, Style::default().fg(ACCENT))));
    frame.render_widget(Paragraph::new(rows), area);
}

fn filtered_models<'a>(all: &'a [String], filter: &str) -> Vec<&'a str> {
    let q = filter.trim().to_lowercase();
    all.iter()
        .map(String::as_str)
        .filter(|m| q.is_empty() || m.to_lowercase().contains(&q))
        .collect()
}

fn render_model_picker(
    frame: &mut Frame,
    area: Rect,
    all: &[String],
    filter: &str,
    active_model: &str,
    sel: usize,
) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let items = filtered_models(all, filter);
    let rule_len = area.width.saturating_sub(2) as usize;
    let rule = format!(" {}", "─".repeat(rule_len));
    let max_rows = (area.height.saturating_sub(3) as usize).max(1);
    let sel = sel.min(items.len().saturating_sub(1));
    let start = sel.saturating_sub(max_rows.saturating_sub(1));
    let end = (start + max_rows).min(items.len());
    let row_w = area.width.saturating_sub(2) as usize;
    let sel_fg = Color::Rgb(22, 19, 17);

    let header = if filter.is_empty() {
        format!(
            "  Select model ({} via magpie · type to filter · Enter apply)",
            all.len()
        )
    } else {
        format!("  Select model  filter: {filter}  ({} matches)", items.len())
    };
    let mut rows = vec![
        Line::from(Span::styled(rule.clone(), Style::default().fg(ACCENT))),
        Line::from(Span::styled(
            header,
            Style::default()
                .fg(ACCENT_PEAK)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    if items.is_empty() {
        rows.push(Line::from(Span::styled(
            "   (no matching models)",
            Style::default().fg(MUTED),
        )));
    } else {
        for (offset, name) in items[start..end].iter().enumerate() {
            let is_sel = start + offset == sel;
            let is_current = *name == active_model;
            let bg = if is_sel { ACCENT } else { Color::Reset };
            let marker = if is_sel { " › " } else { "   " };
            let badge = if is_current { "  (active)" } else { "" };
            let used = display_width(marker) + display_width(badge);
            let label = clip_str(name, row_w.saturating_sub(used + 2));
            let pad = row_w.saturating_sub(used + display_width(&label));
            rows.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    marker,
                    Style::default()
                        .fg(if is_sel { sel_fg } else { ACCENT })
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label,
                    Style::default()
                        .fg(if is_sel { sel_fg } else { IVORY })
                        .bg(bg)
                        .add_modifier(if is_sel {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(
                    badge,
                    Style::default()
                        .fg(if is_sel { sel_fg } else { SAGE })
                        .bg(bg),
                ),
                Span::styled(" ".repeat(pad), Style::default().bg(bg)),
            ]));
        }
    }
    rows.push(Line::from(Span::styled(rule, Style::default().fg(ACCENT))));
    frame.render_widget(Paragraph::new(rows), area);
}

/// Borderless Codex-style interactive session picker rendered above the footer for `/resume`.
fn render_resume_picker(
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

fn render_rewind_picker(frame: &mut Frame, area: Rect, turns: &[String], sel: usize) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let rule_len = area.width.saturating_sub(2) as usize;
    let rule = format!(" {}", "─".repeat(rule_len));
    let max_rows = (area.height.saturating_sub(3) as usize).max(1);
    let sel = sel.min(turns.len().saturating_sub(1));
    let start = sel.saturating_sub(max_rows.saturating_sub(1));
    let end = (start + max_rows).min(turns.len());
    let row_w = area.width.saturating_sub(2) as usize;
    let sel_fg = Color::Rgb(22, 19, 17);

    let mut rows = vec![
        Line::from(Span::styled(rule.clone(), Style::default().fg(ACCENT))),
        Line::from(Span::styled(
            "  Rewind to turn  (Enter rewind & edit prompt · f fork · Esc cancel)",
            Style::default()
                .fg(ACCENT_PEAK)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    for (offset, prompt) in turns[start..end].iter().enumerate() {
        let idx = start + offset;
        let is_sel = idx == sel;
        let bg = if is_sel { ACCENT } else { Color::Reset };
        let marker = if is_sel { " › " } else { "   " };
        let num = format!("#{:<3}", idx + 1);
        let one_line = prompt.replace('\n', " ");
        let used = display_width(marker) + display_width(&num);
        let label = clip_str(&one_line, row_w.saturating_sub(used + 2));
        let pad = row_w.saturating_sub(used + display_width(&label));
        rows.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                marker,
                Style::default()
                    .fg(if is_sel { sel_fg } else { ACCENT })
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                num,
                Style::default()
                    .fg(if is_sel { sel_fg } else { MUTED })
                    .bg(bg),
            ),
            Span::styled(
                label,
                Style::default()
                    .fg(if is_sel { sel_fg } else { IVORY })
                    .bg(bg)
                    .add_modifier(if is_sel {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        ]));
    }
    rows.push(Line::from(Span::styled(rule, Style::default().fg(ACCENT))));
    frame.render_widget(Paragraph::new(rows), area);
}

fn render_history_picker(
    frame: &mut Frame,
    area: Rect,
    all: &[String],
    filter: &str,
    sel: usize,
) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let items = filtered_models(all, filter);
    let rule_len = area.width.saturating_sub(2) as usize;
    let rule = format!(" {}", "─".repeat(rule_len));
    let max_rows = (area.height.saturating_sub(3) as usize).max(1);
    let sel = sel.min(items.len().saturating_sub(1));
    let start = sel.saturating_sub(max_rows.saturating_sub(1));
    let end = (start + max_rows).min(items.len());
    let row_w = area.width.saturating_sub(2) as usize;
    let sel_fg = Color::Rgb(22, 19, 17);

    let header = if filter.is_empty() {
        format!("  Prompt history ({} saved · type to search · Enter fill)", all.len())
    } else {
        format!("  Prompt history  query: {filter}  ({} matches)", items.len())
    };
    let mut rows = vec![
        Line::from(Span::styled(rule.clone(), Style::default().fg(ACCENT))),
        Line::from(Span::styled(
            header,
            Style::default()
                .fg(ACCENT_PEAK)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    if items.is_empty() {
        rows.push(Line::from(Span::styled(
            "   (no matching prompts)",
            Style::default().fg(MUTED),
        )));
    } else {
        for (offset, entry) in items[start..end].iter().enumerate() {
            let is_sel = start + offset == sel;
            let bg = if is_sel { ACCENT } else { Color::Reset };
            let marker = if is_sel { " › " } else { "   " };
            let one_line = entry.replace('\n', " ↵ ");
            let label = clip_str(&one_line, row_w.saturating_sub(display_width(marker) + 2));
            let pad = row_w.saturating_sub(display_width(marker) + display_width(&label));
            rows.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    marker,
                    Style::default()
                        .fg(if is_sel { sel_fg } else { ACCENT })
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label,
                    Style::default()
                        .fg(if is_sel { sel_fg } else { IVORY })
                        .bg(bg)
                        .add_modifier(if is_sel {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(" ".repeat(pad), Style::default().bg(bg)),
            ]));
        }
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
        return clip_str(&format!("{srv}/{target}"), 92);
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
        return clip_str(&summary.replace('\n', " "), 92);
    }
    let raw = match name {
        "bash" => args["command"].as_str().unwrap_or(""),
        "read" | "edit" | "write" => args["path"].as_str().unwrap_or(""),
        "grep" | "glob" => args["pattern"].as_str().unwrap_or(""),
        "skill" => args["name"].as_str().unwrap_or("list"),
        "subagent" => args["task"].as_str().unwrap_or(""),
        "papercut" => args["component"].as_str().unwrap_or(""),
        _ => "",
    };
    clip_str(&raw.replace('\n', " "), 92)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
