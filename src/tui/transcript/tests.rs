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

fn render_joined(t: &Transcript, width: usize) -> String {
    t.build_lines(
        width,
        "~/twig",
        "",
        false,
        Duration::from_secs(1),
        MotionMode::Reduced,
    )
    .iter()
    .map(ToString::to_string)
    .collect::<Vec<_>>()
    .join("\n")
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

    let collapsed = render_joined(&t, 72);
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

    t.toggle_expand();
    let expanded = render_joined(&t, 72);
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

    let joined = render_joined(&t, 76);
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
