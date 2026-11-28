//! Stateless status strips rendered around the composer:
//! - `Plan progress` (`todo`) checklist strip with idle reconciliation & auto-collapse
//! - Live `Subagents` strip with tool counts & turn-scoped visibility
//! - `/btw` ephemeral answer card and queued follow-up strip

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use super::theme::{
    clip_width, display_width, fmt_elapsed_compact, horizontal_pad, wrap_plain, MotionMode, ACCENT,
    ACCENT_PEAK, AMBER, BASH_MODE_COLOR, HAIRLINE, IVORY, MUTED, ROSE, SAGE, STONE, SURFACE,
};
use super::transcript::{NoticeKind, Transcript};

#[derive(Debug, Clone)]
pub struct TodoEntry {
    content: String,
    status: String,
}

impl TodoEntry {
    pub fn is_completed(&self) -> bool {
        self.status == "completed"
    }
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

pub fn todo_strip_height(todos: &[TodoEntry], running: bool, done_at: Option<Instant>) -> u16 {
    if todos.is_empty() {
        return 0;
    }
    let all_done = todos.iter().all(TodoEntry::is_completed);
    if all_done && !running && done_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(4)) {
        return 0;
    }
    if all_done && !running {
        return 1;
    }
    (todos.len().min(4) as u16) + 1
}

/// Compact borderless task progress strip rendered above the composer when `todo` items exist.
pub fn render_todo_strip(frame: &mut Frame, area: Rect, todos: &[TodoEntry], running: bool) {
    if todos.is_empty() || area.height == 0 || area.width < 16 {
        return;
    }
    let width = area.width as usize;
    let pad_x = horizontal_pad(width);
    let pad = " ".repeat(pad_x);
    let done = todos.iter().filter(|t| t.is_completed()).count();
    let total = todos.len();
    let all_done = done == total;

    if all_done && area.height == 1 {
        let row = Line::from(vec![
            Span::raw(pad),
            Span::styled("✓ ", Style::default().fg(SAGE).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("Plan complete  {done}/{total}"),
                Style::default().fg(MUTED),
            ),
        ]);
        frame.render_widget(Paragraph::new(vec![row]), area);
        return;
    }

    let max_items = area.height.saturating_sub(1) as usize;
    let hidden = total.saturating_sub(max_items);
    let mut header_spans = vec![
        Span::raw(pad.clone()),
        Span::styled(
            if all_done { "✓ " } else { "◈ " },
            Style::default()
                .fg(if all_done {
                    SAGE
                } else if running {
                    ACCENT
                } else {
                    STONE
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if all_done {
                format!("Plan complete  {done}/{total}")
            } else {
                format!("Plan progress  {done}/{total}")
            },
            Style::default()
                .fg(if all_done { SAGE } else { STONE })
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !all_done && !running {
        header_spans.push(Span::styled("  · idle", Style::default().fg(MUTED)));
    }
    if hidden > 0 {
        header_spans.push(Span::styled(
            format!("  · +{hidden} more"),
            Style::default().fg(MUTED),
        ));
    }

    let mut rows = vec![Line::from(header_spans)];
    let inner = width.saturating_sub(pad_x * 2 + 4).max(10);
    let focus = todos
        .iter()
        .position(|t| t.status == "in_progress")
        .or_else(|| todos.iter().position(|t| !t.is_completed()))
        .unwrap_or(0);
    let start = focus.saturating_sub(1).min(total.saturating_sub(max_items));
    for item in todos.iter().skip(start).take(max_items) {
        let (glyph, color, text_col) = match item.status.as_str() {
            "completed" => ("✓", SAGE, MUTED),
            "in_progress" if running => ("●", ACCENT, IVORY),
            "in_progress" => ("◐", AMBER, STONE),
            _ => ("○", MUTED, STONE),
        };
        rows.push(Line::from(vec![
            Span::raw(format!("{pad}  ")),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(clip_width(&item.content, inner), Style::default().fg(text_col)),
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
    pub child_sid: Option<String>,
    pub role: String,
    pub model: Option<String>,
    pub task: String,
    pub parallel: bool,
    pub active_turn: bool,
    pub started_at: Instant,
    pub elapsed_secs: Option<u64>,
    pub status: SubagentStatus,
    pub error: Option<String>,
    pub activity: Option<String>,
    pub turns: u32,
    pub tool_count: u32,
    pub trace: Vec<String>,
}

impl SubagentEntry {
    pub fn from_history_json(child: &Value) -> Option<Self> {
        let csid = child["sid"].as_str().filter(|s| !s.is_empty())?;
        Some(Self {
            id: csid.to_owned(),
            child_sid: Some(csid.to_owned()),
            role: child["role"].as_str().unwrap_or("scout").to_owned(),
            model: child["model"].as_str().map(str::to_owned),
            task: child["task"].as_str().unwrap_or("").to_owned(),
            parallel: false,
            active_turn: false,
            started_at: Instant::now(),
            elapsed_secs: Some(0),
            status: SubagentStatus::Completed,
            error: None,
            activity: None,
            turns: child["turns"].as_u64().unwrap_or(0) as u32,
            tool_count: child["tool_count"].as_u64().unwrap_or(0) as u32,
            trace: Vec::new(),
        })
    }

    fn from_started(id: String, ev: &Value) -> Self {
        Self {
            id,
            child_sid: ev["sid"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            role: ev["role"].as_str().unwrap_or("subagent").to_owned(),
            model: ev["model"].as_str().map(str::to_owned),
            task: ev["task"].as_str().unwrap_or("").trim().to_owned(),
            parallel: ev["parallel"] == true,
            active_turn: true,
            started_at: Instant::now(),
            elapsed_secs: None,
            status: SubagentStatus::Running,
            error: None,
            activity: None,
            turns: 0,
            tool_count: 0,
            trace: Vec::new(),
        }
    }

    fn apply_progress(&mut self, ev: &Value) {
        if let Some(csid) = ev["sid"].as_str().filter(|s| !s.is_empty()) {
            self.child_sid = Some(csid.to_owned());
        }
        let turns = ev["turns"].as_u64().unwrap_or(0) as u32;
        if turns > self.turns {
            self.turns = turns;
        }
        let tool_count = ev["tool_count"].as_u64().unwrap_or(0) as u32;
        if tool_count > self.tool_count {
            self.tool_count = tool_count;
        }
        let activity = ev["activity"].as_str().unwrap_or("").trim();
        if !activity.is_empty() {
            self.activity = Some(activity.to_owned());
            if !activity.starts_with("thinking")
                && !activity.starts_with("starting")
                && self.trace.last().map(String::as_str) != Some(activity)
            {
                self.trace.push(activity.to_owned());
                if self.trace.len() > 6 {
                    self.trace.remove(0);
                }
            }
        }
    }
}

fn find_running_subagent(subagents: &[SubagentEntry], id: &str, role: &str) -> Option<usize> {
    (!id.is_empty())
        .then(|| {
            subagents
                .iter()
                .position(|s| s.status == SubagentStatus::Running && s.id == id)
        })
        .flatten()
        .or_else(|| {
            subagents
                .iter()
                .position(|s| s.status == SubagentStatus::Running && s.role == role)
        })
}

fn handle_subagent_completed(
    subagents: &mut Vec<SubagentEntry>,
    transcripts: &mut HashMap<String, Transcript>,
    ev: &Value,
) -> bool {
    let id = ev["id"].as_str().unwrap_or("");
    let role = ev["role"].as_str().unwrap_or("subagent");
    let child_sid = ev["sid"].as_str().filter(|s| !s.is_empty());
    let model = ev["model"].as_str().map(str::to_owned);
    let has_patch = ev["patch_artifact"].as_str().is_some();
    let ok = ev["ok"] == true;
    let secs = ev["elapsed_secs"].as_u64().unwrap_or(0);
    let err_note = ev["error"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let next_status = if ok {
        SubagentStatus::Completed
    } else {
        SubagentStatus::Failed
    };
    let match_idx = find_running_subagent(subagents, id, role).or_else(|| {
        subagents
            .iter()
            .position(|s| s.status == SubagentStatus::Running)
    });
    let (done_id, resolved_sid) = if let Some(idx) = match_idx {
        if let Some(csid) = child_sid {
            subagents[idx].child_sid = Some(csid.to_owned());
        }
        if subagents[idx].model.is_none() {
            subagents[idx].model = model;
        }
        subagents[idx].status = next_status;
        subagents[idx].elapsed_secs = Some(secs);
        subagents[idx].error = err_note.clone();
        subagents[idx].activity = None;
        (subagents[idx].id.clone(), subagents[idx].child_sid.clone())
    } else {
        let fallback_id = if id.is_empty() {
            format!("{role}-{}", subagents.len() + 1)
        } else {
            id.to_owned()
        };
        let mut entry = SubagentEntry::from_started(fallback_id.clone(), ev);
        entry.status = next_status;
        entry.elapsed_secs = Some(secs);
        entry.error = err_note.clone();
        let csid = entry.child_sid.clone();
        subagents.push(entry);
        (fallback_id, csid)
    };
    if let Some(tr) = transcripts.get_mut(&done_id) {
        if ok {
            let apply_hint = match (has_patch, resolved_sid.as_deref()) {
                (true, Some(csid)) => format!(" · patch ready: `/apply {csid}`"),
                _ => String::new(),
            };
            tr.push_notice(
                NoticeKind::Success,
                format!(
                    "subagent [{role}] completed in {secs}s{apply_hint} (Enter reply · Esc ← main)"
                ),
            );
        } else {
            tr.push_notice(
                NoticeKind::Error,
                format!(
                    "subagent [{role}] failed: {}",
                    err_note.as_deref().unwrap_or("error")
                ),
            );
        }
    }
    has_patch
}

/// Apply a `subagent.*` stream/lifecycle event to the subagent thread tree.
/// Returns `(optional_status_header, has_patch, sync_overlay)`.
pub fn apply_subagent_event(
    subagents: &mut Vec<SubagentEntry>,
    transcripts: &mut HashMap<String, Transcript>,
    viewing: Option<&str>,
    ev: &Value,
) -> (Option<String>, bool, bool) {
    match ev["ev"].as_str() {
        Some("subagent.started") => {
            let raw_id = ev["id"].as_str().unwrap_or("");
            let role = ev["role"].as_str().unwrap_or("subagent");
            let id = if raw_id.is_empty() {
                format!("{role}-{}", subagents.len() + 1)
            } else {
                raw_id.to_owned()
            };
            let entry = SubagentEntry::from_started(id.clone(), ev);
            let header = format!("Subagent [{}]", entry.role);
            let mut child_tr = Transcript::new();
            if !entry.task.is_empty() {
                child_tr.push_user(entry.task.clone());
            }
            transcripts.insert(id, child_tr);
            subagents.push(entry);
            if subagents.len() > 16 {
                let drop_n = subagents.len() - 16;
                for dropped in subagents.drain(..drop_n) {
                    if viewing != Some(dropped.id.as_str()) {
                        transcripts.remove(&dropped.id);
                    }
                }
            }
            (Some(header), false, true)
        }
        Some("subagent.progress") => {
            let id = ev["id"].as_str().unwrap_or("");
            let role = ev["role"].as_str().unwrap_or("subagent");
            if let Some(idx) = find_running_subagent(subagents, id, role) {
                subagents[idx].apply_progress(ev);
                (None, false, true)
            } else {
                (None, false, false)
            }
        }
        Some("subagent.stream") => {
            let id = ev["id"].as_str().unwrap_or("");
            let role = ev["role"].as_str().unwrap_or("subagent");
            let key = if !id.is_empty() && transcripts.contains_key(id) {
                Some(id.to_owned())
            } else {
                subagents
                    .iter()
                    .rev()
                    .find(|s| s.status == SubagentStatus::Running && s.role == role)
                    .map(|s| s.id.clone())
            };
            if let Some(k) = key {
                if let Some(tr) = transcripts.get_mut(&k) {
                    let _ = tr.apply_stream_event(&ev["child"]);
                }
            }
            (None, false, false)
        }
        Some("subagent.completed") => {
            let has_patch = handle_subagent_completed(subagents, transcripts, ev);
            (None, has_patch, true)
        }
        _ => (None, false, false),
    }
}

fn visible_in_strip(s: &SubagentEntry) -> bool {
    s.active_turn || s.status == SubagentStatus::Running
}

pub fn subagent_strip_height(subagents: &[SubagentEntry]) -> u16 {
    let visible: Vec<&SubagentEntry> = subagents.iter().filter(|s| visible_in_strip(s)).collect();
    if visible.is_empty() {
        return 0;
    }
    let active_or_failed = visible
        .iter()
        .any(|s| s.status != SubagentStatus::Completed);
    if !active_or_failed {
        return 1;
    }
    (visible.len().min(4) as u16) + 1
}

/// Strip repetitive boilerplate prefixes or tail-preserve module targets so parallel
/// subagents remain distinguishable at a glance without spanning 180 columns.
pub fn compact_subagent_task(raw: &str, max_chars: usize) -> String {
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
    let visible: Vec<&SubagentEntry> = subagents.iter().filter(|s| visible_in_strip(s)).collect();
    if visible.is_empty() || area.height == 0 || area.width < 16 {
        return;
    }
    let width = area.width as usize;
    let pad_x = horizontal_pad(width);
    let pad = " ".repeat(pad_x);
    let running = visible
        .iter()
        .filter(|s| s.status == SubagentStatus::Running)
        .count();
    let done = visible
        .iter()
        .filter(|s| s.status == SubagentStatus::Completed)
        .count();
    let failed = visible
        .iter()
        .filter(|s| s.status == SubagentStatus::Failed)
        .count();

    if running == 0 && failed == 0 {
        let hint = if width >= 68 {
            "  (/agent or ↓ inspect & chat)"
        } else {
            ""
        };
        let summary = Line::from(vec![
            Span::raw(pad),
            Span::styled("✓ ", Style::default().fg(SAGE).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(
                    "{done} subagent{} completed{hint}",
                    if done == 1 { "" } else { "s" }
                ),
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
    let mut ordered: Vec<&SubagentEntry> = visible
        .iter()
        .copied()
        .filter(|s| s.status == SubagentStatus::Running)
        .collect();
    ordered.extend(
        visible
            .iter()
            .copied()
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
            .map(Duration::from_secs)
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
                let compact_act = clip_width(act, task_cap.min(34));
                let tc = if entry.tool_count > 0 {
                    format!(
                        " · {} tool{}",
                        entry.tool_count,
                        if entry.tool_count == 1 { "" } else { "s" }
                    )
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
                Some(err) => clip_width(&format!("[{err}] {clean_task}"), task_cap),
                None => clean_task,
            }
        } else if live_suffix.is_some() && task_budget < 12 {
            String::new()
        } else {
            clip_width(
                &compact_subagent_task(&entry.task, task_budget.max(12)),
                task_budget.max(12),
            )
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
    let pad_x = horizontal_pad(width);
    let inner_w = width.saturating_sub(pad_x * 2 + 4).max(10);
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
    let pad_x = horizontal_pad(width);
    let pad = " ".repeat(pad_x);
    let card_w = width.saturating_sub(pad_x * 2).max(12);
    let q_cap = card_w.saturating_sub(28).max(8);
    let head = format!("✦ /btw · {} ", clip_width(&card.question, q_cap));
    let tail_hint = " Esc/Space dismiss ";
    let fill = card_w.saturating_sub(display_width(&head) + display_width(tail_hint) + 2);
    let mut rows = vec![Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled("╭─ ", Style::default().fg(HAIRLINE)),
        Span::styled(
            head,
            Style::default().fg(ACCENT_PEAK).add_modifier(Modifier::BOLD),
        ),
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
                    Span::styled(
                        format!("… +{} lines", total - max_body),
                        Style::default().fg(MUTED),
                    ),
                ]));
                frame.render_widget(Paragraph::new(rows), area);
                return;
            }
        }
    }
    if rows.len() < area.height as usize {
        rows.push(Line::from(vec![
            Span::raw(pad),
            Span::styled(
                format!("╰{}", "─".repeat(card_w.saturating_sub(1))),
                Style::default().fg(HAIRLINE),
            ),
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
    let pad_x = horizontal_pad(width);
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
            Span::styled(
                "↳ Queued ",
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} ", clip_width(&one_line, cap)),
                Style::default().fg(IVORY).bg(SURFACE),
            ),
            Span::styled(hint, Style::default().fg(MUTED)),
        ]));
    }
    frame.render_widget(Paragraph::new(rows), area);
}
