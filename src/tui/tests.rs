use super::client::{LocalMsg, ServerMsg};
use super::*;
use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::backend::TestBackend;

fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn render_frame_text(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}

#[test]
fn composer_handles_cjk_editing_paste_pills_and_running_steer() {
    let mut app = App::new(".");
    for ch in "修复测试".chars() {
        app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(press(KeyCode::Backspace, KeyModifiers::NONE));
    app.handle_paste("line1\nline2\nline3\nline4\nline5");
    assert_eq!(
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        Action::SendTurn {
            input: "修复测line1\nline2\nline3\nline4\nline5".into(),
            role: None,
        }
    );
    assert!(app.running);

    for ch in "不要改接口".chars() {
        app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        Action::SendSteer("不要改接口".into())
    );
}

#[test]
fn codex_slash_commands_bang_shell_mouse_and_history_replay_work() {
    let mut app = App::new(".");
    for ch in "!git status".chars() {
        app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        Action::RunLocalShell("git status".into())
    );

    for ch in "/new".chars() {
        app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Slash(SlashCommand::NewSession)
    );
    assert_eq!(
        app.handle_key(press(KeyCode::Char('m'), KeyModifiers::ALT)),
        Action::ToggleMouse
    );

    app.pending_rpcs.insert(9, PendingRpc::History);
    app.apply_server_msg(ServerMsg::Reply {
        id: 9,
        result: json!({
            "sid": "sess-123",
            "title": "refactor tui",
            "model": "qwen3",
            "turns": 1,
            "context_bytes": 4096,
            "context_max": 10000,
            "messages": [{"role": "user", "content": "check build"}]
        }),
    });
    assert_eq!(app.transcript.len(), 1);
    assert_eq!(app.title.as_deref(), Some("refactor tui"));
    assert_eq!(app.context_pct(), Some(40));
}

#[test]
fn question_mark_toggles_codex_shortcut_overlay_and_esc_twice_clears_draft() {
    let mut app = App::new(".");
    app.handle_key(press(KeyCode::Char('?'), KeyModifiers::NONE));
    assert_eq!(app.footer_mode, FooterMode::ShortcutOverlay);
    let frame_text = render_frame_text(&mut app, 92, 24);
    assert!(frame_text.contains("compose in $EDITOR"));
    assert!(frame_text.contains("Shift+Tab"));

    app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.footer_mode, FooterMode::Contextual);
    app.handle_key(press(KeyCode::Char('a'), KeyModifiers::NONE));
    app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.footer_mode, FooterMode::EscHint);
    app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.composer.is_empty());
}

#[test]
fn approval_selector_and_copy_search_shortcuts_work() {
    let mut app = App::new(".");
    app.apply_event(&json!({
        "ev": "approval.requested",
        "id": "call_1",
        "name": "bash",
        "args": {"command": "git reset --hard"}
    }));
    assert_eq!(
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        Action::ResolveApproval {
            id: "call_1".into(),
            allow: true,
            remember: false,
            note: None,
        }
    );
    app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        Action::ResolveApproval {
            id: "call_1".into(),
            allow: true,
            remember: true,
            note: None,
        }
    );
    app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        Action::ResolveApproval {
            id: "call_1".into(),
            allow: false,
            remember: false,
            note: None,
        }
    );
    app.approvals.clear();

    app.transcript
        .push_assistant("Code:\n```rust\nlet b = 2;\n```".into());
    assert_eq!(
        app.handle_key(press(KeyCode::Char('y'), KeyModifiers::ALT)),
        Action::CopyCode("let b = 2;".into())
    );
    assert_eq!(
        app.handle_key(
            press(KeyCode::Char('Y'), KeyModifiers::ALT | KeyModifiers::SHIFT)
        ),
        Action::CopyCode("Code:\n```rust\nlet b = 2;\n```".into())
    );
    app.handle_key(press(KeyCode::Char('f'), KeyModifiers::CONTROL));
    for ch in "let b".chars() {
        app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let _ = render_frame_text(&mut app, 90, 24);
    assert_eq!(app.transcript.search_match_count(), 1);
}

#[test]
fn renders_welcome_telemetry_tools_approval_and_new_commands() {
    let mut app = App::new("/tmp/demo");
    app.env_summary = "skills 10 (+13 vault)  ·  docs AGENTS.md  ·  mcp 0  ·  sandbox off".into();
    let welcome = render_frame_text(&mut app, 110, 28);
    assert!(welcome.contains("✻ twig") && !welcome.contains("╭─"));
    assert!(welcome.contains("skills 10 (+13 vault)") && welcome.contains("docs AGENTS.md"));
    assert!(!welcome.contains("ctx 0%"));
    let phone = render_frame_text(&mut app, 36, 16);
    assert!(phone.contains("✻ twig") && phone.contains("Ask twig… (/ @ $ !)"));
    assert!(render_frame_text(&mut app, 60, 20).contains("✻ twig"));
    assert!(render_frame_text(&mut app, 180, 40).contains("skills 10 (+13 vault)"));

    assert_eq!(
        app.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
        Action::SetPlan(true)
    );
    app.plan_mode = true;
    assert_eq!(
        app.dispatch_input("/effort high"),
        Action::SetEffort("high".into())
    );
    assert_eq!(
        app.dispatch_input("/go focus on error cases"),
        Action::Slash(SlashCommand::ExecutePlan(Some(
            "focus on error cases".into()
        )))
    );
    assert_eq!(
        app.dispatch_input("/commit"),
        Action::Slash(SlashCommand::CommitChanges(None))
    );
    assert_eq!(
        app.dispatch_input("/fork"),
        Action::Slash(SlashCommand::ForkSession)
    );

    app.apply_server_msg(ServerMsg::Reply {
        id: 1,
        result: json!({
            "pong": true, "sid": "abc123456789", "model": "demo-model",
            "effort": "high", "plan": true, "context_bytes": 3400, "context_max": 10000
        }),
    });
    app.apply_event(&json!({
        "ev": "todo.updated",
        "items": [
            {"content": "Inspect runtime", "status": "completed"},
            {"content": "Wire Codex parity", "status": "in_progress"}
        ]
    }));
    app.apply_event(&json!({
        "ev": "model.usage",
        "usage": {"input_tokens": 1000, "output_tokens": 200, "cached_input_tokens": 800}
    }));
    app.pending_rpcs.insert(17, PendingRpc::Verify);
    app.apply_server_msg(ServerMsg::Reply {
        id: 17,
        result: json!({
            "chain": {"ok": true, "entries": 12},
            "guard": {"samples": 2, "stall": 0, "digest": "abcdef123456"},
            "policy": {"hits": 5, "misses": 1, "size": 3},
            "plan": true, "read_only": true
        }),
    });
    app.apply_event(&json!({
        "ev": "tool.started", "id": "t1", "tool": "bash",
        "args": {"command": "git diff"}, "tier": "auto"
    }));
    app.apply_event(
        &json!({"ev": "tool.output", "id": "t1", "text": "@@ -1,1 +1,1 @@\n-old\n+new\n"}),
    );
    app.apply_event(&json!({"ev": "tool.completed", "id": "t1", "bytes": 24, "exit_code": 0}));

    app.apply_event(&json!({
        "ev": "task.started", "id": "bg-1", "command": "cargo test", "running": 1
    }));
    app.apply_event(&json!({
        "ev": "subagent.started", "role": "reviewer", "task": "Audit session recovery", "parallel": true
    }));
    app.apply_local_msg(LocalMsg::GitStat(Some((42, 8))));
    let active_frame = render_frame_text(&mut app, 130, 28);
    assert!(active_frame.contains("◈ plan"));
    assert!(active_frame.contains("+42") && active_frame.contains("-8"));
    assert!(active_frame.contains("⚙ 1 task"));
    assert!(active_frame.contains("high"));
    assert!(active_frame.contains("Plan progress  1/2"));
    assert!(active_frame.contains("◈ Subagents") && active_frame.contains("Audit session recovery"));
    assert!(active_frame.contains("ctx 34%"));
    assert!(active_frame.contains("cache 80%"));
    assert!(active_frame.contains("blake3 chain: ok (12 entries)"));
    assert!(active_frame.contains("$ Bash"));

    app.transcript.push_user("first prompt".into());
    app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.footer_mode, FooterMode::RewindHint);
    app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
    assert!(matches!(
        app.overlay_picker,
        Some(OverlayPicker::Rewind { .. })
    ));
    assert_eq!(
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Slash(SlashCommand::RewindSession {
            turns: 1,
            restore_code: false,
            prefill: Some("first prompt".into()),
        })
    );

    app.apply_event(&json!({
        "ev": "approval.requested", "id": "t2", "name": "bash",
        "args": {"command": "rm -rf /"}
    }));
    let approval_frame = render_frame_text(&mut app, 100, 28);
    assert!(approval_frame.contains("Approve Bash?"));
    assert!(approval_frame.contains("1. Yes, allow once"));
    assert!(approval_frame.contains("2. Always allow this pattern"));
}

#[test]
fn claude_cli_benchmark_ux_diff_approval_modes_queue_and_drawers_work() {
    let dir = std::env::temp_dir().join(format!("twig-claude-ux-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "line1\nline2_target\nline3\nline4\n").unwrap();

    let mut app = App::new(dir.to_str().unwrap());

    assert_eq!(
        app.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
        Action::SetPlan(true)
    );
    app.plan_mode = true;
    assert_eq!(
        app.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
        Action::SetMode {
            plan: false,
            gate_edits: true
        }
    );
    app.plan_mode = false;
    app.gate_edits = true;
    assert!(render_frame_text(&mut app, 110, 24).contains("review-edits"));
    assert_eq!(
        app.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
        Action::SetMode {
            plan: false,
            gate_edits: false
        }
    );
    app.gate_edits = false;

    app.transcript.push_user("修改配置".into());
    app.transcript.push_tool_snapshot(
        "r1".into(),
        "read".into(),
        "src/lib.rs".into(),
        "auto".into(),
        "line1\n".into(),
        Some(0),
    );
    app.transcript.push_tool_snapshot(
        "g1".into(),
        "grep".into(),
        "line2".into(),
        "auto".into(),
        "src/lib.rs:2".into(),
        Some(0),
    );
    app.transcript.push_tool_snapshot(
        "e1".into(),
        "edit".into(),
        "src/lib.rs".into(),
        "audit".into(),
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n line1\n-let port = 8080;\n+let port = 9090;\n line3\n".into(),
        Some(0),
    );
    let chat_view = render_frame_text(&mut app, 110, 28);
    assert!(chat_view.contains("▸ 2 tools (1 read · 1 search)"));
    assert!(
        chat_view.contains("● Update(src/lib.rs)")
            && chat_view.contains("+1")
            && chat_view.contains("-1")
    );
    assert!(chat_view.contains("let port = 9090;"));

    app.apply_event(&json!({
        "ev": "approval.requested",
        "id": "ed_1",
        "name": "edit",
        "args": {"path": "src/lib.rs", "old_string": "let a = 1;", "new_string": "let a = 2;"}
    }));
    let ed_frame = render_frame_text(&mut app, 110, 28);
    assert!(ed_frame.contains("Approve Edit?") && ed_frame.contains("let a = 2;"));
    app.handle_key(press(KeyCode::Tab, KeyModifiers::NONE));
    for ch in "use const".chars() {
        app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        Action::ResolveApproval {
            id: "ed_1".into(),
            allow: false,
            remember: false,
            note: Some("use const".into()),
        }
    );
    app.approvals.clear();

    app.composer.set("!cargo check");
    assert!(render_frame_text(&mut app, 100, 24).contains("! bash mode"));
    app.composer.set("");
    assert_eq!(
        app.dispatch_input("# Always run deslop-please"),
        Action::None
    );
    assert!(matches!(
        app.overlay_picker,
        Some(OverlayPicker::MemorySave { .. })
    ));
    assert_eq!(
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    assert!(
        std::fs::read_to_string(dir.join(".twig/preferences.md"))
            .unwrap()
            .contains("Always run deslop-please")
    );

    app.running = true;
    assert_eq!(
        app.handle_key(press(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        Action::BackgroundActive
    );
    app.composer.set("检查一下 @src/lib.rs#L2-3");
    app.handle_key(press(KeyCode::Enter, KeyModifiers::ALT));
    assert_eq!(app.queued_turns.len(), 1);
    assert!(render_frame_text(&mut app, 110, 28).contains("↳ Queued"));
    app.handle_key(press(KeyCode::Up, KeyModifiers::NONE));
    assert!(app.queued_turns.is_empty());
    assert_eq!(app.composer.raw_text(), "检查一下 @src/lib.rs#L2-3");
    let (turn_params, _) = self::client::build_turn_params(
        dir.to_str().unwrap(),
        app.composer.take_submitted(),
        Vec::new(),
        None,
    );
    assert!(turn_params["input"].as_str().unwrap().contains("line2_target"));

    app.btw_card = Some(BtwCard {
        question: "why flock?".into(),
        answer: Some("Crash-safe kernel release.".into()),
    });
    assert!(render_frame_text(&mut app, 110, 28).contains("Crash-safe kernel release."));
    app.handle_key(press(KeyCode::Char(' '), KeyModifiers::NONE));
    assert!(app.btw_card.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn subagent_thread_tree_interactive_chat_mouse_copy_and_todo_lifecycle_work() {
    let mut app = App::new(".");
    app.motion = MotionMode::Reduced;

    let cells_before = app.transcript.len();
    app.apply_event(&json!({
        "ev": "subagent.started", "id": "sub-1", "role": "scout", "task": "Map tui modules", "parallel": true
    }));
    app.apply_event(&json!({
        "ev": "subagent.started", "id": "sub-2", "role": "code", "task": "Fix web timeout", "parallel": true
    }));
    app.apply_event(&json!({
        "ev": "subagent.progress", "id": "sub-1", "sid": "scout-sid-1", "role": "scout",
        "activity": "Read(src/tui/mod.rs)", "turns": 2, "tool_count": 5
    }));
    app.apply_event(&json!({
        "ev": "subagent.stream", "id": "sub-1", "role": "scout",
        "child": {"ev": "model.delta", "text": "Subagent-1 live finding: TUI uses Ratatui."}
    }));
    assert_eq!(app.transcript.len(), cells_before);
    assert!(render_frame_text(&mut app, 128, 28).contains("Read(src/tui/mod.rs) · 5 tools · t2"));

    app.apply_event(&json!({
        "ev": "subagent.completed", "id": "sub-1", "sid": "scout-sid-1", "role": "scout", "ok": true, "elapsed_secs": 3
    }));
    app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(press(KeyCode::Char('v'), KeyModifiers::NONE));
    assert_eq!(app.viewing_subagent.as_deref(), Some("sub-1"));

    for ch in "Check line 120 too".chars() {
        app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        Action::SendSubagentTurn {
            id: "sub-1".into(),
            child_sid: "scout-sid-1".into(),
            role: "scout".into(),
            model: None,
            input: "Check line 120 too".into(),
        }
    );
    app.handle_key(press(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.viewing_subagent.as_deref(), Some("sub-2"));
    app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.viewing_subagent.is_none());

    app.pending_rpcs.insert(42, PendingRpc::History);
    app.apply_server_msg(ServerMsg::Reply {
        id: 42,
        result: json!({
            "sid": "main-99",
            "messages": [{"role": "user", "content": "audit"}],
            "child_threads": [{
                "sid": "scout-restored",
                "role": "scout",
                "task": "Audit MCP",
                "turns": 1,
                "tool_count": 3,
                "messages": [
                    {"role": "user", "content": "Audit MCP"},
                    {"role": "assistant", "content": "MCP findings ready."}
                ]
            }]
        }),
    });
    assert_eq!(app.subagents.len(), 1);
    app.viewing_subagent = Some("scout-restored".into());
    assert!(render_frame_text(&mut app, 110, 24).contains("MCP findings ready."));
    app.viewing_subagent = None;

    app.running = true;
    app.apply_event(&json!({
        "ev": "todo.updated",
        "items": [
            {"content": "Step A", "status": "completed"},
            {"content": "Step B", "status": "in_progress"},
            {"content": "Step C", "status": "pending"},
            {"content": "Step D", "status": "pending"},
            {"content": "Step E", "status": "pending"}
        ]
    }));
    app.running = false;
    let idle_todo = render_frame_text(&mut app, 110, 26);
    assert!(
        idle_todo.contains("Plan progress  1/5  · idle  · +1 more")
            && idle_todo.contains("◐ Step B")
    );
    app.todos_updated_in_turn = false;
    app.pending_rpcs.insert(77, PendingRpc::Turn);
    app.apply_server_msg(ServerMsg::Reply {
        id: 77,
        result: json!({"done": true, "reason": "final", "text": "All done"}),
    });
    assert!(app.todos.is_empty());

    app.transcript.clear();
    for i in 0..30 {
        app.transcript.push_user(format!("Prompt line {i}"));
        app.transcript.push_assistant(format!("Answer line {i}"));
    }
    let _ = render_frame_text(&mut app, 100, 24);
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 2,
        row: 2,
        modifiers: KeyModifiers::NONE,
    });
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: 18,
        row: 2,
        modifiers: KeyModifiers::NONE,
    });
    assert!(matches!(
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 18,
            row: 2,
            modifiers: KeyModifiers::NONE,
        }),
        Action::CopyCode(_)
    ));
    app.handle_key(press(KeyCode::Home, KeyModifiers::NONE));
    let _ = render_frame_text(&mut app, 100, 24);
    app.apply_event(&json!({"ev": "model.delta", "text": "Tail update"}));
    assert!(render_frame_text(&mut app, 100, 24).contains("Prompt line 0"));
}

#[test]
fn pending_rpc_error_and_session_reset_clean_up_all_trackers() {
    let mut app = App::new(".");
    app.btw_card = Some(BtwCard {
        question: "stuck?".into(),
        answer: None,
    });
    app.pending_rpcs.insert(5, PendingRpc::Btw);
    app.apply_server_msg(ServerMsg::Error {
        id: Some(5),
        message: "provider timeout".into(),
    });
    assert!(
        app.btw_card.is_none(),
        "/btw RPC error must dismiss stuck Thinking card"
    );
    assert!(app.pending_rpcs.is_empty());

    let act = app.begin_turn("run".into(), "Thinking", Some("code"), true);
    assert_eq!(
        act,
        Action::SendTurn {
            input: "run".into(),
            role: Some("code"),
        }
    );
    app.pending_rpcs.insert(6, PendingRpc::Turn);
    app.pending_rpcs.insert(7, PendingRpc::History);
    app.apply_server_msg(ServerMsg::Error {
        id: Some(6),
        message: "turn failed".into(),
    });
    assert!(!app.running);
    assert!(app.pending_rpcs.contains_key(&7));
    app.reset_session_view();
    assert!(
        app.pending_rpcs.is_empty(),
        "reset_session_view must clear History and all pending RPCs"
    );
}
