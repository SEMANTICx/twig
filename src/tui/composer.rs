//! Borderless Claude-Code/Codex multiline composer with:
//! - Inset top & bottom hairline rules (`─`) and warm terracotta `❯` glyph
//! - CJK/emoji-safe cursor math and readline shortcuts
//! - Codex-style large-paste folding pills (`[Pasted #1 · 12 lines]`)
//! - Codex-style borderless scrollable `/` command & `@` file mention palette
//!   with full-width accent selection bar

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use super::markdown::{
    display_width, wrap_plain, ACCENT, ACCENT_PEAK, BASH_MODE_COLOR, IVORY, MEMORY_MODE_COLOR,
    MUTED, STONE, SURFACE_HI,
};

/// Codex & Claude Code parity slash command catalog.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/plan", "toggle read-only planning mode (Shift+Tab)"),
    ("/study", "toggle REA epistemic clean-room study mode (/study [on|off|<target>])"),
    ("/go", "exit /plan & /study, switch to `code` role, and execute agreed spec/plan"),
    ("/operator", "toggle RCI split-fact collective orchestration mode"),
    ("/blackboard", "view/add/clear RCI split-fact blackboard (/blackboard [clear|add <fact>])"),
    ("/rci", "RCI equal-budget role-card status, harvest & distillation (/rci [status|harvest|distill])"),
    ("/roles", "view or set per-role models (/roles <role> <model> [effort])"),
    ("/dual-plan ", "independent blind dual-model plan + synthesis"),
    ("/btw ", "ask an ephemeral side question without polluting context"),
    ("/walkthrough", "generate a human-readable walkthrough of git diff"),
    ("/commit", "stage/inspect changes and commit with style-matched message"),
    ("/pr", "commit, push branch, and open a clean Pull Request (/pr [--draft] [hint])"),
    ("/worktree ", "live mid-session handoff to an isolated Git worktree (/worktree <name|back|list>)"),
    ("/queue ", "queue follow-up prompt to run right after active turn (or Alt+Enter)"),
    ("/apply ", "apply a subagent session patch into worktree (/undo reverts)"),
    ("/gc", "clean up 0-byte session remnants and clean subagent worktrees"),
    ("/effort", "set reasoning effort (off/low/medium/high/xhigh)"),
    ("/model", "browse and switch active model via magpie"),
    ("/new", "start a new chat session"),
    ("/resume", "browse and resume a previous session"),
    ("/fork", "fork current session into a new branch"),
    ("/rewind", "pick a previous turn to rewind & edit (or Esc×2)"),
    ("/rename ", "rename the current session"),
    ("/archive", "archive current session and start a new one"),
    ("/undo", "revert the last edit or write tool change"),
    ("/diff", "show current git diff with line numbers"),
    ("/review", "run independent read-only review on current changes"),
    ("/image ", "attach image file or paste clipboard image (Alt+V)"),
    ("/tasks", "list asynchronous background shell tasks"),
    ("/mcp", "list or toggle MCP servers (/mcp [enable|disable <name> | init-rea])"),
    ("/evolve", "propose human-reviewed .twig/preferences or skill upgrade"),
    ("/skills", "list/manage skills (/skills enable|disable|doctor, or $name)"),
    ("/memory", "edit .twig/preferences.md project memory in $EDITOR"),
    ("/init", "initialize .twig/preferences.md and .twig/project.json"),
    ("/doctor", "run twig environment & dependency diagnostics"),
    ("/status", "show session telemetry, usage, and context"),
    ("/verify", "verify session hash chain, guard, and policy stats"),
    ("/compact", "compact conversation context now"),
    ("/export ", "export conversation transcript to Markdown"),
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

#[derive(Debug, PartialEq, Eq)]
pub enum SlashCommand {
    Quit,
    SetPlan(bool),
    SetOperator(Option<bool>),
    SetStudy {
        enabled: Option<bool>,
        target: Option<String>,
    },
    ShowRoles,
    ResetRoles,
    SetRole {
        role: String,
        model: Option<String>,
        effort: Option<String>,
    },
    DualPlan(String),
    ExecutePlan(Option<String>),
    SideQuestion(String),
    RunWalkthrough,
    CommitChanges(Option<String>),
    CreatePr(Option<String>),
    ManageWorktree(Option<String>),
    QueueFollowUp(Option<String>),
    ApplySession(String),
    GcSessions,
    OpenEffortPicker,
    SetEffort(String),
    OpenModelPicker,
    SetModel(String),
    NewSession,
    OpenResume,
    SwitchSession(String),
    ForkSession,
    OpenRewindPicker,
    RewindSession {
        turns: usize,
        restore_code: bool,
    },
    RenameSession(String),
    ArchiveSession,
    UndoEdit,
    RunDiff,
    RunReview,
    AttachImage(Option<String>),
    ClearImages,
    ListTasks,
    CancelTask(String),
    ManageMcp {
        action: String,
        server: Option<String>,
    },
    Blackboard {
        action: String,
        fact: Option<String>,
    },
    RunRci(String),
    EvolveSession,
    ManageSkills {
        action: String,
        name: Option<String>,
        project: bool,
    },
    EditMemory,
    ShowMemory,
    InitProject,
    RunDoctor,
    ShowStatus,
    VerifySession,
    CompactNow,
    ExportTranscript(Option<String>),
    ShowConfig,
    Steer(String),
    Cancel,
    ResolveApproval { id: String, allow: bool },
    ToggleExpand,
    OpenFind(String),
    CopyCode,
    OpenEditor,
    ToggleMouse,
    ClearTranscript,
    ShowHelp,
}

/// Single source of truth for parsing `/` commands matching `SLASH_COMMANDS`.
pub fn parse_slash(trimmed: &str, plan_mode: bool) -> Option<SlashCommand> {
    let exact = match trimmed {
        "/quit" | "/exit" => Some(SlashCommand::Quit),
        "/plan" => Some(SlashCommand::SetPlan(!plan_mode)),
        "/plan on" => Some(SlashCommand::SetPlan(true)),
        "/plan off" => Some(SlashCommand::SetPlan(false)),
        "/operator" => Some(SlashCommand::SetOperator(None)),
        "/operator on" => Some(SlashCommand::SetOperator(Some(true))),
        "/operator off" => Some(SlashCommand::SetOperator(Some(false))),
        "/study" => Some(SlashCommand::SetStudy {
            enabled: None,
            target: None,
        }),
        "/study on" => Some(SlashCommand::SetStudy {
            enabled: Some(true),
            target: None,
        }),
        "/study off" => Some(SlashCommand::SetStudy {
            enabled: Some(false),
            target: None,
        }),
        "/roles" => Some(SlashCommand::ShowRoles),
        "/roles reset" => Some(SlashCommand::ResetRoles),
        "/walkthrough" => Some(SlashCommand::RunWalkthrough),
        "/go" | "/execute" => Some(SlashCommand::ExecutePlan(None)),
        "/commit" => Some(SlashCommand::CommitChanges(None)),
        "/pr" => Some(SlashCommand::CreatePr(None)),
        "/worktree" | "/wt" => Some(SlashCommand::ManageWorktree(None)),
        "/queue" => Some(SlashCommand::QueueFollowUp(None)),
        "/unqueue" | "/queue clear" => Some(SlashCommand::QueueFollowUp(Some("clear".into()))),
        "/gc" => Some(SlashCommand::GcSessions),
        "/effort" => Some(SlashCommand::OpenEffortPicker),
        "/model" => Some(SlashCommand::OpenModelPicker),
        "/new" => Some(SlashCommand::NewSession),
        "/resume" => Some(SlashCommand::OpenResume),
        "/fork" => Some(SlashCommand::ForkSession),
        "/rewind" => Some(SlashCommand::OpenRewindPicker),
        "/archive" => Some(SlashCommand::ArchiveSession),
        "/undo" => Some(SlashCommand::UndoEdit),
        "/diff" => Some(SlashCommand::RunDiff),
        "/review" => Some(SlashCommand::RunReview),
        "/image" => Some(SlashCommand::AttachImage(None)),
        "/image clear" => Some(SlashCommand::ClearImages),
        "/tasks" => Some(SlashCommand::ListTasks),
        "/mcp" => Some(SlashCommand::ManageMcp {
            action: "list".into(),
            server: None,
        }),
        "/mcp init-rea" | "/init --rea" => Some(SlashCommand::ManageMcp {
            action: "init_rea".into(),
            server: None,
        }),
        "/blackboard" | "/bb" => Some(SlashCommand::Blackboard {
            action: "list".into(),
            fact: None,
        }),
        "/blackboard clear" | "/bb clear" => Some(SlashCommand::Blackboard {
            action: "clear".into(),
            fact: None,
        }),
        "/rci" | "/rci status" => Some(SlashCommand::RunRci("status".into())),
        "/rci harvest" => Some(SlashCommand::RunRci("harvest".into())),
        "/rci distill" | "/evolve --rci" => Some(SlashCommand::RunRci("distill".into())),
        "/evolve" => Some(SlashCommand::EvolveSession),
        "/skills" | "/playbooks" => Some(SlashCommand::ManageSkills {
            action: "list".into(),
            name: None,
            project: false,
        }),
        "/skills doctor" => Some(SlashCommand::ManageSkills {
            action: "doctor".into(),
            name: None,
            project: false,
        }),
        "/memory" => Some(SlashCommand::EditMemory),
        "/memory show" => Some(SlashCommand::ShowMemory),
        "/init" => Some(SlashCommand::InitProject),
        "/doctor" => Some(SlashCommand::RunDoctor),
        "/status" => Some(SlashCommand::ShowStatus),
        "/verify" => Some(SlashCommand::VerifySession),
        "/compact" => Some(SlashCommand::CompactNow),
        "/export" => Some(SlashCommand::ExportTranscript(None)),
        "/config" => Some(SlashCommand::ShowConfig),
        "/cancel" => Some(SlashCommand::Cancel),
        "/expand" => Some(SlashCommand::ToggleExpand),
        "/find" => Some(SlashCommand::OpenFind(String::new())),
        "/copy" => Some(SlashCommand::CopyCode),
        "/editor" => Some(SlashCommand::OpenEditor),
        "/mouse" => Some(SlashCommand::ToggleMouse),
        "/clear" => Some(SlashCommand::ClearTranscript),
        "/help" => Some(SlashCommand::ShowHelp),
        _ => None,
    };
    if exact.is_some() {
        return exact;
    }
    if let Some(arg) = trimmed.strip_prefix("/study ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::SetStudy {
                enabled: Some(true),
                target: Some(a.to_owned()),
            });
        }
    }
    if let Some(arg) = trimmed
        .strip_prefix("/blackboard ")
        .or_else(|| trimmed.strip_prefix("/bb "))
    {
        let a = arg.trim();
        if let Some(rest) = a.strip_prefix("add ") {
            let f = rest.trim();
            if !f.is_empty() {
                return Some(SlashCommand::Blackboard {
                    action: "add".into(),
                    fact: Some(f.to_owned()),
                });
            }
        } else if !a.is_empty() {
            return Some(SlashCommand::Blackboard {
                action: "add".into(),
                fact: Some(a.to_owned()),
            });
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/rci ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::RunRci(a.to_owned()));
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/mcp ") {
        let parts: Vec<&str> = arg.split_whitespace().collect();
        match parts.as_slice() {
            ["init-rea" | "init_rea"] => {
                return Some(SlashCommand::ManageMcp {
                    action: "init_rea".into(),
                    server: None,
                });
            }
            ["enable", srv, ..] => {
                return Some(SlashCommand::ManageMcp {
                    action: "enable".into(),
                    server: Some((*srv).to_owned()),
                });
            }
            ["disable", srv, ..] => {
                return Some(SlashCommand::ManageMcp {
                    action: "disable".into(),
                    server: Some((*srv).to_owned()),
                });
            }
            _ => {
                return Some(SlashCommand::ManageMcp {
                    action: "list".into(),
                    server: None,
                });
            }
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/effort ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::SetEffort(a.to_owned()));
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/roles ") {
        let parts: Vec<&str> = arg.split_whitespace().collect();
        match parts.as_slice() {
            ["reset"] => return Some(SlashCommand::ResetRoles),
            [role, "reset" | "default"] => {
                return Some(SlashCommand::SetRole {
                    role: (*role).to_owned(),
                    model: None,
                    effort: None,
                });
            }
            [role, model] => {
                return Some(SlashCommand::SetRole {
                    role: (*role).to_owned(),
                    model: Some((*model).to_owned()),
                    effort: None,
                });
            }
            [role, model, effort, ..] => {
                return Some(SlashCommand::SetRole {
                    role: (*role).to_owned(),
                    model: Some((*model).to_owned()),
                    effort: Some((*effort).to_owned()),
                });
            }
            _ => return Some(SlashCommand::ShowRoles),
        }
    }
    if let Some(arg) = trimmed
        .strip_prefix("/dual-plan ")
        .or_else(|| trimmed.strip_prefix("/council "))
    {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::DualPlan(a.to_owned()));
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/btw ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::SideQuestion(a.to_owned()));
        }
    }
    if let Some(arg) = trimmed
        .strip_prefix("/go ")
        .or_else(|| trimmed.strip_prefix("/execute "))
    {
        let a = arg.trim();
        return Some(SlashCommand::ExecutePlan((!a.is_empty()).then(|| a.to_owned())));
    }
    if let Some(arg) = trimmed.strip_prefix("/commit ") {
        let a = arg.trim();
        return Some(SlashCommand::CommitChanges((!a.is_empty()).then(|| a.to_owned())));
    }
    if let Some(arg) = trimmed.strip_prefix("/pr ") {
        let a = arg.trim();
        return Some(SlashCommand::CreatePr((!a.is_empty()).then(|| a.to_owned())));
    }
    if let Some(arg) = trimmed
        .strip_prefix("/worktree ")
        .or_else(|| trimmed.strip_prefix("/wt "))
    {
        let a = arg.trim();
        return Some(SlashCommand::ManageWorktree((!a.is_empty()).then(|| a.to_owned())));
    }
    if let Some(arg) = trimmed.strip_prefix("/queue ") {
        let a = arg.trim();
        return Some(SlashCommand::QueueFollowUp((!a.is_empty()).then(|| a.to_owned())));
    }
    if let Some(arg) = trimmed.strip_prefix("/apply ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::ApplySession(a.to_owned()));
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/model ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::SetModel(a.to_owned()));
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/resume ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::SwitchSession(a.to_owned()));
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/rewind ") {
        let restore_code = arg.split_whitespace().any(|t| t == "--code" || t == "-c");
        let n = arg
            .split_whitespace()
            .find_map(|t| t.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        return Some(SlashCommand::RewindSession {
            turns: n,
            restore_code,
        });
    }
    if let Some(arg) = trimmed.strip_prefix("/rename ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::RenameSession(a.to_owned()));
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/image ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::AttachImage(Some(a.to_owned())));
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/export ") {
        let a = arg.trim();
        return Some(SlashCommand::ExportTranscript(
            (!a.is_empty()).then(|| a.to_owned()),
        ));
    }
    if let Some(arg) = trimmed.strip_prefix("/find ") {
        return Some(SlashCommand::OpenFind(arg.trim().to_owned()));
    }
    if let Some(arg) = trimmed.strip_prefix("/steer ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::Steer(a.to_owned()));
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/approve ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::ResolveApproval {
                id: a.to_owned(),
                allow: true,
            });
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/skills ") {
        let parts: Vec<&str> = arg.split_whitespace().collect();
        let project = parts.contains(&"--project");
        let pos: Vec<&str> = parts
            .into_iter()
            .filter(|p| *p != "--project")
            .collect();
        match pos.as_slice() {
            ["list"] => {
                return Some(SlashCommand::ManageSkills {
                    action: "list".into(),
                    name: None,
                    project,
                });
            }
            ["doctor"] => {
                return Some(SlashCommand::ManageSkills {
                    action: "doctor".into(),
                    name: None,
                    project,
                });
            }
            [act @ ("enable" | "disable"), name] => {
                return Some(SlashCommand::ManageSkills {
                    action: (*act).to_owned(),
                    name: Some((*name).to_owned()),
                    project,
                });
            }
            _ => {}
        }
    }
    if let Some(arg) = trimmed.strip_prefix("/deny ") {
        let a = arg.trim();
        if !a.is_empty() {
            return Some(SlashCommand::ResolveApproval {
                id: a.to_owned(),
                allow: false,
            });
        }
    }
    None
}

pub enum PopupKind {
    Slash(Vec<(String, String)>),
    Mention {
        at_byte: usize,
        items: Vec<String>,
    },
    Skill {
        dollar_byte: usize,
        items: Vec<(String, String, String)>,
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
    skill_catalog: Vec<(String, String, String)>,
    popup_sel: usize,
}

impl Composer {
    pub fn new(repo: &str) -> Self {
        let skill_catalog =
            crate::runtime::guidance::SkillStore::default_for(Some(Path::new(repo)))
                .catalog_for_autocomplete();
        Self {
            text: String::new(),
            cursor: 0,
            history: super::client::load_input_history(),
            history_idx: None,
            draft: String::new(),
            pastes: HashMap::new(),
            next_paste_id: 1,
            repo_files: list_repo_files(repo),
            skill_catalog,
            popup_sel: 0,
        }
    }

    pub fn refresh_skills(&mut self, repo: &str) {
        self.skill_catalog =
            crate::runtime::guidance::SkillStore::default_for(Some(Path::new(repo)))
                .catalog_for_autocomplete();
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    #[cfg(test)]
    pub fn raw_text(&self) -> &str {
        &self.text
    }

    pub fn is_browsing_history(&self) -> bool {
        self.history_idx.is_some()
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

    /// Handle readline shortcuts, popup navigation, editing, and submission.
    /// Returns `Some(text)` when the user submits a prompt or executes a slash command.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('a') => self.move_home(),
                KeyCode::Char('e') => self.move_end(),
                KeyCode::Char('u') => self.clear_to_start(),
                KeyCode::Char('w') => self.delete_word_back(),
                KeyCode::Char('j') => self.insert_char('\n'),
                _ => {}
            }
            return None;
        }
        if self.has_popup() {
            match key.code {
                KeyCode::Up => {
                    self.popup_move(-1);
                    return None;
                }
                KeyCode::Down => {
                    self.popup_move(1);
                    return None;
                }
                KeyCode::Tab => {
                    self.accept_popup();
                    return None;
                }
                KeyCode::Enter if key.modifiers.is_empty() => match self.accept_popup_on_enter() {
                    PopupAccept::Execute(cmd) => return Some(cmd),
                    PopupAccept::Filled => return None,
                    PopupAccept::None => {}
                },
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.insert_char('\n');
                None
            }
            KeyCode::Enter => Some(self.take_submitted()),
            KeyCode::Tab => {
                self.accept_popup();
                None
            }
            KeyCode::Backspace => {
                self.backspace();
                None
            }
            KeyCode::Delete => {
                self.delete();
                None
            }
            KeyCode::Left => {
                self.move_left();
                None
            }
            KeyCode::Right => {
                self.move_right();
                None
            }
            KeyCode::Home => {
                self.move_home();
                None
            }
            KeyCode::End => {
                self.move_end();
                None
            }
            KeyCode::Up => {
                self.history_up();
                None
            }
            KeyCode::Down => {
                self.history_down();
                None
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                None
            }
            _ => None,
        }
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
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
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
            PopupKind::Skill { items, .. } => items.len(),
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
            PopupKind::Skill { dollar_byte, items } => {
                let idx = self.popup_sel.min(items.len().saturating_sub(1));
                if let Some((name, _, _)) = items.get(idx) {
                    let replacement = format!("${name} ");
                    self.text.replace_range(dollar_byte..self.cursor, &replacement);
                    self.cursor = dollar_byte + replacement.len();
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
            if !cfg!(test) {
                super::client::append_input_history(&expanded);
            }
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
            let bare = q.strip_prefix('/').unwrap_or(&q);
            let mut matches: Vec<(usize, String, String)> = SLASH_COMMANDS
                .iter()
                .filter_map(|(cmd, desc)| {
                    let low_cmd = cmd.trim_end().to_lowercase();
                    if low_cmd.starts_with(&q) {
                        Some((0, (*cmd).to_owned(), (*desc).to_owned()))
                    } else if low_cmd.contains(bare) || desc.to_lowercase().contains(bare) {
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
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        let token = &before[token_start..];
        if let Some(query) = token.strip_prefix('@') {
            if query.contains('#') || query.contains(':') {
                // User is specifying a `#L10-50` or `:10-50` line slice; stay out of the way!
                return None;
            }
            let q = query.to_lowercase();
            let mut items: Vec<String> = ["diff", "staged", "last"]
                .into_iter()
                .filter(|kw| q.is_empty() || kw.starts_with(&q))
                .map(str::to_owned)
                .collect();
            if !self.repo_files.is_empty() {
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
                items.extend(
                    scored
                        .into_iter()
                        .take(12usize.saturating_sub(items.len()))
                        .map(|(_, p)| p.clone()),
                );
            }
            if !items.is_empty() {
                return Some(PopupKind::Mention {
                    at_byte: token_start,
                    items,
                });
            }
        }
        if let Some(query) = token.strip_prefix('$') {
            if !self.skill_catalog.is_empty() {
                let q = query.to_lowercase();
                let mut scored: Vec<(usize, &(String, String, String))> = self
                    .skill_catalog
                    .iter()
                    .filter_map(|item| {
                        let low = item.0.to_lowercase();
                        if q.is_empty() || low.starts_with(&q) {
                            Some((0, item))
                        } else if low.contains(&q) || item.2.to_lowercase().contains(&q) {
                            Some((1, item))
                        } else {
                            None
                        }
                    })
                    .collect();
                scored.sort_by_key(|(rank, item)| (*rank, item.0.as_str()));
                let items: Vec<(String, String, String)> =
                    scored.into_iter().take(10).map(|(_, it)| it.clone()).collect();
                if !items.is_empty() {
                    return Some(PopupKind::Skill {
                        dollar_byte: token_start,
                        items,
                    });
                }
            }
        }
        None
    }

    pub fn desired_height(&self, total_width: usize, max_rows: u16) -> u16 {
        let pad_x = if total_width >= 120 {
            4
        } else if total_width >= 80 {
            3
        } else if total_width >= 56 {
            2
        } else {
            1
        };
        let text_w = total_width.saturating_sub(pad_x * 2 + 4).max(12);
        let lines = wrap_plain(&self.text, text_w);
        (lines.len() as u16 + 2).clamp(3, max_rows.max(3))
    }

    /// Claude Code hairline composer with symmetric outer margins.
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
        let pad_x: u16 = if area.width >= 120 {
            4
        } else if area.width >= 80 {
            3
        } else if area.width >= 56 {
            2
        } else {
            1
        };
        let pad_str = " ".repeat(pad_x as usize);
        let rule_len = area.width.saturating_sub(pad_x * 2) as usize;
        let is_bash = self.text.starts_with('!');
        let is_mem = self.text.starts_with('#') && !self.text.contains('\n');
        let (eff_rule_color, eff_prompt_color, glyph, mode_badge) = if is_bash {
            (BASH_MODE_COLOR, BASH_MODE_COLOR, "$ ", Some(" ! bash mode · runs locally "))
        } else if is_mem {
            (
                MEMORY_MODE_COLOR,
                MEMORY_MODE_COLOR,
                "♯ ",
                Some(" # memory mode · Enter saves to preferences.md "),
            )
        } else {
            (rule_color, prompt_color, "❯ ", None)
        };
        let rule_style = Style::default().fg(eff_rule_color);
        let top_rule = if let Some(badge) = mode_badge.filter(|b| rule_len > b.len() + 6) {
            let rem = rule_len.saturating_sub(display_width(badge) + 3);
            Line::from(vec![
                Span::styled(format!("{pad_str}───"), rule_style),
                Span::styled(
                    badge,
                    Style::default()
                        .fg(eff_rule_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("─".repeat(rem), rule_style),
            ])
        } else {
            Line::from(Span::styled(
                format!("{pad_str}{}", "─".repeat(rule_len)),
                rule_style,
            ))
        };
        let bot_rule = format!("{pad_str}{}", "─".repeat(rule_len));

        frame.render_widget(
            Paragraph::new(top_rule),
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
        );
        frame.render_widget(
            Paragraph::new(Span::styled(bot_rule, rule_style)),
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
            height: area.height.saturating_sub(2).max(1),
        };
        let prompt_head = format!("{pad_str}{glyph}");
        let prompt_cont = format!("{pad_str}  ");
        let prompt_cols = pad_x + 2;
        let text_w = area
            .width
            .saturating_sub(prompt_cols + pad_x)
            .max(10) as usize;

        let rendered_lines: Vec<Line<'static>> = if self.text.is_empty() {
            vec![Line::from(vec![
                Span::styled(
                    prompt_head.clone(),
                    Style::default()
                        .fg(eff_prompt_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(placeholder.to_owned(), Style::default().fg(MUTED)),
            ])]
        } else {
            wrap_plain(&self.text, text_w)
                .into_iter()
                .enumerate()
                .map(|(idx, segment)| {
                    let prefix = if idx == 0 {
                        prompt_head.clone()
                    } else {
                        prompt_cont.clone()
                    };
                    let mut spans = vec![Span::styled(
                        prefix,
                        Style::default()
                            .fg(eff_prompt_color)
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
                    body_area.x + prompt_cols + cur_col.min(text_w as u16),
                    body_area.y + cur_row,
                ));
            }
        }
    }

    /// Codex-style borderless scrollable command/file/skill popup with full-width accent selection bar.
    pub fn render_popup(&self, frame: &mut Frame, composer_area: Rect, transcript_h: u16) {
        let Some(popup) = self.active_popup() else {
            return;
        };
        let max_rows = (transcript_h.saturating_sub(1) as usize).clamp(1, 8);
        if composer_area.width < 14 {
            return;
        }
        let popup_w = composer_area.width.saturating_sub(2) as usize;
        let sel_bg = ACCENT;
        let sel_fg = Color::Rgb(22, 19, 17);
        let cmd_pad = if popup_w < 44 { 10 } else { 14 };

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
                        let cmd_col = format!("{:<width$}", cmd.trim_end(), width = cmd_pad);
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
            PopupKind::Skill { items, .. } => {
                let sel = self.popup_sel.min(items.len().saturating_sub(1));
                let start = sel.saturating_sub(max_rows.saturating_sub(1));
                let end = (start + max_rows).min(items.len());
                items[start..end]
                    .iter()
                    .enumerate()
                    .map(|(offset, (name, scope, desc))| {
                        let active = start + offset == sel;
                        let bg = if active { sel_bg } else { Color::Reset };
                        let marker = if active { " › $" } else { "   $" };
                        let tag = if scope == "vault" { " [vault] " } else { " " };
                        let name_col = format!("{name}{tag}");
                        let used = display_width(marker) + display_width(&name_col);
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
                                name_col,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slash_covers_all_codex_and_twig_commands() {
        assert_eq!(parse_slash("/plan", true), Some(SlashCommand::SetPlan(false)));
        assert_eq!(parse_slash("/plan on", false), Some(SlashCommand::SetPlan(true)));
        assert_eq!(parse_slash("/operator", false), Some(SlashCommand::SetOperator(None)));
        assert_eq!(parse_slash("/roles", false), Some(SlashCommand::ShowRoles));
        assert_eq!(
            parse_slash("/roles advisor gemini-4 xhigh", false),
            Some(SlashCommand::SetRole {
                role: "advisor".into(),
                model: Some("gemini-4".into()),
                effort: Some("xhigh".into()),
            })
        );
        assert_eq!(
            parse_slash("/dual-plan redesign auth", false),
            Some(SlashCommand::DualPlan("redesign auth".into()))
        );
        assert_eq!(
            parse_slash("/btw why is Lease flock?", false),
            Some(SlashCommand::SideQuestion("why is Lease flock?".into()))
        );
        assert_eq!(parse_slash("/walkthrough", false), Some(SlashCommand::RunWalkthrough));
        assert_eq!(
            parse_slash("/apply sub-123", false),
            Some(SlashCommand::ApplySession("sub-123".into()))
        );
        assert_eq!(parse_slash("/gc", false), Some(SlashCommand::GcSessions));
        assert_eq!(parse_slash("/effort", false), Some(SlashCommand::OpenEffortPicker));
        assert_eq!(
            parse_slash("/effort high", false),
            Some(SlashCommand::SetEffort("high".into()))
        );
        assert_eq!(parse_slash("/model", false), Some(SlashCommand::OpenModelPicker));
        assert_eq!(parse_slash("/new", false), Some(SlashCommand::NewSession));
        assert_eq!(parse_slash("/fork", false), Some(SlashCommand::ForkSession));
        assert_eq!(
            parse_slash("/rewind 2", false),
            Some(SlashCommand::RewindSession {
                turns: 2,
                restore_code: false,
            })
        );
        assert_eq!(
            parse_slash("/rewind 2 --code", false),
            Some(SlashCommand::RewindSession {
                turns: 2,
                restore_code: true,
            })
        );
        assert_eq!(parse_slash("/undo", false), Some(SlashCommand::UndoEdit));
        assert_eq!(
            parse_slash("/mcp", false),
            Some(SlashCommand::ManageMcp {
                action: "list".into(),
                server: None,
            })
        );
        assert_eq!(
            parse_slash("/mcp enable rea", false),
            Some(SlashCommand::ManageMcp {
                action: "enable".into(),
                server: Some("rea".into()),
            })
        );
        assert_eq!(
            parse_slash("/study auth module", false),
            Some(SlashCommand::SetStudy {
                enabled: Some(true),
                target: Some("auth module".into()),
            })
        );
        assert_eq!(parse_slash("/memory", false), Some(SlashCommand::EditMemory));
        assert_eq!(parse_slash("/init", false), Some(SlashCommand::InitProject));
        assert_eq!(parse_slash("/doctor", false), Some(SlashCommand::RunDoctor));
        assert_eq!(parse_slash("/archive", false), Some(SlashCommand::ArchiveSession));
        assert_eq!(parse_slash("/verify", false), Some(SlashCommand::VerifySession));
        assert_eq!(parse_slash("/export", false), Some(SlashCommand::ExportTranscript(None)));
        assert_eq!(
            parse_slash("/export out/chat.md", false),
            Some(SlashCommand::ExportTranscript(Some("out/chat.md".into())))
        );
        assert_eq!(
            parse_slash("/image shot.png", false),
            Some(SlashCommand::AttachImage(Some("shot.png".into())))
        );
        assert_eq!(
            parse_slash("/skills", false),
            Some(SlashCommand::ManageSkills {
                action: "list".into(),
                name: None,
                project: false,
            })
        );
        assert_eq!(
            parse_slash("/skills enable ponytail --project", false),
            Some(SlashCommand::ManageSkills {
                action: "enable".into(),
                name: Some("ponytail".into()),
                project: true,
            })
        );
        assert_eq!(
            parse_slash("/skills doctor", false),
            Some(SlashCommand::ManageSkills {
                action: "doctor".into(),
                name: None,
                project: false,
            })
        );
        assert_eq!(
            parse_slash("/pr --draft ship it", false),
            Some(SlashCommand::CreatePr(Some("--draft ship it".into())))
        );
        assert_eq!(
            parse_slash("/worktree feat-ui", false),
            Some(SlashCommand::ManageWorktree(Some("feat-ui".into())))
        );
        assert_eq!(
            parse_slash("/queue run benchmarks", false),
            Some(SlashCommand::QueueFollowUp(Some("run benchmarks".into())))
        );
        assert_eq!(
            parse_slash("/unqueue", false),
            Some(SlashCommand::QueueFollowUp(Some("clear".into())))
        );
    }

    #[test]
    fn composer_handles_multibyte_cjk_whitespace_without_panicking() {
        let mut comp = Composer::new(".");
        comp.repo_files = vec!["src/main.rs".into()];
        for c in "中文\u{3000}@main".chars() {
            comp.insert_char(c);
        }
        assert!(comp.has_popup());
        assert!(comp.accept_popup());
        assert_eq!(comp.text, "中文\u{3000}@src/main.rs ");
        comp.delete_word_back();
        assert_eq!(comp.text, "中文\u{3000}");
        comp.delete_word_back();
        assert_eq!(comp.text, "");
    }
}
