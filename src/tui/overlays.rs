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

use super::client::SessionSummary;
use super::markdown::{
    clip_str, display_width, fmt_elapsed_compact, wrap_plain, DiffGutterState, MotionMode, ACCENT,
    ACCENT_PEAK, AMBER, BASH_MODE_COLOR, HAIRLINE, IVORY, MEMORY_MODE_COLOR, MUTED, ROSE, SAGE,
    STONE, SURFACE, SURFACE_HI,
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
                    clip_str(path, w.saturating_sub(4)),
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
                    clip_str(path, w.saturating_sub(4)),
                    Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
                ),
            ])];
            for raw in body.lines().take(4) {
                let shown = clip_str(raw, w.saturating_sub(4));
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
        let shown = clip_str(label, inner_w.saturating_sub(display_width(marker)));
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
            clip_str(&title, area.width as usize),
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
    let width = area.width as usize;
    let pad_x = if width >= 120 {
        4
    } else if width >= 80 {
        3
    } else if width >= 56 {
        2
    } else {
        1
    };
    let pad = " ".repeat(pad_x);
    let done = todos.iter().filter(|t| t.status == "completed").count();
    let mut rows = vec![Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(
            "◈ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("Plan progress  {done}/{}", todos.len()),
            Style::default().fg(STONE).add_modifier(Modifier::BOLD),
        ),
    ])];
    let inner = width.saturating_sub(pad_x * 2 + 4).max(10);
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
            Span::raw(format!("{pad}  ")),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub struct SubagentEntry {
    pub id: String,
    pub role: String,
    pub task: String,
    pub parallel: bool,
    pub started_at: std::time::Instant,
    pub elapsed_secs: Option<u64>,
    pub status: SubagentStatus,
    pub error: Option<String>,
    pub activity: Option<String>,
    pub turns: u32,
    pub tool_count: u32,
    pub trace: Vec<String>,
}

pub fn subagent_strip_height(subagents: &[SubagentEntry]) -> u16 {
    if subagents.is_empty() {
        return 0;
    }
    let active_or_failed = subagents
        .iter()
        .any(|s| s.status != SubagentStatus::Completed);
    if !active_or_failed {
        // Collapse to a single clean summary line once all subagents succeed.
        return 1;
    }
    (subagents.len().min(4) as u16) + 1
}

/// Strip repetitive boilerplate prefixes or tail-preserve module targets so parallel
/// subagents remain distinguishable at a glance without spanning 180 columns.
fn compact_subagent_task(raw: &str, max_chars: usize) -> String {
    let mut s = raw.trim();
    for prefix in [
        "Read-only defensive review of ",
        "Read-only defensive review ",
        "Read-only defensive static audit of ",
        "Read-only defensive audit of ",
        "Read-only static audit of ",
        "Read-only review of ",
        "Read-only audit of ",
        "Read-only audit ",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim_start();
            break;
        }
    }
    let limit = max_chars.max(16);
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= limit {
        return s.to_owned();
    }
    // If there is a second sentence (`Audit src/...`), surface that distinctive focus!
    if let Some(dot_pos) = s.find(". ") {
        let tail = s[dot_pos + 2..].trim();
        if !tail.is_empty() && tail.chars().count() <= limit {
            return tail.to_owned();
        }
        if tail.chars().count() > limit {
            let head_part: String = tail.chars().take(limit - 1).collect();
            return format!("{head_part}…");
        }
    }
    let clipped: String = chars.into_iter().take(limit - 1).collect();
    format!("{clipped}…")
}

/// Claude Code style live Subagents list rendered directly below the composer box.
pub fn render_subagent_strip(
    frame: &mut Frame,
    area: Rect,
    subagents: &[SubagentEntry],
    _motion: MotionMode,
) {
    if subagents.is_empty() || area.height == 0 || area.width < 16 {
        return;
    }
    let width = area.width as usize;
    let pad_x = if width >= 120 {
        4
    } else if width >= 80 {
        3
    } else if width >= 56 {
        2
    } else {
        1
    };
    let pad = " ".repeat(pad_x);
    let running = subagents
        .iter()
        .filter(|s| s.status == SubagentStatus::Running)
        .count();
    let done = subagents
        .iter()
        .filter(|s| s.status == SubagentStatus::Completed)
        .count();
    let failed = subagents
        .iter()
        .filter(|s| s.status == SubagentStatus::Failed)
        .count();

    if running == 0 && failed == 0 {
        let summary = Line::from(vec![
            Span::raw(pad),
            Span::styled("✓ ", Style::default().fg(SAGE).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("{done} subagent{} completed", if done == 1 { "" } else { "s" }),
                Style::default().fg(MUTED),
            ),
        ]);
        frame.render_widget(Paragraph::new(vec![summary]), area);
        return;
    }

    let mut stat_parts = Vec::new();
    if running > 0 {
        stat_parts.push(format!("{running} running"));
    }
    if done > 0 {
        stat_parts.push(format!("{done} done"));
    }
    if failed > 0 {
        stat_parts.push(format!("{failed} failed"));
    }

    let mut rows = vec![Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(
            "◈ ",
            Style::default()
                .fg(if running > 0 {
                    ACCENT
                } else if failed > 0 && done == 0 {
                    ROSE
                } else {
                    SAGE
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "Subagents",
            Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  ·  {}{}",
                stat_parts.join(" · "),
                if width >= 72 { "  (↓ / ^T inspect)" } else { "" }
            ),
            Style::default().fg(MUTED),
        ),
    ])];

    let max_items = area.height.saturating_sub(1) as usize;
    let mut ordered: Vec<&SubagentEntry> = subagents
        .iter()
        .filter(|s| s.status == SubagentStatus::Running)
        .collect();
    ordered.extend(
        subagents
            .iter()
            .rev()
            .filter(|s| s.status != SubagentStatus::Running),
    );
    let total_entries = ordered.len();
    let show_n = if total_entries > max_items {
        max_items.saturating_sub(1)
    } else {
        total_entries
    };
    let hidden = total_entries.saturating_sub(show_n);
    let task_cap = width.saturating_sub(pad_x * 2 + 30).clamp(16, 86);

    for entry in ordered.into_iter().take(show_n) {
        let elapsed_dur = entry
            .elapsed_secs
            .map(std::time::Duration::from_secs)
            .unwrap_or_else(|| entry.started_at.elapsed());
        let icon = match entry.status {
            SubagentStatus::Running => {
                let frame_ch = if (elapsed_dur.as_millis() / 500).is_multiple_of(2) {
                    "✦"
                } else {
                    "✧"
                };
                Span::styled(
                    frame_ch,
                    Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                )
            }
            SubagentStatus::Completed => {
                Span::styled("✓", Style::default().fg(SAGE).add_modifier(Modifier::BOLD))
            }
            SubagentStatus::Failed => {
                Span::styled("✕", Style::default().fg(ROSE).add_modifier(Modifier::BOLD))
            }
        };
        let role_tag = if entry.parallel {
            format!("{}*", entry.role)
        } else {
            entry.role.clone()
        };
        let live_suffix = if entry.status == SubagentStatus::Running {
            entry.activity.as_deref().map(|act| {
                let compact_act = clip_str(act, task_cap.min(34));
                let tc = if entry.tool_count > 0 {
                    format!(" · {} tool{}", entry.tool_count, if entry.tool_count == 1 { "" } else { "s" })
                } else {
                    String::new()
                };
                if entry.turns > 0 {
                    format!(" · {compact_act}{tc} · t{}", entry.turns)
                } else {
                    format!(" · {compact_act}{tc}")
                }
            })
        } else if entry.tool_count > 0 {
            Some(format!(
                " · {} tool{}",
                entry.tool_count,
                if entry.tool_count == 1 { "" } else { "s" }
            ))
        } else {
            None
        };
        let live_w = live_suffix.as_deref().map(display_width).unwrap_or(0);
        let task_budget = task_cap.saturating_sub(live_w);
        let summary_text = if entry.status == SubagentStatus::Failed {
            let clean_task = compact_subagent_task(&entry.task, task_cap);
            match &entry.error {
                Some(err) => clip_str(&format!("[{err}] {clean_task}"), task_cap),
                None => clean_task,
            }
        } else if live_suffix.is_some() && task_budget < 12 {
            String::new()
        } else {
            clip_str(&compact_subagent_task(&entry.task, task_budget.max(12)), task_budget.max(12))
        };
        let mut spans = vec![
            Span::raw(format!("{pad}  ")),
            icon,
            Span::raw("  "),
            Span::styled(
                format!("{role_tag:<11} "),
                Style::default()
                    .fg(if entry.status == SubagentStatus::Running {
                        ACCENT_PEAK
                    } else {
                        STONE
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if !summary_text.is_empty() {
            spans.push(Span::styled(
                summary_text,
                Style::default().fg(if entry.status == SubagentStatus::Running {
                    IVORY
                } else if entry.status == SubagentStatus::Failed {
                    ROSE
                } else {
                    MUTED
                }),
            ));
        }
        if let Some(live) = live_suffix {
            let shown_live = if spans.len() == 4 {
                live.trim_start_matches(" · ").to_owned()
            } else {
                live
            };
            spans.push(Span::styled(shown_live, Style::default().fg(BASH_MODE_COLOR)));
        }
        spans.push(Span::styled(
            format!("  {}", fmt_elapsed_compact(elapsed_dur.as_secs())),
            Style::default().fg(MUTED),
        ));
        rows.push(Line::from(spans));
    }
    if hidden > 0 {
        rows.push(Line::from(vec![
            Span::raw(format!("{pad}  ")),
            Span::styled(
                format!("… +{hidden} more subagents"),
                Style::default().fg(MUTED),
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

/// Ephemeral side-question card (`/btw`) rendered above the composer without polluting chat history.
#[derive(Debug, Clone)]
pub struct BtwCard {
    pub question: String,
    pub answer: Option<String>,
}

pub fn btw_card_height(card: Option<&BtwCard>, width: usize) -> u16 {
    let Some(c) = card else {
        return 0;
    };
    let inner_w = width.saturating_sub(10).max(16);
    let body_rows = match &c.answer {
        None => 1,
        Some(ans) => wrap_plain(ans.trim(), inner_w).len().clamp(1, 6),
    };
    (body_rows as u16) + 2
}

pub fn render_btw_card(frame: &mut Frame, area: Rect, card: &BtwCard) {
    if area.height < 2 || area.width < 18 {
        return;
    }
    let width = area.width as usize;
    let pad_x = if width >= 120 { 4 } else if width >= 80 { 3 } else if width >= 56 { 2 } else { 1 };
    let pad = " ".repeat(pad_x);
    let card_w = width.saturating_sub(pad_x * 2).max(12);
    let q_cap = card_w.saturating_sub(28).max(8);
    let head = format!("✦ /btw · {} ", clip_str(&card.question, q_cap));
    let tail_hint = " Esc/Space dismiss ";
    let fill = card_w.saturating_sub(display_width(&head) + display_width(tail_hint) + 2);
    let mut rows = vec![Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled("╭─ ", Style::default().fg(HAIRLINE)),
        Span::styled(head, Style::default().fg(ACCENT_PEAK).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(fill), Style::default().fg(HAIRLINE)),
        Span::styled(tail_hint, Style::default().fg(MUTED)),
    ])];
    let max_body = (area.height as usize).saturating_sub(2).max(1);
    let inner_w = card_w.saturating_sub(4).max(10);
    match &card.answer {
        None => {
            rows.push(Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled("│ ", Style::default().fg(HAIRLINE)),
                Span::styled(
                    "Thinking… (ephemeral, zero context pollution)",
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
        Some(ans) => {
            let wrapped = wrap_plain(ans.trim(), inner_w);
            let total = wrapped.len();
            for seg in wrapped.into_iter().take(max_body) {
                rows.push(Line::from(vec![
                    Span::raw(pad.clone()),
                    Span::styled("│ ", Style::default().fg(HAIRLINE)),
                    Span::styled(seg, Style::default().fg(IVORY)),
                ]));
            }
            if total > max_body && rows.len() < area.height as usize {
                rows.push(Line::from(vec![
                    Span::raw(pad.clone()),
                    Span::styled("╰─ ", Style::default().fg(HAIRLINE)),
                    Span::styled(format!("… +{} lines", total - max_body), Style::default().fg(MUTED)),
                ]));
                frame.render_widget(Paragraph::new(rows), area);
                return;
            }
        }
    }
    if rows.len() < area.height as usize {
        rows.push(Line::from(vec![
            Span::raw(pad),
            Span::styled(format!("╰{}", "─".repeat(card_w.saturating_sub(1))), Style::default().fg(HAIRLINE)),
        ]));
    }
    frame.render_widget(Paragraph::new(rows), area);
}

/// Queued follow-up pill strip rendered right above the composer (`↑` pops into composer to edit).
pub fn queued_strip_height(queued: &VecDeque<String>) -> u16 {
    queued.len().min(2) as u16
}

pub fn render_queued_strip(frame: &mut Frame, area: Rect, queued: &VecDeque<String>) {
    if queued.is_empty() || area.height == 0 || area.width < 16 {
        return;
    }
    let width = area.width as usize;
    let pad_x = if width >= 120 { 4 } else if width >= 80 { 3 } else if width >= 56 { 2 } else { 1 };
    let pad = " ".repeat(pad_x);
    let max_rows = area.height as usize;
    let total = queued.len();
    let mut rows = Vec::with_capacity(max_rows);
    for (idx, item) in queued.iter().take(max_rows).enumerate() {
        let hint = if idx == 0 && width >= 64 {
            if total > max_rows {
                format!("  (+{} more · ↑ edit · /unqueue)", total - max_rows)
            } else {
                "  (↑ edit · /unqueue)".to_owned()
            }
        } else {
            String::new()
        };
        let cap = width.saturating_sub(pad_x * 2 + 14 + display_width(&hint)).max(10);
        let one_line = item.replace('\n', " ↵ ");
        rows.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled("↳ Queued ", Style::default().fg(AMBER).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {} ", clip_str(&one_line, cap)),
                Style::default().fg(IVORY).bg(SURFACE),
            ),
            Span::styled(hint, Style::default().fg(MUTED)),
        ]));
    }
    frame.render_widget(Paragraph::new(rows), area);
}

/// Borderless Codex-style interactive overlay picker shared by `/resume`, `/model`, `/effort`, `/rewind`, `Ctrl+R`, `Ctrl+T`, and `#` memory.
pub enum OverlayPicker {
    Resume {
        sessions: Vec<SessionSummary>,
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
    pub fn resume(sessions: Vec<SessionSummary>, all_repos: bool) -> Self {
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
    let rule_len = area.width.saturating_sub(2) as usize;
    let rule = format!(" {}", "─".repeat(rule_len));
    let row_w = area.width.saturating_sub(2) as usize;
    let sel_fg = Color::Rgb(22, 19, 17);
    let total = subagents.len() + tasks.len();
    let sel = sel.min(total.saturating_sub(1));

    let mut rows = vec![
        Line::from(Span::styled(rule.clone(), Style::default().fg(BASH_MODE_COLOR))),
        Line::from(Span::styled(
            format!(
                "  Subagents ({}) & Tasks ({})  (↑/↓ · v view live · Enter trace · x kill · Esc)",
                subagents.len(),
                tasks.len()
            ),
            Style::default().fg(BASH_MODE_COLOR).add_modifier(Modifier::BOLD),
        )),
    ];

    if total == 0 {
        rows.push(Line::from(Span::styled(
            "   (no active subagents or background tasks — press Ctrl+B during a bash run to detach)",
            Style::default().fg(MUTED),
        )));
    } else {
        let reserve_detail = if expanded { 3usize } else { 0usize };
        let max_list = (area.height.saturating_sub(3) as usize)
            .saturating_sub(reserve_detail)
            .max(1);
        let start = sel.saturating_sub(max_list.saturating_sub(1));
        let end = (start + max_list).min(total);
        let mut selected_detail: Option<String> = None;

        for idx in start..end {
            let is_sel = idx == sel;
            let bg = if is_sel { BASH_MODE_COLOR } else { Color::Reset };
            let marker = if is_sel { " › " } else { "   " };
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
            let used = display_width(marker) + display_width(badge);
            let shown = clip_str(&label, row_w.saturating_sub(used + 2));
            let pad = row_w.saturating_sub(used + display_width(&shown));
            rows.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    marker,
                    Style::default()
                        .fg(if is_sel { sel_fg } else { BASH_MODE_COLOR })
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    badge,
                    Style::default()
                        .fg(if is_sel { sel_fg } else { badge_col })
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    shown,
                    Style::default()
                        .fg(if is_sel { sel_fg } else { IVORY })
                        .bg(bg),
                ),
                Span::styled(" ".repeat(pad), Style::default().bg(bg)),
            ]));
        }

        if expanded {
            if let Some(det) = selected_detail {
                for seg in det
                    .lines()
                    .flat_map(|line| wrap_plain(line, row_w.saturating_sub(6)))
                    .take(3)
                {
                    rows.push(Line::from(vec![
                        Span::styled("    ⎿  ", Style::default().fg(MUTED)),
                        Span::styled(seg, Style::default().fg(STONE).bg(SURFACE)),
                    ]));
                }
            }
        }
    }
    rows.push(Line::from(Span::styled(
        rule,
        Style::default().fg(BASH_MODE_COLOR),
    )));
    frame.render_widget(Paragraph::new(rows), area);
}

fn render_memory_picker(frame: &mut Frame, area: Rect, note: &str, sel: usize) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let rule_len = area.width.saturating_sub(2) as usize;
    let rule = format!(" {}", "─".repeat(rule_len));
    let row_w = area.width.saturating_sub(2) as usize;
    let sel_fg = Color::Rgb(22, 19, 17);
    let choices = [
        ("1. Project memory", ".twig/preferences.md (shared with this repo)"),
        ("2. Global memory ", "~/.config/twig/preferences.md (all repos)"),
    ];
    let mut rows = vec![
        Line::from(Span::styled(
            rule.clone(),
            Style::default().fg(MEMORY_MODE_COLOR),
        )),
        Line::from(vec![
            Span::styled(
                "  ♯ Save memory: ",
                Style::default()
                    .fg(MEMORY_MODE_COLOR)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                clip_str(note, row_w.saturating_sub(18)),
                Style::default().fg(IVORY),
            ),
        ]),
    ];
    for (idx, (k, desc)) in choices.iter().enumerate() {
        let is_sel = idx == sel;
        let bg = if is_sel { MEMORY_MODE_COLOR } else { Color::Reset };
        let marker = if is_sel { " › " } else { "   " };
        let text = format!("{k}  ·  {desc}");
        let shown = clip_str(&text, row_w.saturating_sub(display_width(marker) + 1));
        let pad = row_w.saturating_sub(display_width(marker) + display_width(&shown));
        rows.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                marker,
                Style::default()
                    .fg(if is_sel { sel_fg } else { MEMORY_MODE_COLOR })
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                shown,
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
    rows.push(Line::from(Span::styled(
        rule,
        Style::default().fg(MEMORY_MODE_COLOR),
    )));
    frame.render_widget(Paragraph::new(rows), area);
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


fn render_resume_picker(
    frame: &mut Frame,
    area: Rect,
    sessions: &[SessionSummary],
    all_repos: bool,
    sel: usize,
) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    let rule_len = area.width.saturating_sub(2) as usize;
    let rule = format!(" {}", "─".repeat(rule_len));
    let max_rows = (area.height.saturating_sub(3) as usize).max(1);
    let sel = sel.min(sessions.len().saturating_sub(1));
    let start = sel.saturating_sub(max_rows.saturating_sub(1));
    let end = (start + max_rows).min(sessions.len());
    let row_w = area.width.saturating_sub(2) as usize;
    let sel_fg = Color::Rgb(22, 19, 17);
    let scope = if all_repos { "all repos" } else { "current repo" };

    let mut rows = vec![
        Line::from(Span::styled(rule.clone(), Style::default().fg(ACCENT))),
        Line::from(Span::styled(
            format!("  Resume session [{scope}]  (↑/↓ select · Enter open · a toggle scope · Esc cancel)"),
            Style::default()
                .fg(ACCENT_PEAK)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    if sessions.is_empty() {
        rows.push(Line::from(Span::styled(
            "   (no sessions in scope — press 'a' to toggle all repos)",
            Style::default().fg(MUTED),
        )));
    } else {
        for (offset, s) in sessions[start..end].iter().enumerate() {
            let active = start + offset == sel;
            let bg = if active { ACCENT } else { Color::Reset };
            let marker = if active { " › " } else { "   " };
            let (inbox_tag, inbox_color) = match s.status.as_str() {
                "approval" => ("▲ wait ", AMBER),
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
            let used = display_width(marker)
                + display_width(inbox_tag)
                + display_width(&age_col)
                + display_width(&meta);
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
                    inbox_tag,
                    Style::default()
                        .fg(if active { sel_fg } else { inbox_color })
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
            "  Rewind to turn  (Enter chat only · r chat + restore files · f fork · Esc cancel)",
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
        "outline" => args["path"].as_str().unwrap_or("."),
        "apply_patch" => "patch",
        "grep" | "glob" => args["pattern"].as_str().unwrap_or(""),
        "skill" => args["name"].as_str().unwrap_or("list"),
        "subagent" => args["task"].as_str().unwrap_or(""),
        "papercut" => args["component"].as_str().unwrap_or(""),
        _ => "",
    };
    clip_str(&raw.replace('\n', " "), 92)
}
