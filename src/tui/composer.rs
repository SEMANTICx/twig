//! Borderless Claude-Code-style multiline composer with generous breathing margins:
//! - Inset top & bottom hairline rules (`─`) and warm terracotta `❯` glyph
//! - CJK/emoji-safe cursor math and readline shortcuts
//! - Codex-style large-paste folding pills (`[Pasted #1 · 12 lines]`)
//! - Charmbracelet/Lip-Gloss rounded popups for `/` commands and `@` repo file mentions

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use super::markdown::{
    display_width, wrap_plain, ACCENT, ACCENT_PEAK, HAIRLINE, IVORY, MUTED, STONE, SURFACE_HI,
};

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/steer ", "Queue steering guidance for the active run"),
    ("/cancel", "Interrupt active turn and child processes"),
    ("/approve ", "Approve pending tool call by ID"),
    ("/deny ", "Deny pending tool call by ID"),
    ("/expand", "Toggle compact/expanded tool outputs (Ctrl+O)"),
    ("/find ", "Search transcript history (Ctrl+F)"),
    ("/copy", "Copy latest assistant code block (Alt+Y)"),
    ("/editor", "Compose prompt in $EDITOR (Ctrl+G)"),
    ("/clear", "Clear local transcript view"),
    ("/help", "Show keyboard shortcuts (or press ?)"),
    ("/quit", "Exit twig chat"),
];

pub enum PopupKind {
    Slash(Vec<(String, String)>),
    Mention {
        at_byte: usize,
        items: Vec<String>,
    },
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

    pub fn accept_popup(&mut self) -> bool {
        let Some(popup) = self.active_popup() else {
            return false;
        };
        match popup {
            PopupKind::Slash(items) => {
                let idx = self.popup_sel.min(items.len().saturating_sub(1));
                if let Some((cmd, _)) = items.get(idx) {
                    self.text = cmd.clone();
                    self.cursor = self.text.len();
                    self.popup_sel = 0;
                    return true;
                }
            }
            PopupKind::Mention { at_byte, items } => {
                let idx = self.popup_sel.min(items.len().saturating_sub(1));
                if let Some(path) = items.get(idx) {
                    let replacement = format!("@{path} ");
                    self.text.replace_range(at_byte..self.cursor, &replacement);
                    self.cursor = at_byte + replacement.len();
                    self.popup_sel = 0;
                    return true;
                }
            }
        }
        false
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
            let matches: Vec<(String, String)> = SLASH_COMMANDS
                .iter()
                .filter(|(cmd, _)| cmd.starts_with(self.text.as_str()))
                .map(|(c, d)| ((*c).to_owned(), (*d).to_owned()))
                .collect();
            if !matches.is_empty() {
                return Some(PopupKind::Slash(matches));
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
                    .take(8)
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
        // 2-col left margin + `❯ ` (2 cols) + 2-col right margin = 6 cols chrome
        let text_w = total_width.saturating_sub(6).max(16);
        let lines = wrap_plain(&self.text, text_w);
        // 1 top hairline + content lines (1..=6) + 1 bottom hairline
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

    /// Borderless minimal popup above the composer for `/` commands or `@` mentions.
    pub fn render_popup(&self, frame: &mut Frame, composer_area: Rect, transcript_h: u16) {
        let Some(popup) = self.active_popup() else {
            return;
        };
        if transcript_h < 4 || composer_area.width < 20 {
            return;
        }
        let popup_w = composer_area.width.saturating_sub(4).min(68);
        let inner_w = popup_w as usize;
        let (title, rows): (&str, Vec<Line<'static>>) = match popup {
            PopupKind::Slash(items) => {
                let sel = self.popup_sel.min(items.len().saturating_sub(1));
                let lines = items
                    .into_iter()
                    .enumerate()
                    .map(|(i, (cmd, desc))| {
                        let active = i == sel;
                        let bg = if active { SURFACE_HI } else { Color::Reset };
                        let marker = if active { " ▸ " } else { "   " };
                        let cmd_col = format!("{cmd:<12}");
                        let used = display_width(marker) + display_width(&cmd_col);
                        let desc_clipped: String = desc
                            .chars()
                            .take(inner_w.saturating_sub(used + 1))
                            .collect();
                        let pad = inner_w.saturating_sub(used + display_width(&desc_clipped));
                        Line::from(vec![
                            Span::styled(marker, Style::default().fg(ACCENT).bg(bg)),
                            Span::styled(
                                cmd_col,
                                Style::default()
                                    .fg(if active { ACCENT_PEAK } else { ACCENT })
                                    .bg(bg)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                desc_clipped,
                                Style::default()
                                    .fg(if active { IVORY } else { STONE })
                                    .bg(bg),
                            ),
                            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
                        ])
                    })
                    .collect();
                (" Commands · ↑/↓ · Tab ", lines)
            }
            PopupKind::Mention { items, .. } => {
                let sel = self.popup_sel.min(items.len().saturating_sub(1));
                let lines = items
                    .into_iter()
                    .enumerate()
                    .map(|(i, path)| {
                        let active = i == sel;
                        let bg = if active { SURFACE_HI } else { Color::Reset };
                        let marker = if active { " ▸ ◇ " } else { "   ◇ " };
                        let used = display_width(marker);
                        let clipped: String = path
                            .chars()
                            .take(inner_w.saturating_sub(used + 1))
                            .collect();
                        let pad = inner_w.saturating_sub(used + display_width(&clipped));
                        Line::from(vec![
                            Span::styled(
                                marker,
                                Style::default()
                                    .fg(if active { ACCENT } else { MUTED })
                                    .bg(bg),
                            ),
                            Span::styled(
                                clipped,
                                Style::default()
                                    .fg(if active { IVORY } else { STONE })
                                    .bg(bg),
                            ),
                            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
                        ])
                    })
                    .collect();
                (" Files · ↑/↓ · Tab ", lines)
            }
        };

        let popup_h = (rows.len() as u16 + 1).min(transcript_h);
        let popup_area = Rect {
            x: composer_area.x + 2,
            y: composer_area.y.saturating_sub(popup_h),
            width: popup_w,
            height: popup_h,
        };
        let mut all = Vec::with_capacity(rows.len() + 1);
        let top_fill = inner_w.saturating_sub(display_width(title) + 2);
        all.push(Line::from(vec![
            Span::styled("──", Style::default().fg(HAIRLINE)),
            Span::styled(title, Style::default().fg(MUTED)),
            Span::styled("─".repeat(top_fill), Style::default().fg(HAIRLINE)),
        ]));
        all.extend(rows);
        frame.render_widget(Clear, popup_area);
        frame.render_widget(Paragraph::new(all), popup_area);
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
