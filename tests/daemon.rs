use serde_json::json;
use twig::{
    config::HarnessConfig,
    stub::{call, StubLlm},
    Daemon, Record,
};

#[test]
fn approving_recovered_call_never_replays_unknown_side_effect_and_survives_restart() {
    let dir = std::env::temp_dir().join(format!("twig-daemon-recovery-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let runs = dir.join("runs");
    {
        let mut record = Record::open(runs.join("resume.jsonl"), "resume").unwrap();
        let assistant = json!({"role":"assistant","content":"","tool_calls":[{"id":"pending","type":"function","function":{"name":"bash","arguments":"{\"command\":\"printf executed > result\"}"}}]});
        record
            .append("message", json!({"message": assistant.clone()}))
            .unwrap();
        record
            .append(
                "approval.requested",
                json!({"id":"pending","name":"bash","args":{"command":"printf executed > result"}}),
            )
            .unwrap();
        record
            .append("messages.checkpoint", json!({"messages": [assistant]}))
            .unwrap();
    }
    let model = StubLlm::new(vec![]);
    let mut daemon = Daemon::new(&model, &dir, &runs, "resume", &HarnessConfig::default()).unwrap();
    let out = daemon
        .dispatch(
            "approval.resolve",
            &json!({"id": "pending", "allow": true}),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(out["recovered"], true);
    assert_eq!(out["replayed"], false);
    assert!(!dir.join("result").exists());
    assert_eq!(
        daemon
            .dispatch("approval.list", &json!({}), &mut |_| {})
            .unwrap()["pending"],
        json!([])
    );
    assert!(Record::verify_chain(daemon.record_path()).is_ok());
    drop(daemon);

    let mut resumed =
        Daemon::new(&model, &dir, &runs, "resume", &HarnessConfig::default()).unwrap();
    let hist = resumed
        .dispatch("session.history", &json!({}), &mut |_| {})
        .unwrap();
    assert_eq!(
        hist["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "pending")
            .count(),
        1
    );
    assert!(!dir.join("result").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn approved_started_crash_before_result_stays_unknown_and_never_replays() {
    let dir = std::env::temp_dir().join(format!(
        "twig-daemon-approved-started-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let runs = dir.join("runs");
    {
        let mut record = Record::open(runs.join("crash.jsonl"), "crash").unwrap();
        let assistant = json!({"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"printf ran > side_effect\"}"}}]});
        record
            .append("message", json!({"message": assistant.clone()}))
            .unwrap();
        record
            .append(
                "approval.requested",
                json!({"id":"c1","name":"bash","args":{"command":"printf ran > side_effect"}}),
            )
            .unwrap();
        record
            .append("approval.resolved", json!({"id":"c1","allow":true}))
            .unwrap();
        record
            .append(
                "tool.started",
                json!({"id":"c1","tool":"bash","args":{"command":"printf ran > side_effect"}}),
            )
            .unwrap();
        record
            .append("messages.checkpoint", json!({"messages": [assistant]}))
            .unwrap();
    }
    let model = StubLlm::new(vec![]);
    let mut daemon = Daemon::new(&model, &dir, &runs, "crash", &HarnessConfig::default()).unwrap();
    let list = daemon
        .dispatch("approval.list", &json!({}), &mut |_| {})
        .unwrap();
    assert_eq!(list["pending"].as_array().unwrap().len(), 1);
    assert_eq!(list["pending"][0]["recovered"], true);
    let out = daemon
        .dispatch(
            "approval.resolve",
            &json!({"id": "c1", "allow": true}),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(out["recovered"], true);
    assert_eq!(out["replayed"], false);
    assert!(!dir.join("side_effect").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn active_noninteractive_approval_resolves_and_drains_deferred_calls() {
    let dir = std::env::temp_dir().join(format!("twig-daemon-active-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "hello\n").unwrap();
    let runs = dir.join("runs");
    let llm = StubLlm::new(vec![
        StubLlm::with_tools(
            "",
            vec![
                call("a", "read", json!({"path": "note.txt"})),
                call("d", "bash", json!({"command": "rm -rf /"})),
                call("e", "bash", json!({"command": "printf ok > after.txt"})),
            ],
        ),
        StubLlm::text("done"),
    ]);
    let mut daemon = Daemon::new(&llm, &dir, &runs, "active", &HarnessConfig::default()).unwrap();
    let paused = daemon
        .dispatch("turn", &json!({"input": "go"}), &mut |_| {})
        .unwrap();
    assert_eq!(paused["reason"], "awaiting_approval");
    let resolved = daemon
        .dispatch(
            "approval.resolve",
            &json!({"id": "d", "allow": false}),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(resolved["allow"], false);
    assert_eq!(resolved["recovered"], false);
    assert!(dir.join("after.txt").exists());
    let hist = daemon
        .dispatch("session.history", &json!({}), &mut |_| {})
        .unwrap();
    twig::runtime::context::validate_tool_pairs(hist["messages"].as_array().unwrap()).unwrap();
    assert_eq!(
        daemon
            .dispatch("approval.list", &json!({}), &mut |_| {})
            .unwrap()["pending"],
        json!([])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn operator_btw_and_git_worktree_rewind_work_without_context_pollution() {
    let dir = std::env::temp_dir().join(format!("twig-daemon-m4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let runs = dir.join("runs");

    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status()
        .unwrap()
        .success());
    let _ = std::process::Command::new("git")
        .args(["config", "user.email", "t@example.com"])
        .current_dir(&dir)
        .status();
    let _ = std::process::Command::new("git")
        .args(["config", "user.name", "twig"])
        .current_dir(&dir)
        .status();
    std::fs::write(dir.join("app.txt"), "v1\n").unwrap();
    let _ = std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&dir)
        .status();
    assert!(std::process::Command::new("git")
        .args(["commit", "-q", "-m", "init"])
        .current_dir(&dir)
        .status()
        .unwrap()
        .success());

    let llm = StubLlm::new(vec![
        StubLlm::text("turn1 reply"),
        StubLlm::text("ephemeral btw answer"),
    ]);
    let mut daemon = Daemon::new(&llm, &dir, &runs, "m4-test", &HarnessConfig::default()).unwrap();

    let op = daemon
        .dispatch("mode.operator", &json!({"enabled": true}), &mut |_| {})
        .unwrap();
    assert_eq!(op["operator"], true);

    let t1 = daemon
        .dispatch("turn", &json!({"input": "inspect app"}), &mut |_| {})
        .unwrap();
    assert_eq!(t1["done"], true);
    let before = daemon
        .dispatch("session.history", &json!({}), &mut |_| {})
        .unwrap()["messages"]
        .as_array()
        .unwrap()
        .len();

    let btw = daemon
        .dispatch(
            "session.btw",
            &json!({"question": "what file exists?"}),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(btw["answer"], "ephemeral btw answer");
    let after = daemon
        .dispatch("session.history", &json!({}), &mut |_| {})
        .unwrap()["messages"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(after, before);

    std::fs::write(dir.join("app.txt"), "mutated-v2\n").unwrap();
    std::fs::write(dir.join("stray.txt"), "untracked\n").unwrap();
    let rw = daemon
        .dispatch(
            "session.rewind",
            &json!({"turns": 1, "restore_code": true}),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(rw["rewound_turns"], 1);
    assert_eq!(std::fs::read_to_string(dir.join("app.txt")).unwrap(), "v1\n");
    assert!(!dir.join("stray.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transient_plan_reminders_never_leak_after_plan_exit_and_git_commit_works() {
    let dir = std::env::temp_dir().join(format!("twig-rem-commit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let runs = dir.join("runs");

    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status()
        .unwrap()
        .success());
    let _ = std::process::Command::new("git")
        .args(["config", "user.email", "t@example.com"])
        .current_dir(&dir)
        .status();
    let _ = std::process::Command::new("git")
        .args(["config", "user.name", "twig"])
        .current_dir(&dir)
        .status();
    std::fs::write(dir.join("lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
    let _ = std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&dir)
        .status();
    assert!(std::process::Command::new("git")
        .args(["commit", "-q", "-m", "feat: initial"])
        .current_dir(&dir)
        .status()
        .unwrap()
        .success());

    let llm = StubLlm::new(vec![
        StubLlm::text("planned"),
        StubLlm::text("implemented"),
        StubLlm::text("feat: bump v() to 2"),
    ]);
    let mut daemon = Daemon::new(&llm, &dir, &runs, "rem-test", &HarnessConfig::default()).unwrap();

    daemon
        .dispatch("mode.set", &json!({"plan": true}), &mut |_| {})
        .unwrap();
    daemon
        .dispatch("turn", &json!({"input": "design change"}), &mut |_| {})
        .unwrap();
    let h1 = daemon
        .dispatch("session.history", &json!({}), &mut |_| {})
        .unwrap();
    assert!(h1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"].as_str().is_some_and(|s| s.contains("Plan mode is active"))));

    daemon
        .dispatch("mode.set", &json!({"plan": false}), &mut |_| {})
        .unwrap();
    let h2 = daemon
        .dispatch("session.history", &json!({}), &mut |_| {})
        .unwrap();
    assert!(!h2["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"].as_str().is_some_and(|s| s.contains("Plan mode is active"))));

    daemon
        .dispatch("turn", &json!({"input": "implement it"}), &mut |_| {})
        .unwrap();
    std::fs::write(dir.join("lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    let committed = daemon
        .dispatch("git.commit", &json!({"hint": "bump v"}), &mut |_| {})
        .unwrap();
    assert_eq!(committed["subject"], "feat: bump v() to 2");
    assert!(!committed["sha"].as_str().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn secret_read_requires_approval_and_exit_2_stop_hook_feeds_back_to_model() {
    let dir = std::env::temp_dir().join(format!("twig-secret-hook-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".twig")).unwrap();
    let runs = dir.join("runs");
    std::fs::write(dir.join(".env"), "SECRET=123\n").unwrap();
    std::fs::write(
        dir.join(".twig/project.json"),
        r#"{"stop_hook": "test -f ready.flag || { echo 'missing ready.flag' >&2; exit 2; }"}"#,
    )
    .unwrap();

    let llm_secret = StubLlm::new(vec![StubLlm::with_tools(
        "",
        vec![call("s1", "read", json!({"path": ".env"}))],
    )]);
    let mut d_sec =
        Daemon::new(&llm_secret, &dir, &runs, "sec-test", &HarnessConfig::default()).unwrap();
    let paused = d_sec
        .dispatch("turn", &json!({"input": "check env"}), &mut |_| {})
        .unwrap();
    assert_eq!(paused["reason"], "awaiting_approval");

    let llm_hook = StubLlm::new(vec![
        StubLlm::text("done too early"),
        StubLlm::with_tools(
            "",
            vec![call(
                "w1",
                "write",
                json!({"path": "ready.flag", "content": "ok\n"}),
            )],
        ),
        StubLlm::text("now truly done"),
    ]);
    let mut d_hook =
        Daemon::new(&llm_hook, &dir, &runs, "hook-test", &HarnessConfig::default()).unwrap();
    let fin = d_hook
        .dispatch("turn", &json!({"input": "finish task"}), &mut |_| {})
        .unwrap();
    assert_eq!(fin["done"], true);
    assert_eq!(fin["reason"], "final");
    assert!(dir.join("ready.flag").is_file());
    assert!(llm_hook
        .seen()
        .iter()
        .any(|m| m.contains("missing ready.flag")));

    let llm_study = StubLlm::new(vec![
        StubLlm::text("[Observed] ok"),
        StubLlm::text("normal reply"),
        StubLlm::with_tools(
            "",
            vec![call(
                "cp1",
                "write",
                json!({"path": ".twig/project.json", "content": "{}"}),
            )],
        ),
    ]);
    let mut d_st =
        Daemon::new(&llm_study, &dir, &runs, "study-test", &HarnessConfig::default()).unwrap();
    d_st.dispatch("mode.study", &json!({"enabled": true}), &mut |_| {})
        .unwrap();
    d_st.dispatch("turn", &json!({"input": "study target"}), &mut |_| {})
        .unwrap();
    let hs1 = d_st
        .dispatch("session.history", &json!({}), &mut |_| {})
        .unwrap();
    assert!(hs1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"].as_str().is_some_and(|s| s.contains("REA Study Mode is active"))));
    d_st.dispatch("mode.study", &json!({"enabled": false}), &mut |_| {})
        .unwrap();
    d_st.dispatch("turn", &json!({"input": "normal turn"}), &mut |_| {})
        .unwrap();
    let hs2 = d_st
        .dispatch("session.history", &json!({}), &mut |_| {})
        .unwrap();
    assert!(!hs2["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"].as_str().is_some_and(|s| s.contains("REA Study Mode is active"))));
    let cp_pause = d_st
        .dispatch("turn", &json!({"input": "edit project config"}), &mut |_| {})
        .unwrap();
    assert_eq!(cp_pause["reason"], "awaiting_approval");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tools_edit_write_grep_glob_undo_xrefs_deps_binary_and_secret_filters_work() {
    use twig::runtime::tools::Tools;

    let tc = |name: &str, args| call("t", name, args);
    let d = std::env::temp_dir().join(format!("twig-daemon-tools-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("note.txt"), "hello\nworld\n").unwrap();

    let mut t = Tools::new(&d);
    let created = t.run(&tc(
        "write",
        json!({"path": "src/lib.rs", "content": "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n"}),
    ));
    assert!(created.contains("created src/lib.rs"));
    let diff = t.run(&tc(
        "edit",
        json!({"path": "src/lib.rs", "old_string": "a + b", "new_string": "a.saturating_add(b)"}),
    ));
    assert!(diff.contains("-    a + b") && diff.contains("+    a.saturating_add(b)"));
    assert!(t
        .run(&tc("grep", json!({"pattern": "saturating_add", "glob": "*.rs"})))
        .contains("src/lib.rs:2:"));
    assert_eq!(t.run(&tc("glob", json!({"pattern": "**/*.rs"}))), "src/lib.rs");

    assert!(t.undo_last_edit().unwrap().contains("restored src/lib.rs"));
    assert!(std::fs::read_to_string(d.join("src/lib.rs")).unwrap().contains("a + b"));
    assert!(t.undo_last_edit().unwrap().contains("removed newly created"));
    assert!(!d.join("src/lib.rs").exists());

    t.apply_plan_mode(true);
    assert!(t
        .run(&tc("write", json!({"path": "x.txt", "content": "hi"})))
        .starts_with("refused"));
    assert!(t.undo_last_edit().is_err());
    t.apply_plan_mode(false);

    std::fs::write(
        d.join("src/engine.rs"),
        "use std::path::Path;\n\n// compute_score comment ignored\npub fn compute_score(x: i32) -> i32 {\n    x * 2\n}\n\npub fn run_pipeline(v: i32) -> i32 {\n    let _msg = \"compute_score string literal ignored\";\n    compute_score(v) + 1\n}\n\npub static DEFAULT_SCORE: i32 = compute_score(3);\n",
    )
    .unwrap();
    std::fs::write(d.join(".env"), "API_TOKEN=super-secret-prod\n").unwrap();
    std::fs::write(d.join(".env.example"), "API_TOKEN=placeholder\n").unwrap();
    let mut elf = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    elf.extend_from_slice(&2u16.to_le_bytes());
    elf.extend_from_slice(&0x3Eu16.to_le_bytes());
    elf.extend_from_slice(&[0u8; 12]);
    std::fs::write(d.join("artifact.bin"), &elf).unwrap();

    let xref = t.run(&tc("outline", json!({"path": "src", "symbol": "compute_score"})));
    assert!(xref.contains("[def] src/engine.rs:4:"), "{xref}");
    assert!(xref.contains("[in fn run_pipeline] src/engine.rs:10:"), "{xref}");
    assert!(xref.contains("[in top-level] src/engine.rs:13:"), "{xref}");
    assert!(!xref.contains("src/engine.rs:3:") && !xref.contains("src/engine.rs:9:"));

    assert!(t
        .run(&tc("outline", json!({"path": "src/engine.rs", "deps": true})))
        .contains("use std::path::Path"));

    let bin_info = t.run(&tc("read", json!({"path": "artifact.bin"})));
    assert!(bin_info.contains("ELF 64-bit LSB executable (x86_64)") && bin_info.contains("blake3:"));
    assert!(t
        .run(&tc("read", json!({"path": "artifact.bin", "start_line": 1, "limit": 1})))
        .contains("00000000  7f 45 4c 46"));

    let wasm_bytes: &[u8] = &[
        0x00, b'a', b's', b'm', 0x01, 0x00, 0x00, 0x00,
        0x02, 0x0B, 0x01, 0x03, b'e', b'n', b'v', 0x03, b'l', b'o', b'g', 0x00, 0x00,
        0x07, 0x07, 0x01, 0x03, b'r', b'u', b'n', 0x00, 0x00,
    ];
    std::fs::write(d.join("mod.wasm"), wasm_bytes).unwrap();
    let wasm_info = t.run(&tc("read", json!({"path": "mod.wasm"})));
    assert!(wasm_info.contains("wasm_imports: env::log") && wasm_info.contains("wasm_exports: run:fn"));

    let har_json = json!({
        "log": {
            "entries": [
                {
                    "request": {"method": "GET", "url": "https://ex.test/logo.png", "headers": []},
                    "response": {"status": 200, "content": {"mimeType": "image/png", "text": ""}}
                },
                {
                    "request": {
                        "method": "POST",
                        "url": "https://ex.test/api/v1/items",
                        "headers": [{"name": "Authorization", "value": "Bearer super-secret-jwt"}],
                        "postData": {"text": "{\"title\":\"demo\",\"count\":3}"}
                    },
                    "response": {
                        "status": 201,
                        "content": {"mimeType": "application/json; charset=utf-8", "text": "{\"id\":42,\"ok\":true}"}
                    }
                }
            ]
        }
    });
    std::fs::write(d.join("trace.har"), har_json.to_string()).unwrap();
    let har_info = t.run(&tc("read", json!({"path": "trace.har"})));
    assert!(
        har_info.contains("1 API call(s), 1 static asset(s) filtered")
            && har_info.contains("Authorization=<redacted>")
            && !har_info.contains("super-secret-jwt")
            && har_info.contains("req:{count, title}")
    );

    let grep_res = t.run(&tc("grep", json!({"pattern": "API_TOKEN"})));
    assert!(grep_res.contains(".env.example:1:") && !grep_res.contains("super-secret-prod"));
    let direct_secret_grep = t.run(&tc("grep", json!({"pattern": "API_TOKEN", "path": ".env"})));
    assert!(direct_secret_grep.starts_with("refused:") && !direct_secret_grep.contains("super-secret-prod"));

    let _ = t.run(&tc("read", json!({"path": "src/engine.rs"})));
    let bash_out = t.run(&tc(
        "bash",
        json!({"command": "printf '// formatted\\n' >> src/engine.rs"}),
    ));
    assert!(bash_out.contains("modified previously read file(s): src/engine.rs"));

    let _ = std::fs::remove_dir_all(&d);
}
