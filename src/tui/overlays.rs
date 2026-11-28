//! Interactive borderless TUI overlays: approval selector, todo progress strip,
//! and unified fuzzy/list pickers (`/resume`, `/model`, `/effort`, `/rewind`, `Ctrl+R`).

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use crate::runtime::session::SessionInfo;
use super::highlight::DiffGutterState;
use super::strips::{compact_subagent_task, SubagentEntry, SubagentStatus};
use super::theme::{
    clip_width, display_width, fmt_elapsed_compact, summarize_tool_args, title_tool_name,
    wrap_plain, ACCENT, ACCENT_PEAK, AMBER, BASH_MODE_COLOR, IVORY, MEMORY_MODE_COLOR, MUTED,
    ON_ACCENT_FG, ROSE, SAGE, STONE, SURFACE, SURFACE_HI,
};

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
        note: Option<String>,
    },
}

/// Self-contained pending approval queue with multi-line diff/command preview
/// and Claude Code inline rejection feedback typing on option 3.
#[derive(Default)]
pub struct ApprovalQueue {
    items: VecDeque<PendingApproval>,
    sel: usize,
    typing_note: bool,
    note_buf: String,
}

impl ApprovalQueue {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.sel = 0;
        self.typing_note = false;
        self.note_buf.clear();
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        let Some(pending) = self.items.front() else {
            return 0;
        };
        let prev_n = build_approval_preview(&pending.name, &pending.args, width.saturating_sub(6) as usize).len();
        (prev_n as u16 + 6).clamp(7, 13)
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
        self.typing_note = false;
        self.note_buf.clear();
        Some((name, summary))
    }

    pub fn resolve_id(&mut self, id: &str) {
        self.items.retain(|a| a.id != id);
        self.sel = 0;
        self.typing_note = false;
        self.note_buf.clear();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ApprovalKeyResult {
        let Some(pending) = self.items.front() else {
            return ApprovalKeyResult::Ignored;
        };
        let id = pending.id.clone();

        if self.typing_note {
            match key.code {
                KeyCode::Esc => {
                    self.typing_note = false;
                    self.note_buf.clear();
                    return ApprovalKeyResult::Consumed;
                }
                KeyCode::Up => {
                    self.typing_note = false;
                    self.sel = 1;
                    return ApprovalKeyResult::Consumed;
                }
                KeyCode::Backspace => {
                    self.note_buf.pop();
                    return ApprovalKeyResult::Consumed;
                }
                KeyCode::Enter => {
                    let note = std::mem::take(&mut self.note_buf);
                    self.typing_note = false;
                    let trimmed = note.trim().to_owned();
                    return ApprovalKeyResult::Resolve {
                        id,
                        allow: false,
                        remember: false,
                        note: (!trimmed.is_empty()).then_some(trimmed),
                    };
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.note_buf.push(c);
                    return ApprovalKeyResult::Consumed;
                }
                _ => return ApprovalKeyResult::Consumed,
            }
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = self.sel.saturating_sub(1);
                ApprovalKeyResult::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(2);
                ApprovalKeyResult::Consumed
            }
            KeyCode::Tab => {
                self.sel = 2;
                self.typing_note = true;
                ApprovalKeyResult::Consumed
            }
            KeyCode::Char('1') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                ApprovalKeyResult::Resolve {
                    id,
                    allow: true,
                    remember: false,
                    note: None,
                }
            }
            KeyCode::Char('2') | KeyCode::Char('a') | KeyCode::Char('A') => {
                ApprovalKeyResult::Resolve {
                    id,
                    allow: true,
                    remember: true,
                    note: None,
                }
            }
            KeyCode::Char('3') => {
                self.sel = 2;
                self.typing_note = true;
                ApprovalKeyResult::Consumed
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                ApprovalKeyResult::Resolve {
                    id,
                    allow: false,
                    remember: false,
                    note: None,
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
                    note: None,
                }
            }
            KeyCode::Char(c)
                if self.sel == 2 && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.typing_note = true;
                self.note_buf.push(c);
                ApprovalKeyResult::Consumed
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
            pending,
            self.sel,
            self.typing_note,
            &self.note_buf,
        );
    }
}

fn build_approval_preview(name: &str, args: &Value, max_w: usize) -> Vec<Line<'static>> {
    let w = max_w.max(16);
    match name {
        "edit" => {
            let path = args["path"].as_str().unwrap_or("file");
            let old_s = args["old_string"].as_str().unwrap_or("");
            let new_s = args["new_string"].as_str().unwrap_or("");
            let mut raw = Vec::new();
            for l in old_s.lines().take(3) {
                raw.push(format!("-{l}"));
            }
            for l in new_s.lines().take(3) {
                raw.push(format!("+{l}"));
            }
            let refs: Vec<&str> = raw.iter().map(String::as_str).collect();
            let mut gutter = DiffGutterState::default();
            let mut lines = vec![Line::from(vec![
                Span::styled("  ✎ ", Style::default().fg(ACCENT)),
                Span::styled(
                    clip_width(path, w.saturating_sub(4)),
                    Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
                ),
            ])];
            for spans in gutter.render_block(&refs, w.saturating_sub(2)).into_iter().take(5) {
                let mut row = vec![Span::raw("  ")];
                row.extend(spans);
                lines.push(Line::from(row));
            }
            lines
        }
        "write" | "apply_patch" => {
            let path = args["path"].as_str().unwrap_or("patch");
            let body = args["content"]
                .as_str()
                .or_else(|| args["patch"].as_str())
                .unwrap_or("");
            let mut lines = vec![Line::from(vec![
                Span::styled("  + ", Style::default().fg(SAGE)),
                Span::styled(
                    clip_width(path, w.saturating_sub(4)),
                    Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
                ),
            ])];
            for raw in body.lines().take(4) {
                let shown = clip_width(raw, w.saturating_sub(4));
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(shown, Style::default().fg(STONE).bg(SURFACE)),
                ]));
            }
            lines
        }
        _ => {
            let cmd = args["command"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| summarize_tool_args(name, args));
            wrap_plain(&cmd, w.saturating_sub(4))
                .into_iter()
                .take(4)
                .enumerate()
                .map(|(i, seg)| {
                    let prefix = if i == 0 { " $ " } else { "   " };
                    let text = format!("{prefix}{seg}");
                    let pad = w.saturating_sub(display_width(&text));
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(text, Style::default().fg(ACCENT_PEAK).bg(SURFACE)),
                        Span::styled(" ".repeat(pad), Style::default().bg(SURFACE)),
                    ])
                })
                .collect()
        }
    }
}

fn render_approval_panel(
    frame: &mut Frame,
    area: Rect,
    pending: &PendingApproval,
    sel: usize,
    typing_note: bool,
    note_buf: &str,
) {
    if area.height < 5 || area.width < 14 {
        return;
    }
    let compact = area.width < 56;
    let rule_len = area.width.saturating_sub(2) as usize;
    let rule = format!(" {}", "─".repeat(rule_len));
    let inner_w = area.width.saturating_sub(if compact { 2 } else { 4 }) as usize;
    let title = if pending.recovered {
        format!("  ▲ Recovered ({}) — NO replay", pending.id)
    } else if compact {
        format!(" ▲ Approve {}?", title_tool_name(&pending.name))
    } else {
        format!("  ▲ Approve {}? ({})", title_tool_name(&pending.name), pending.id)
    };

    let opt = |idx: usize, label: &str| -> Line<'static> {
        let active = sel == idx;
        let bg = if active { SURFACE_HI } else { Color::Reset };
        let marker = if active { " ▸ " } else { "   " };
        let fg = if active { IVORY } else { STONE };
        let shown = clip_width(label, inner_w.saturating_sub(display_width(marker)));
        let pad = inner_w.saturating_sub(display_width(marker) + display_width(&shown));
        Line::from(vec![
            Span::raw(if compact { " " } else { "  " }),
            Span::styled(
                marker,
                Style::default()
                    .fg(ACCENT)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                shown,
                Style::default().fg(fg).bg(bg).add_modifier(if active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        ])
    };

    let is_edit_gate = matches!(pending.name.as_str(), "edit" | "write" | "apply_patch");
    let (l0, l1, l2_default) = if compact {
        ("1. Allow once (y)", "2. Always allow (a)", "3. Deny / tell twig (3/n)")
    } else {
        (
            "1. Yes, allow once (y / Enter)",
            if is_edit_gate {
                "2. Yes, allow all edits for this session (a / Shift+Tab)"
            } else {
                "2. Always allow this pattern for this session (a)"
            },
            "3. No, and tell twig what to do differently (Tab / 3, or n to deny)",
        )
    };
    let l2 = if typing_note || !note_buf.is_empty() {
        format!("3. Tell twig instead: {note_buf}▎")
    } else {
        l2_default.to_owned()
    };

    let max_preview = (area.height as usize).saturating_sub(6).max(1);
    let mut rows = vec![
        Line::from(Span::styled(rule.clone(), Style::default().fg(AMBER))),
        Line::from(Span::styled(
            clip_width(&title, area.width as usize),
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        )),
    ];
    for p_line in build_approval_preview(&pending.name, &pending.args, inner_w)
        .into_iter()
        .take(max_preview)
    {
        rows.push(p_line);
    }
    rows.push(opt(0, l0));
    rows.push(opt(1, l1));
    rows.push(opt(2, &l2));
    if rows.len() < area.height as usize {
        rows.push(Line::from(Span::styled(rule, Style::default().fg(AMBER))));
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
    RewindTo {
        turns_back: usize,
        prompt: String,
        restore_code: bool,
    },
    ForkSession,
    FillComposer(String),
    ToggleResumeScope,
    ViewSubagent(String),
    CancelTask(String),
    SaveMemory {
        note: String,
        global: bool,
    },
}

/// Borderless Codex-style interactive overlay picker shared by `/resume`, `/model`, `/effort`, `/rewind`, `Ctrl+R`, `Ctrl+T`, and `#` memory.
pub enum OverlayPicker {
    Resume {
        sessions: Vec<SessionInfo>,
        all_repos: bool,
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
    Tasks {
        subagents: Vec<SubagentEntry>,
        tasks: Vec<Value>,
        sel: usize,
        expanded: bool,
    },
    MemorySave {
        note: String,
        sel: usize,
    },
}

impl OverlayPicker {
    pub fn resume(sessions: Vec<SessionInfo>, all_repos: bool) -> Self {
        Self::Resume {
            sessions,
            all_repos,
            sel: 0,
        }
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

    pub fn tasks(subagents: Vec<SubagentEntry>, tasks: Vec<Value>) -> Self {
        Self::Tasks {
            subagents,
            tasks,
            sel: 0,
            expanded: false,
        }
    }

    pub fn memory_save(note: String) -> Self {
        Self::MemorySave { note, sel: 0 }
    }

    pub fn hint(&self) -> &'static str {
        match self {
            Self::Resume { .. } => "↑/↓ select · Enter open · a toggle all repos · Esc close  ",
            Self::Model { .. } => "type to filter · ↑/↓ select · Enter apply · Esc close  ",
            Self::Effort { .. } => "↑/↓ select · Enter apply · Esc close  ",
            Self::Rewind { .. } => {
                "↑/↓ select · Enter chat only · r chat+code · f fork · Esc close  "
            }
            Self::History { .. } => "type to filter · ↑/↓ select · Enter fill · Esc close  ",
            Self::Tasks { .. } => "↑/↓ select · Enter details · x kill task · Esc close  ",
            Self::MemorySave { .. } => "↑/↓ scope · Enter save memory · Esc cancel  ",
        }
    }

    pub fn desired_height(&self) -> u16 {
        match self {
            Self::Tasks {
                subagents,
                tasks,
                expanded,
                ..
            } => {
                let n = subagents.len() + tasks.len();
                let extra = if *expanded { 4 } else { 0 };
                ((n.clamp(1, 6) + extra) as u16) + 3
            }
            Self::MemorySave { .. } => 6,
            _ => {
                let count = match self {
                    Self::Resume { sessions, .. } => sessions.len(),
                    Self::Model { all, filter, .. } => filtered_models(all, filter).len(),
                    Self::Effort { .. } => EFFORT_LEVELS.len(),
                    Self::Rewind { turns, .. } => turns.len(),
                    Self::History { all, filter, .. } => filtered_models(all, filter).len(),
                    _ => 1,
                };
                (count.clamp(1, 8) as u16) + 3
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        match self {
            Self::Resume { sessions, sel, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => PickerOutcome::Close,
                KeyCode::Char('a') => PickerOutcome::ToggleResumeScope,
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
                KeyCode::Char('r') => turns
                    .get(*sel)
                    .map(|prompt| PickerOutcome::RewindTo {
                        turns_back: turns.len() - *sel,
                        prompt: prompt.clone(),
                        restore_code: true,
                    })
                    .unwrap_or(PickerOutcome::Close),
                KeyCode::Enter => turns
                    .get(*sel)
                    .map(|prompt| PickerOutcome::RewindTo {
                        turns_back: turns.len() - *sel,
                        prompt: prompt.clone(),
                        restore_code: false,
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
            Self::Tasks {
                subagents,
                tasks,
                sel,
                expanded,
            } => {
                let total = subagents.len() + tasks.len();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => PickerOutcome::Close,
                    KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        PickerOutcome::Close
                    }
                    KeyCode::Up | KeyCode::Char('k') if total > 0 => {
                        *sel = sel.saturating_sub(1);
                        PickerOutcome::None
                    }
                    KeyCode::Down | KeyCode::Char('j') if total > 0 => {
                        *sel = (*sel + 1).min(total - 1);
                        PickerOutcome::None
                    }
                    KeyCode::Char('v' | 'V') | KeyCode::Right if *sel < subagents.len() => {
                        PickerOutcome::ViewSubagent(subagents[*sel].id.clone())
                    }
                    KeyCode::Enter | KeyCode::Char(' ') if total > 0 => {
                        *expanded = !*expanded;
                        PickerOutcome::None
                    }
                    KeyCode::Char('x' | 'X') | KeyCode::Delete if *sel >= subagents.len() => {
                        let t_idx = *sel - subagents.len();
                        tasks
                            .get(t_idx)
                            .and_then(|t| t["id"].as_str())
                            .map(|id| PickerOutcome::CancelTask(id.to_owned()))
                            .unwrap_or(PickerOutcome::None)
                    }
                    _ => PickerOutcome::None,
                }
            }
            Self::MemorySave { note, sel } => match key.code {
                KeyCode::Esc => PickerOutcome::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    *sel = 0;
                    PickerOutcome::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *sel = 1;
                    PickerOutcome::None
                }
                KeyCode::Char('1') => PickerOutcome::SaveMemory {
                    note: note.clone(),
                    global: false,
                },
                KeyCode::Char('2') => PickerOutcome::SaveMemory {
                    note: note.clone(),
                    global: true,
                },
                KeyCode::Enter => PickerOutcome::SaveMemory {
                    note: note.clone(),
                    global: *sel == 1,
                },
                _ => PickerOutcome::None,
            },
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        match self {
            Self::Resume {
                sessions,
                all_repos,
                sel,
            } => render_resume_picker(frame, area, sessions, *all_repos, *sel),
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
            Self::Tasks {
                subagents,
                tasks,
                sel,
                expanded,
            } => render_tasks_picker(frame, area, subagents, tasks, *sel, *expanded),
            Self::MemorySave { note, sel } => render_memory_picker(frame, area, note, *sel),
        }
    }
}

fn selected_window(total: usize, max_rows: usize, sel: usize) -> (usize, usize, usize) {
    let clamped_sel = sel.min(total.saturating_sub(1));
    let cap = max_rows.max(1);
    let start = clamped_sel.saturating_sub(cap.saturating_sub(1));
    let end = (start + cap).min(total);
    (clamped_sel, start, end)
}

fn render_picker_frame(
    frame: &mut Frame,
    area: Rect,
    rule_color: Color,
    header: Line<'static>,
    body: Vec<Line<'static>>,
) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let rule = format!(" {}", "─".repeat(area.width.saturating_sub(2) as usize));
    let mut rows = Vec::with_capacity(body.len() + 3);
    rows.push(Line::from(Span::styled(
        rule.clone(),
        Style::default().fg(rule_color),
    )));
    rows.push(header);
    rows.extend(body);
    rows.push(Line::from(Span::styled(
        rule,
        Style::default().fg(rule_color),
    )));
    frame.render_widget(Paragraph::new(rows), area);
}

fn picker_row(
    row_w: usize,
    is_sel: bool,
    accent_color: Color,
    mut middle: Vec<Span<'static>>,
) -> Line<'static> {
    let bg = if is_sel { accent_color } else { Color::Reset };
    let marker = if is_sel { " › " } else { "   " };
    let mut used = display_width(marker);
    for sp in &middle {
        used += display_width(sp.content.as_ref());
    }
    let pad = row_w.saturating_sub(used);
    let mut spans = Vec::with_capacity(middle.len() + 3);
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        marker,
        Style::default()
            .fg(if is_sel { ON_ACCENT_FG } else { accent_color })
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    ));
    spans.append(&mut middle);
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    Line::from(spans)
}

fn filtered_models<'a>(all: &'a [String], filter: &str) -> Vec<&'a str> {
    let q = filter.trim().to_lowercase();
    all.iter()
        .map(String::as_str)
        .filter(|m| q.is_empty() || m.to_lowercase().contains(&q))
        .collect()
}

fn render_tasks_picker(
    frame: &mut Frame,
    area: Rect,
    subagents: &[SubagentEntry],
    tasks: &[Value],
    sel: usize,
    expanded: bool,
) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let row_w = area.width.saturating_sub(2) as usize;
    let total = subagents.len() + tasks.len();
    let header = Line::from(Span::styled(
        format!(
            "  Subagents ({}) & Tasks ({})  (↑/↓ · v view live · Enter trace · x kill · Esc)",
            subagents.len(),
            tasks.len()
        ),
        Style::default()
            .fg(BASH_MODE_COLOR)
            .add_modifier(Modifier::BOLD),
    ));
    let mut body = Vec::new();
    if total == 0 {
        body.push(Line::from(Span::styled(
            "   (no active subagents or background tasks — press Ctrl+B during a bash run to detach)",
            Style::default().fg(MUTED),
        )));
    } else {
        let reserve_detail = if expanded { 3usize } else { 0usize };
        let max_list = (area.height.saturating_sub(3) as usize).saturating_sub(reserve_detail);
        let (sel, start, end) = selected_window(total, max_list, sel);
        let mut selected_detail: Option<String> = None;

        for idx in start..end {
            let is_sel = idx == sel;
            let bg = if is_sel { BASH_MODE_COLOR } else { Color::Reset };
            let (badge, badge_col, label, detail) = if idx < subagents.len() {
                let s = &subagents[idx];
                let dur = s
                    .elapsed_secs
                    .map(std::time::Duration::from_secs)
                    .unwrap_or_else(|| s.started_at.elapsed());
                let (st, col) = match s.status {
                    SubagentStatus::Running => ("✦ run  ", AMBER),
                    SubagentStatus::Completed => ("✓ done ", SAGE),
                    SubagentStatus::Failed => ("✕ fail ", ROSE),
                };
                let mut full = match &s.error {
                    Some(err) => format!("[{err}] {}", s.task),
                    None => s.task.clone(),
                };
                if !s.trace.is_empty() {
                    full.push_str(&format!(
                        "\ntrace (t{}): {}",
                        s.turns.max(1),
                        s.trace.join(" → ")
                    ));
                }
                let tc = if s.tool_count > 0 {
                    format!(" · {}t", s.tool_count)
                } else {
                    String::new()
                };
                let live_tag = s
                    .activity
                    .as_deref()
                    .filter(|_| s.status == SubagentStatus::Running)
                    .map(|act| format!(" · {act}{tc}"))
                    .unwrap_or(tc);
                (
                    st,
                    col,
                    format!(
                        "[sub:{}] {}{} ({})",
                        s.role,
                        compact_subagent_task(&s.task, 40),
                        live_tag,
                        fmt_elapsed_compact(dur.as_secs())
                    ),
                    full,
                )
            } else {
                let t = &tasks[idx - subagents.len()];
                let id = t["id"].as_str().unwrap_or("bg");
                let status = t["status"].as_str().unwrap_or("running");
                let secs = t["elapsed_secs"].as_u64().unwrap_or(0);
                let cmd = t["command"].as_str().unwrap_or("");
                let (st, col) = match status {
                    "running" => ("⚙ run  ", AMBER),
                    "completed" => ("✓ done ", SAGE),
                    _ => ("✕ stop ", ROSE),
                };
                let out_tail = t["output"].as_str().unwrap_or(cmd);
                (
                    st,
                    col,
                    format!("{id} · {cmd} ({})", fmt_elapsed_compact(secs)),
                    format!("$ {cmd}\n{out_tail}"),
                )
            };
            if is_sel {
                selected_detail = Some(detail);
            }
            let used = 3 + display_width(badge);
            let shown = clip_width(&label, row_w.saturating_sub(used + 2));
            body.push(picker_row(
                row_w,
                is_sel,
                BASH_MODE_COLOR,
                vec![
                    Span::styled(
                        badge,
                        Style::default()
                            .fg(if is_sel { ON_ACCENT_FG } else { badge_col })
                            .bg(bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        shown,
                        Style::default()
                            .fg(if is_sel { ON_ACCENT_FG } else { IVORY })
                            .bg(bg),
                    ),
                ],
            ));
        }

        if expanded {
            if let Some(det) = selected_detail {
                for seg in det
                    .lines()
                    .flat_map(|line| wrap_plain(line, row_w.saturating_sub(6)))
                    .take(3)
                {
                    body.push(Line::from(vec![
                        Span::styled("    ⎿  ", Style::default().fg(MUTED)),
                        Span::styled(seg, Style::default().fg(STONE).bg(SURFACE)),
                    ]));
                }
            }
        }
    }
    render_picker_frame(frame, area, BASH_MODE_COLOR, header, body);
}

fn render_memory_picker(frame: &mut Frame, area: Rect, note: &str, sel: usize) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let row_w = area.width.saturating_sub(2) as usize;
    let choices = [
        ("1. Project memory", ".twig/preferences.md (shared with this repo)"),
        ("2. Global memory ", "~/.config/twig/preferences.md (all repos)"),
    ];
    let header = Line::from(vec![
        Span::styled(
            "  ♯ Save memory: ",
            Style::default()
                .fg(MEMORY_MODE_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            clip_width(note, row_w.saturating_sub(18)),
            Style::default().fg(IVORY),
        ),
    ]);
    let body = choices
        .iter()
        .enumerate()
        .map(|(idx, (k, desc))| {
            let is_sel = idx == sel;
            let bg = if is_sel { MEMORY_MODE_COLOR } else { Color::Reset };
            let shown = clip_width(&format!("{k}  ·  {desc}"), row_w.saturating_sub(4));
            picker_row(
                row_w,
                is_sel,
                MEMORY_MODE_COLOR,
                vec![Span::styled(
                    shown,
                    Style::default()
                        .fg(if is_sel { ON_ACCENT_FG } else { IVORY })
                        .bg(bg)
                        .add_modifier(if is_sel {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                )],
            )
        })
        .collect();
    render_picker_frame(frame, area, MEMORY_MODE_COLOR, header, body);
}

fn render_effort_picker(frame: &mut Frame, area: Rect, active: Option<&str>, sel: usize) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let row_w = area.width.saturating_sub(2) as usize;
    let current = active.unwrap_or("off");
    let header = Line::from(Span::styled(
        "  Reasoning effort  (↑/↓ select · Enter apply · Esc cancel)",
        Style::default()
            .fg(ACCENT_PEAK)
            .add_modifier(Modifier::BOLD),
    ));
    let body = EFFORT_LEVELS
        .iter()
        .enumerate()
        .map(|(idx, (level, desc))| {
            let is_sel = idx == sel;
            let bg = if is_sel { ACCENT } else { Color::Reset };
            let col = format!("{level:<8}");
            let badge = if *level == current { "  (active)" } else { "" };
            let used = 3 + display_width(&col) + display_width(badge);
            let d = clip_width(desc, row_w.saturating_sub(used + 2));
            picker_row(
                row_w,
                is_sel,
                ACCENT,
                vec![
                    Span::styled(
                        col,
                        Style::default()
                            .fg(if is_sel { ON_ACCENT_FG } else { IVORY })
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
                            .fg(if is_sel { ON_ACCENT_FG } else { STONE })
                            .bg(bg),
                    ),
                    Span::styled(
                        badge,
                        Style::default()
                            .fg(if is_sel { ON_ACCENT_FG } else { SAGE })
                            .bg(bg),
                    ),
                ],
            )
        })
        .collect();
    render_picker_frame(frame, area, ACCENT, header, body);
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
    let row_w = area.width.saturating_sub(2) as usize;
    let header_text = if filter.is_empty() {
        format!(
            "  Select model ({} via magpie · type to filter · Enter apply)",
            all.len()
        )
    } else {
        format!("  Select model  filter: {filter}  ({} matches)", items.len())
    };
    let header = Line::from(Span::styled(
        header_text,
        Style::default()
            .fg(ACCENT_PEAK)
            .add_modifier(Modifier::BOLD),
    ));
    let body = if items.is_empty() {
        vec![Line::from(Span::styled(
            "   (no matching models)",
            Style::default().fg(MUTED),
        ))]
    } else {
        let (sel, start, end) =
            selected_window(items.len(), area.height.saturating_sub(3) as usize, sel);
        items[start..end]
            .iter()
            .enumerate()
            .map(|(offset, name)| {
                let is_sel = start + offset == sel;
                let bg = if is_sel { ACCENT } else { Color::Reset };
                let badge = if *name == active_model { "  (active)" } else { "" };
                let used = 3 + display_width(badge);
                let label = clip_width(name, row_w.saturating_sub(used + 2));
                picker_row(
                    row_w,
                    is_sel,
                    ACCENT,
                    vec![
                        Span::styled(
                            label,
                            Style::default()
                                .fg(if is_sel { ON_ACCENT_FG } else { IVORY })
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
                                .fg(if is_sel { ON_ACCENT_FG } else { SAGE })
                                .bg(bg),
                        ),
                    ],
                )
            })
            .collect()
    };
    render_picker_frame(frame, area, ACCENT, header, body);
}

fn render_resume_picker(
    frame: &mut Frame,
    area: Rect,
    sessions: &[SessionInfo],
    all_repos: bool,
    sel: usize,
) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let row_w = area.width.saturating_sub(2) as usize;
    let scope = if all_repos { "all repos" } else { "current repo" };
    let header = Line::from(Span::styled(
        format!("  Resume session [{scope}]  (↑/↓ select · Enter open · a toggle scope · Esc cancel)"),
        Style::default()
            .fg(ACCENT_PEAK)
            .add_modifier(Modifier::BOLD),
    ));
    let body = if sessions.is_empty() {
        vec![Line::from(Span::styled(
            "   (no sessions in scope — press 'a' to toggle all repos)",
            Style::default().fg(MUTED),
        ))]
    } else {
        let (sel, start, end) =
            selected_window(sessions.len(), area.height.saturating_sub(3) as usize, sel);
        sessions[start..end]
            .iter()
            .enumerate()
            .map(|(offset, s)| {
                let active = start + offset == sel;
                let bg = if active { ACCENT } else { Color::Reset };
                let (inbox_tag, inbox_color) = match s.status.as_str() {
                    "approval" => ("▲ wait ", AMBER),
                    "running" => ("● run  ", ACCENT),
                    "dirty" => ("● diff ", ACCENT_PEAK),
                    _ => ("✓ ok   ", SAGE),
                };
                let age_col = format!("{:<8}", s.age);
                let short_sid: String = s.sid.chars().take(8).collect();
                let br = s
                    .branch
                    .as_deref()
                    .filter(|b| !b.is_empty())
                    .map(|b| format!(" · {b}"))
                    .unwrap_or_default();
                let meta = format!("  t{}{br} · {short_sid}", s.turns);
                let used =
                    3 + display_width(inbox_tag) + display_width(&age_col) + display_width(&meta);
                let label = clip_width(&s.label, row_w.saturating_sub(used + 2));
                picker_row(
                    row_w,
                    active,
                    ACCENT,
                    vec![
                        Span::styled(
                            inbox_tag,
                            Style::default()
                                .fg(if active { ON_ACCENT_FG } else { inbox_color })
                                .bg(bg)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            age_col,
                            Style::default()
                                .fg(if active { ON_ACCENT_FG } else { MUTED })
                                .bg(bg),
                        ),
                        Span::styled(
                            label,
                            Style::default()
                                .fg(if active { ON_ACCENT_FG } else { IVORY })
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
                                .fg(if active { ON_ACCENT_FG } else { STONE })
                                .bg(bg),
                        ),
                    ],
                )
            })
            .collect()
    };
    render_picker_frame(frame, area, ACCENT, header, body);
}

fn render_rewind_picker(frame: &mut Frame, area: Rect, turns: &[String], sel: usize) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let row_w = area.width.saturating_sub(2) as usize;
    let header = Line::from(Span::styled(
        "  Rewind to turn  (Enter chat only · r chat + restore files · f fork · Esc cancel)",
        Style::default()
            .fg(ACCENT_PEAK)
            .add_modifier(Modifier::BOLD),
    ));
    let (sel, start, end) =
        selected_window(turns.len(), area.height.saturating_sub(3) as usize, sel);
    let body = turns[start..end]
        .iter()
        .enumerate()
        .map(|(offset, prompt)| {
            let idx = start + offset;
            let is_sel = idx == sel;
            let bg = if is_sel { ACCENT } else { Color::Reset };
            let num = format!("#{:<3}", idx + 1);
            let one_line = prompt.replace('\n', " ");
            let used = 3 + display_width(&num);
            let label = clip_width(&one_line, row_w.saturating_sub(used + 2));
            picker_row(
                row_w,
                is_sel,
                ACCENT,
                vec![
                    Span::styled(
                        num,
                        Style::default()
                            .fg(if is_sel { ON_ACCENT_FG } else { MUTED })
                            .bg(bg),
                    ),
                    Span::styled(
                        label,
                        Style::default()
                            .fg(if is_sel { ON_ACCENT_FG } else { IVORY })
                            .bg(bg)
                            .add_modifier(if is_sel {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ],
            )
        })
        .collect();
    render_picker_frame(frame, area, ACCENT, header, body);
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
    let row_w = area.width.saturating_sub(2) as usize;
    let header_text = if filter.is_empty() {
        format!(
            "  Prompt history ({} saved · type to search · Enter fill)",
            all.len()
        )
    } else {
        format!(
            "  Prompt history  query: {filter}  ({} matches)",
            items.len()
        )
    };
    let header = Line::from(Span::styled(
        header_text,
        Style::default()
            .fg(ACCENT_PEAK)
            .add_modifier(Modifier::BOLD),
    ));
    let body = if items.is_empty() {
        vec![Line::from(Span::styled(
            "   (no matching prompts)",
            Style::default().fg(MUTED),
        ))]
    } else {
        let (sel, start, end) =
            selected_window(items.len(), area.height.saturating_sub(3) as usize, sel);
        items[start..end]
            .iter()
            .enumerate()
            .map(|(offset, entry)| {
                let is_sel = start + offset == sel;
                let bg = if is_sel { ACCENT } else { Color::Reset };
                let one_line = entry.replace('\n', " ↵ ");
                let label = clip_width(&one_line, row_w.saturating_sub(5));
                picker_row(
                    row_w,
                    is_sel,
                    ACCENT,
                    vec![Span::styled(
                        label,
                        Style::default()
                            .fg(if is_sel { ON_ACCENT_FG } else { IVORY })
                            .bg(bg)
                            .add_modifier(if is_sel {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    )],
                )
            })
            .collect()
    };
    render_picker_frame(frame, area, ACCENT, header, body);
}
