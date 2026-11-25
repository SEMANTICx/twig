//! Borderless Claude-Code/Codex multiline composer with:
//! - Inset top & bottom hairline rules (`─`) and warm terracotta `❯` glyph
//! - CJK/emoji-safe cursor math and readline shortcuts
//! - Codex-style large-paste folding pills (`[Pasted #1 · 12 lines]`)
//! - Codex-style borderless scrollable `/` command & `@` file mention palette
//!   with full-width accent selection bar

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use super::markdown::{
    display_width, wrap_plain, ACCENT, ACCENT_PEAK, IVORY, MUTED, STONE, SURFACE_HI,
};

/// Codex & Claude Code parity slash command catalog.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/new", "start a new chat session"),
    ("/resume", "browse and resume a previous session"),
    ("/rename ", "rename the current session"),
    ("/status", "show session telemetry, usage, and context"),
    ("/diff", "show current git diff with line numbers"),
    ("/review", "run independent read-only review on current changes"),
    ("/skills", "list available playbooks and project skills"),
    ("/model ", "view or switch the active model (or reset)"),
    ("/compact", "compact conversation context now"),
    ("/config", "show effective harness configuration"),
    ("/steer ", "queue steering guidance for the active run"),
    ("/cancel", "interrupt active turn and child processes"),
    ("/approve ", "approve pending tool call by ID"),
    ("/deny ", "deny pending tool call by ID"),
    ("/expand", "toggle expanded/collapsed tool outputs (Ctrl+O)"),
    ("/find ", "search transcript history (Ctrl+F)"),
    ("/copy", "copy latest assistant code block (Alt+Y)"),
    ("/editor", "compose prompt in $EDITOR (Ctrl+G)"),
    ("/mouse", "toggle mouse capture for native text select (Alt+M)"),
    ("/clear", "clear local transcript view"),
    ("/help", "show keyboard shortcuts (?)"),
    ("/quit", "exit twig chat"),
];

pub enum PopupKind {
    Slash(Vec<(String, String)>),
    Mention {
        at_byte: usize,
        items: Vec<String>,
    },
}

pub enum PopupAccept {
    None,
    Filled,
    Execute(String),
}

pub struct Composer {
    text: String,
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    draft: String,
    pastes: HashMap<String, String>,
    next_paste_id: usize,
    repo_files: Vec<String>,
    popup_sel: usize,
}

impl Composer {
    pub fn new(repo: &str) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            draft: String::new(),
            pastes: HashMap::new(),
            next_paste_id: 1,
            repo_files: list_repo_files(repo),
            popup_sel: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn draft_expanded(&self) -> String {
        self.expand_pastes(&self.text)
    }

    pub fn set(&mut self, value: &str) {
        self.text = value.to_owned();
        self.cursor = self.text.len();
        self.history_idx = None;
        self.pastes.clear();
        self.popup_sel = 0;
    }

    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.history_idx = None;
        self.popup_sel = 0;
    }

    /// Codex-style paste handling: large or multiline pastes fold into an inline pill.
    pub fn handle_paste(&mut self, pasted: &str) -> Option<String> {
        let normalized = pasted.replace("\r\n", "\n").replace('\r', "\n");
        let line_count = normalized.lines().count().max(1);
        if line_count >= 5 || normalized.len() > 240 {
            let id = self.next_paste_id;
            self.next_paste_id += 1;
            let token = format!("[Pasted #{id} · {line_count} lines]");
            self.pastes.insert(token.clone(), normalized);
            self.text.insert_str(self.cursor, &token);
            self.cursor += token.len();
            self.history_idx = None;
            Some(token)
        } else {
            self.text.insert_str(self.cursor, &normalized);
            self.cursor += normalized.len();
            self.history_idx = None;
            None
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some(pill) = self
            .pastes
            .keys()
            .find(|k| self.text[..self.cursor].ends_with(k.as_str()))
            .cloned()
        {
            let start = self.cursor - pill.len();
            self.text.drain(start..self.cursor);
            self.cursor = start;
            self.pastes.remove(&pill);
            self.history_idx = None;
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        self.history_idx = None;
        self.popup_sel = 0;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.text[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.cursor);
        self.text.drain(self.cursor..next);
        self.history_idx = None;
    }

    pub fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prefix = &self.text[..self.cursor];
        let trimmed = prefix.trim_end();
        let cut = trimmed
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.text.drain(cut..self.cursor);
        self.cursor = cut;
        self.history_idx = None;
    }

    pub fn clear_to_start(&mut self) {
        if self.cursor > 0 {
            self.text.drain(..self.cursor);
            self.cursor = 0;
            self.history_idx = None;
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            if let Some(ch) = self.text[self.cursor..].chars().next() {
                self.cursor += ch.len_utf8();
            }
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn has_popup(&self) -> bool {
        self.active_popup().is_some()
    }

    pub fn popup_move(&mut self, delta: isize) -> bool {
        let Some(popup) = self.active_popup() else {
            return false;
        };
        let len = match &popup {
            PopupKind::Slash(v) => v.len(),
            PopupKind::Mention { items, .. } => items.len(),
        };
        if len == 0 {
            return false;
        }
        let cur = self.popup_sel.min(len - 1) as isize;
        self.popup_sel = (cur + delta).rem_euclid(len as isize) as usize;
        true
    }

    /// Tab completion: fills the highlighted slash command or file path into the composer.
    pub fn accept_popup(&mut self) -> bool {
        matches!(self.accept_popup_inner(false), PopupAccept::Filled)
    }

    /// Codex-style Enter on popup: executes zero-argument slash commands immediately,
    /// fills argument-taking commands or `@file` mentions into the composer.
    pub fn accept_popup_on_enter(&mut self) -> PopupAccept {
        self.accept_popup_inner(true)
    }

    fn accept_popup_inner(&mut self, execute_zero_arg: bool) -> PopupAccept {
        let Some(popup) = self.active_popup() else {
            return PopupAccept::None;
        };
        match popup {
            PopupKind::Slash(items) => {
                let idx = self.popup_sel.min(items.len().saturating_sub(1));
                if let Some((cmd, _)) = items.get(idx) {
                    if execute_zero_arg && !cmd.ends_with(' ') {
                        let command = cmd.clone();
                        self.text.clear();
                        self.cursor = 0;
                        self.popup_sel = 0;
                        return PopupAccept::Execute(command);
                    }
                    self.text = cmd.clone();
                    self.cursor = self.text.len();
                    self.popup_sel = 0;
                    return PopupAccept::Filled;
                }
            }
            PopupKind::Mention { at_byte, items } => {
                let idx = self.popup_sel.min(items.len().saturating_sub(1));
                if let Some(path) = items.get(idx) {
                    let replacement = format!("@{path} ");
                    self.text.replace_range(at_byte..self.cursor, &replacement);
                    self.cursor = at_byte + replacement.len();
                    self.popup_sel = 0;
                    return PopupAccept::Filled;
                }
            }
        }
        PopupAccept::None
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_idx {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(next);
        self.text = self.history[next].clone();
        self.cursor = self.text.len();
    }

    pub fn history_down(&mut self) {
        let Some(idx) = self.history_idx else { return };
        if idx + 1 < self.history.len() {
            let next = idx + 1;
            self.history_idx = Some(next);
            self.text = self.history[next].clone();
        } else {
            self.history_idx = None;
            self.text = std::mem::take(&mut self.draft);
        }
        self.cursor = self.text.len();
    }

    pub fn take_submitted(&mut self) -> String {
        let display = std::mem::take(&mut self.text);
        let expanded = self.expand_pastes(&display);
        self.cursor = 0;
        self.history_idx = None;
        self.draft.clear();
        self.pastes.clear();
        self.popup_sel = 0;
        if !expanded.trim().is_empty() && self.history.last() != Some(&expanded) {
            self.history.push(expanded.clone());
        }
        expanded
    }

    fn expand_pastes(&self, input: &str) -> String {
        let mut out = input.to_owned();
        for (token, payload) in &self.pastes {
            out = out.replace(token, payload);
        }
        out
    }

    fn active_popup(&self) -> Option<PopupKind> {
        if self.text.starts_with('/') && !self.text.contains('\n') && !self.text.contains(' ') {
            let q = self.text.to_lowercase();
            let mut matches: Vec<(usize, String, String)> = SLASH_COMMANDS
                .iter()
                .filter_map(|(cmd, desc)| {
                    let low_cmd = cmd.trim_end().to_lowercase();
                    if low_cmd.starts_with(&q) {
                        Some((0, (*cmd).to_owned(), (*desc).to_owned()))
                    } else if low_cmd.contains(&q[1..]) || desc.to_lowercase().contains(&q[1..]) {
                        Some((1, (*cmd).to_owned(), (*desc).to_owned()))
                    } else {
                        None
                    }
                })
                .collect();
            matches.sort_by_key(|(rank, _, _)| *rank);
            let out: Vec<(String, String)> =
                matches.into_iter().map(|(_, c, d)| (c, d)).collect();
            if !out.is_empty() {
                return Some(PopupKind::Slash(out));
            }
        }
        let before = &self.text[..self.cursor];
        let token_start = before
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        let token = &before[token_start..];
        if let Some(query) = token.strip_prefix('@') {
            if !self.repo_files.is_empty() {
                let q = query.to_lowercase();
                let mut scored: Vec<(usize, &String)> = self
                    .repo_files
                    .iter()
                    .filter_map(|p| {
                        let low = p.to_lowercase();
                        if q.is_empty() || low.starts_with(&q) {
                            Some((0, p))
                        } else if let Some(pos) = low.find(&q) {
                            Some((pos + 1, p))
                        } else {
                            None
                        }
                    })
                    .collect();
                scored.sort_by_key(|(rank, p)| (*rank, p.len()));
                let items: Vec<String> = scored
                    .into_iter()
                    .take(12)
                    .map(|(_, p)| p.clone())
                    .collect();
                if !items.is_empty() {
                    return Some(PopupKind::Mention {
                        at_byte: token_start,
                        items,
                    });
                }
            }
        }
        None
    }

    pub fn desired_height(&self, total_width: usize) -> u16 {
        let text_w = total_width.saturating_sub(6).max(16);
        let lines = wrap_plain(&self.text, text_w);
        (lines.len() as u16 + 2).clamp(3, 8)
    }

    /// Claude Code hairline composer with 2-col horizontal breathing margin (`  ❯  `).
    pub fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        rule_color: Color,
        prompt_color: Color,
        placeholder: &str,
        show_cursor: bool,
    ) {
        if area.height < 3 || area.width < 10 {
            return;
        }
        let rule_len = area.width.saturating_sub(2) as usize;
        let rule_line = format!(" {}", "─".repeat(rule_len));
        let rule_style = Style::default().fg(rule_color);

        frame.render_widget(
            Paragraph::new(Span::styled(rule_line.clone(), rule_style)),
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
        );
        frame.render_widget(
            Paragraph::new(Span::styled(rule_line, rule_style)),
            Rect {
                x: area.x,
                y: area.y + area.height - 1,
                width: area.width,
                height: 1,
            },
        );

        let body_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: area.height.saturating_sub(2),
        };
        let text_w = area.width.saturating_sub(6).max(12) as usize;

        let rendered_lines: Vec<Line<'static>> = if self.text.is_empty() {
            vec![Line::from(vec![
                Span::styled(
                    "  ❯ ",
                    Style::default()
                        .fg(prompt_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(placeholder.to_owned(), Style::default().fg(MUTED)),
            ])]
        } else {
            wrap_plain(&self.text, text_w)
                .into_iter()
                .enumerate()
                .map(|(idx, segment)| {
                    let prefix = if idx == 0 { "  ❯ " } else { "    " };
                    let mut spans = vec![Span::styled(
                        prefix,
                        Style::default()
                            .fg(prompt_color)
                            .add_modifier(Modifier::BOLD),
                    )];
                    spans.extend(style_composer_segment(&segment));
                    Line::from(spans)
                })
                .collect()
        };

        let visible_h = body_area.height as usize;
        let scroll_y = rendered_lines.len().saturating_sub(visible_h) as u16;
        frame.render_widget(
            Paragraph::new(rendered_lines).scroll((scroll_y, 0)),
            body_area,
        );

        if show_cursor {
            let before_cursor = &self.text[..self.cursor];
            let wrapped_before = wrap_plain(before_cursor, text_w);
            let cur_row = wrapped_before
                .len()
                .saturating_sub(1)
                .saturating_sub(scroll_y as usize) as u16;
            let cur_col = wrapped_before
                .last()
                .map(|l| display_width(l) as u16)
                .unwrap_or(0);
            if cur_row < body_area.height {
                frame.set_cursor_position((
                    body_area.x + 4 + cur_col.min(text_w as u16),
                    body_area.y + cur_row,
                ));
            }
        }
    }

    /// Codex-style borderless scrollable command/file popup with full-width accent selection bar.
    pub fn render_popup(&self, frame: &mut Frame, composer_area: Rect, transcript_h: u16) {
        let Some(popup) = self.active_popup() else {
            return;
        };
        let max_rows = (transcript_h.saturating_sub(1) as usize).clamp(1, 8);
        if composer_area.width < 20 {
            return;
        }
        let popup_w = composer_area.width.saturating_sub(2) as usize;
        let sel_bg = ACCENT;
        let sel_fg = Color::Rgb(22, 19, 17);

        let rows: Vec<Line<'static>> = match popup {
            PopupKind::Slash(items) => {
                let sel = self.popup_sel.min(items.len().saturating_sub(1));
                let start = sel.saturating_sub(max_rows.saturating_sub(1));
                let end = (start + max_rows).min(items.len());
                items[start..end]
                    .iter()
                    .enumerate()
                    .map(|(offset, (cmd, desc))| {
                        let active = start + offset == sel;
                        let bg = if active { sel_bg } else { Color::Reset };
                        let marker = if active { " › " } else { "   " };
                        let cmd_col = format!("{:<14}", cmd.trim_end());
                        let used = display_width(marker) + display_width(&cmd_col);
                        let desc_clipped: String = desc
                            .chars()
                            .take(popup_w.saturating_sub(used + 1))
                            .collect();
                        let pad = popup_w.saturating_sub(used + display_width(&desc_clipped));
                        Line::from(vec![
                            Span::styled(
                                marker,
                                Style::default()
                                    .fg(if active { sel_fg } else { ACCENT })
                                    .bg(bg)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                cmd_col,
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
                                desc_clipped,
                                Style::default()
                                    .fg(if active { sel_fg } else { STONE })
                                    .bg(bg),
                            ),
                            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
                        ])
                    })
                    .collect()
            }
            PopupKind::Mention { items, .. } => {
                let sel = self.popup_sel.min(items.len().saturating_sub(1));
                let start = sel.saturating_sub(max_rows.saturating_sub(1));
                let end = (start + max_rows).min(items.len());
                items[start..end]
                    .iter()
                    .enumerate()
                    .map(|(offset, path)| {
                        let active = start + offset == sel;
                        let bg = if active { sel_bg } else { Color::Reset };
                        let marker = if active { " › ◇ " } else { "   ◇ " };
                        let used = display_width(marker);
                        let clipped: String = path
                            .chars()
                            .take(popup_w.saturating_sub(used + 1))
                            .collect();
                        let pad = popup_w.saturating_sub(used + display_width(&clipped));
                        Line::from(vec![
                            Span::styled(
                                marker,
                                Style::default()
                                    .fg(if active { sel_fg } else { MUTED })
                                    .bg(bg)
                                    .add_modifier(if active {
                                        Modifier::BOLD
                                    } else {
                                        Modifier::empty()
                                    }),
                            ),
                            Span::styled(
                                clipped,
                                Style::default()
                                    .fg(if active { sel_fg } else { IVORY })
                                    .bg(bg)
                                    .add_modifier(if active {
                                        Modifier::BOLD
                                    } else {
                                        Modifier::empty()
                                    }),
                            ),
                            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
                        ])
                    })
                    .collect()
            }
        };

        let popup_h = rows.len() as u16;
        let popup_area = Rect {
            x: composer_area.x + 1,
            y: composer_area.y.saturating_sub(popup_h),
            width: popup_w as u16,
            height: popup_h,
        };
        frame.render_widget(Clear, popup_area);
        frame.render_widget(Paragraph::new(rows), popup_area);
    }
}

fn style_composer_segment(seg: &str) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut rest = seg;
    while let Some(start) = rest.find("[Pasted #") {
        if start > 0 {
            out.push(Span::styled(
                rest[..start].to_owned(),
                Style::default().fg(IVORY),
            ));
        }
        if let Some(rel_end) = rest[start..].find(']') {
            let end = start + rel_end + 1;
            out.push(Span::styled(
                rest[start..end].to_owned(),
                Style::default()
                    .fg(ACCENT_PEAK)
                    .bg(SURFACE_HI)
                    .add_modifier(Modifier::BOLD),
            ));
            rest = &rest[end..];
        } else {
            break;
        }
    }
    if !rest.is_empty() {
        out.push(Span::styled(rest.to_owned(), Style::default().fg(IVORY)));
    }
    out
}

fn list_repo_files(repo: &str) -> Vec<String> {
    let dir = Path::new(repo);
    if !dir.is_dir() {
        return Vec::new();
    }
    if let Ok(out) = Command::new("git")
        .args([
            "-C",
            repo,
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(600)
                .map(str::to_owned)
                .collect();
        }
    }
    Vec::new()
}
