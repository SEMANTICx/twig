//! twig — CLI.

use std::io::BufReader;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use twig::classify;
use twig::config::HarnessConfig;
use twig::daemon::Daemon;
use twig::decisions::Decisions;
use twig::playbook::{route, Playbook};
use twig::provider::{Llm, Provider};
use twig::record::Record;
use twig::stub::{call, StubLlm};

fn tempdir(tag: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dir = std::env::temp_dir().join(format!(
        "twig-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("tempdir");
    dir
}

fn check(name: &str, ok: bool, detail: impl std::fmt::Display) {
    let mark = if ok { "ok  " } else { "FAIL" };
    println!("  [{mark}] {name} :: {detail}");
    assert!(ok, "{name}");
}

fn selfcheck() {
    println!("policy");
    {
        let p = twig::policy::Policy::new();
        check("allow-listed is auto", p.classify("cat README.md") == twig::policy::Tier::Auto, "");
        check("unknown is audit", p.classify("make deploy") == twig::policy::Tier::Audit, "");
        check("destructive is deny", p.classify("rm -rf /") == twig::policy::Tier::Deny, "");
        p.classify("cat one.txt");
        let (_, before, _) = p.stats();
        p.classify("cat two.txt");
        let (hits, after, _) = p.stats();
        check("same shape costs no new miss", before == after, format!("hits={hits}"));
    }

    println!("one round-trip per turn");
    {
        let dir = tempdir("rtt");
        std::fs::write(dir.join("note.txt"), "hello\n").unwrap();
        let reply = StubLlm::with_tools(
            "",
            vec![
                call("a", "bash", json!({"command": "cat note.txt"})),
                call("b", "bash", json!({"command": "ls"})),
                call("c", "read", json!({"path": "note.txt"})),
                call("d", "bash", json!({"command": "rm -rf /"})),
                call("e", "bash", json!({"command": "make deploy"})),
            ],
        );
        let llm = StubLlm::new(vec![reply, StubLlm::text("done")]);
        let runs = tempdir("runs");
        let mut daemon = Daemon::new(&llm, &dir, &runs, "rtt", &HarnessConfig::default()).unwrap();
        let mut events = Vec::new();
        let out = daemon
            .dispatch("turn", &json!({"input": "look around"}), &mut |e: Value| events.push(e))
            .unwrap();
        // dispatch loops until the model stops asking for tools, so four tool
        // calls inside turn 1 must cost exactly one call, and turn 2 one more.
        check("two turns, two model calls (4 tools in turn 1)", llm.calls() == 2, llm.calls());
        check("turn finished", out["done"] == json!(true), &out);
        let planned = events.iter().find(|e| e["ev"] == "tool.planned").unwrap();
        check("deny tier assigned", planned["risk"]["d"] == json!("deny"), &planned["risk"]);
        let drained = daemon
            .dispatch("audit.drain", &json!({}), &mut |_| {})
            .unwrap()["rows"]
            .clone();
        let ids: Vec<&str> = drained
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        check("only the audit tier is recorded", ids == vec!["e"], format!("{ids:?}"));
        let pending = daemon.dispatch("approval.list", &json!({}), &mut |_| {}).unwrap();
        check("denied call waits for a human", pending["pending"].as_array().unwrap().len() == 1,
              &pending["pending"][0]["id"]);
        let refused = daemon
            .dispatch("approval.resolve", &json!({"id": "d", "allow": false}), &mut |_| {})
            .unwrap();
        check("refusal is recorded, nothing runs", refused["allow"] == json!(false), &refused);
        check("queue drained", daemon.dispatch("approval.list", &json!({}), &mut |_| {})
              .unwrap()["pending"].as_array().unwrap().is_empty(), "");
    }

    println!("verifier wired into the loop");
    {
        let dir = tempdir("verify");
        let runs = tempdir("runs");
        let journal = dir.join("journal.jsonl");
        twig::verify::record(
            &journal,
            &[twig::verify::Case {
                input: json!({"a": 1}),
                output: Some(json!({"a": 1})),
                error: None,
            }],
        )
        .unwrap();

        // A faithful candidate passes and the turn is accepted as verified.
        let llm = StubLlm::new(vec![StubLlm::text("finished")]);
        let mut daemon = Daemon::new(&llm, &dir, &runs, "pass", &HarnessConfig::default()).unwrap();
        let out = daemon
            .dispatch(
                "turn",
                &json!({"input": "do it", "journal": journal,
                        "candidate": {"program": "cat", "args": []}}),
                &mut |_| {},
            )
            .unwrap();
        check("faithful candidate accepted", out["verified"] == json!(true), &out);

        // A wrong candidate is fed back instead of being believed.
        let bad_journal = dir.join("bad.jsonl");
        twig::verify::record(
            &bad_journal,
            &[twig::verify::Case {
                input: json!({"a": 1}),
                output: Some(json!({"a": 2})),
                error: None,
            }],
        )
        .unwrap();
        let llm = StubLlm::new(vec![
            StubLlm::text("finished"),
            StubLlm::text("retry 1"),
            StubLlm::text("retry 2"),
        ]);
        let mut daemon = Daemon::new(&llm, &dir, &runs, "fail", &HarnessConfig::default()).unwrap();
        let out = daemon
            .dispatch(
                "turn",
                &json!({"input": "do it", "journal": bad_journal,
                        "candidate": {"program": "cat", "args": []}}),
                &mut |_| {},
            )
            .unwrap();
        check("mismatch is not accepted", out["reason"] == json!("verify_failed"), &out);
        let seen = llm.seen();
        check(
            "mismatch fed back to the model",
            seen.iter().any(|m| m.contains("Fix the code, not the journal")),
            format!("{} messages", seen.len()),
        );
        check("journal intact", Record::verify_chain(daemon.record_path()).is_ok(), "");
    }

    println!("playbooks");
    {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("playbooks");
        let books = Playbook::load_dir(&dir);
        check("three playbooks load", books.len() == 3, books.len());
        let hit = route("there is a crash on idle", &books);
        check("goal routes to bug-fix", hit.map(|b| b.name.as_str()) == Some("bug-fix"),
              hit.map(|b| b.name.clone()).unwrap_or_default());
        let miss = route("add a settings page", &books);
        check("unknown goal does not guess", miss.map(|b| b.name.as_str()) == Some("feature"),
              miss.map(|b| b.name.clone()).unwrap_or_default());
    }

    println!("decisions trail");
    {
        let dir = tempdir("dec");
        let log = Decisions::open(dir.join("decisions.tsv")).unwrap();
        log.log("narrow slice", "batch everything", "one commit per accepted slice").unwrap();
        check("row written", log.rows().unwrap().len() == 1, "");
    }

    println!("\nall checks passed");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");

    match cmd {
        "selfcheck" => selfcheck(),

        "serve" => {
            let worktree = args.get(1).cloned().unwrap_or_else(|| ".".to_string());
            let runs = args.get(2).cloned().unwrap_or_else(|| "runs".to_string());
            let provider = Provider::new();
            let (cfg, source) = match HarnessConfig::resolve() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("twig: config: {e}");
                    std::process::exit(1);
                }
            };
            let sid = blake3::hash(
                format!("{}-{}", std::process::id(), now_nanos()).as_bytes(),
            )
            .to_hex()
            .to_string();
            let mut daemon = match Daemon::new(&provider, &worktree, &runs, &sid[..12], &cfg) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("twig: {e}");
                    std::process::exit(1);
                }
            };
            eprintln!("twig: base={} sid={} config={source}", provider.base(), daemon.sid());
            let stdin = std::io::stdin();
            if let Err(e) = daemon.serve(BufReader::new(stdin.lock()), std::io::stdout()) {
                eprintln!("twig: {e}");
                std::process::exit(1);
            }
        }

        "eval" => {
            let root = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| "evals".into()));
            let reps: u32 = args.get(2).and_then(|r| r.parse().ok()).unwrap_or(1);
            let only = args.get(3).map(String::as_str);
            let tasks = twig::eval::load_tasks(&root, only);
            if tasks.is_empty() {
                eprintln!("twig: no tasks under {}", root.display());
                std::process::exit(1);
            }
            let (cfg, source) = HarnessConfig::resolve().unwrap_or_else(|e| {
                eprintln!("twig: config: {e}");
                std::process::exit(1)
            });
            let scratch = tempdir("eval");
            let cfg_path = scratch.join("harness.json");
            cfg.save(&cfg_path).expect("config");
            let twig = std::env::current_exe().expect("current exe");
            let spec = twig::eval::Spec { twig: &twig, config: &cfg_path, scratch: &scratch, wall: wall() };
            eprintln!("twig eval: {} task(s) x {reps} rep(s), workers={}, config={source}, scratch={}",
                      tasks.len(), workers(), scratch.display());
            let progress = |r: &Value| eprintln!("  {:<18} r{} {} calls={} {}s {}",
                r["task"].as_str().unwrap_or("?"), r["rep"],
                if r["pass"].is_null() { "UNGRADED" } else if r["pass"] == json!(true) { "PASS" } else { "FAIL" },
                r["model_calls"], r["secs"], r["outcome"].as_str().unwrap_or(""));
            let rows = twig::eval::run_all(&tasks, reps, workers(), &spec, &progress);
            let body: String = rows.iter().map(|r| format!("{r}\n")).collect();
            std::fs::write(scratch.join("results.jsonl"), body).ok();
            println!("{}", twig::eval::summarize(&rows));
            println!("results: {}", scratch.join("results.jsonl").display());
        }

        "improve" => {
            let root = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| "evals".into()));
            let rounds: u32 = args.get(2).and_then(|r| r.parse().ok()).unwrap_or(2);
            let (start, source) = HarnessConfig::resolve().unwrap_or_else(|e| {
                eprintln!("twig: config: {e}");
                std::process::exit(1)
            });
            let provider = Provider::new();
            let twig = std::env::current_exe().expect("current exe");
            let scratch = tempdir("improve");
            let out = twig::config::project_config_path();
            let report = out.with_file_name("improve-report.md");
            let opt = twig::improve::Options {
                evals: &root, twig: &twig, scratch: &scratch, out: &out, report: &report,
                reps: 2, workers: workers(), rounds, k: 2, wall: wall(),
            };
            eprintln!("twig improve: start={source} rounds={rounds} scratch={}", scratch.display());
            let log = |m: &str| eprintln!("{m}");
            match twig::improve::run(&provider, &start, &opt, &log) {
                Ok(cfg) => {
                    let diff = start.diff(&cfg);
                    println!("{}", if diff.is_empty() { "no change kept".to_string() } else { diff.join("\n") });
                    println!("report: {}", report.display());
                }
                Err(e) => {
                    eprintln!("twig: improve: {e}");
                    std::process::exit(1);
                }
            }
        }

        "record" => {
            let path = args.get(1).cloned().unwrap_or_else(|| "runs".to_string());
            match Record::verify_chain(Path::new(&path)) {
                Ok(entries) => println!("{}", json!({"ok": true, "entries": entries})),
                Err((index, why)) => {
                    println!("{}", json!({"ok": false, "bad_index": index, "why": why}));
                    std::process::exit(2);
                }
            }
        }

        "classify" => {
            let path = args.get(1).cloned().unwrap_or_else(|| "runs".to_string());
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let mut by_sid: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                if let Ok(row) = serde_json::from_str::<Value>(line) {
                    let sid = row.get("sid").and_then(Value::as_str).unwrap_or("?").to_string();
                    by_sid.entry(sid).or_default().push(row);
                }
            }
            let breakdown = classify::summarize(by_sid.into_values());
            println!(
                "{}",
                json!({
                    "process": breakdown.process,
                    "content": breakdown.content,
                    "process_share": (breakdown.process_share() * 100.0).round() / 100.0,
                    "lever": breakdown.lever()
                })
            );
        }

        "playbooks" => {
            let goal = args.get(1).cloned().unwrap_or_default();
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("playbooks");
            let books = Playbook::load_dir(&dir);
            match route(&goal, &books) {
                Some(book) => println!(
                    "{}",
                    json!({"playbook": book.name, "steps": book.steps})
                ),
                None => println!(
                    "{}",
                    json!({"playbook": Value::Null,
                           "why": "no playbook matched; ask instead of guessing"})
                ),
            }
        }

        "decisions" => {
            let path = args.get(1).cloned().unwrap_or_else(|| "decisions.tsv".to_string());
            match Decisions::open(&path) {
                Ok(log) => {
                    for row in log.rows().unwrap_or_default() {
                        println!("{row}");
                    }
                }
                Err(e) => eprintln!("twig: {e}"),
            }
        }

        _ => {
            println!(
                "twig 0.2.0 — a thin agent harness\n\n\
                 usage:\n  \
                 twig serve [worktree] [runs]   JSON-RPC over stdio\n  \
                 twig selfcheck                 offline checks (no network)\n  \
                 twig record <file.jsonl>       verify the hash chain\n  \
                 twig classify <file.jsonl>     process vs content failures\n  \
                 twig playbooks <goal>          route a goal to a playbook\n  \
                 twig decisions [file.tsv]      read the decision trail\n  \
                 twig eval [dir] [reps] [only]  scoring set: fresh repo, external judge\n  \
                 twig improve [dir] [rounds]    tune harness.json against the scoring set\n\n\
                 env: TWIG_BASE_URL (default http://127.0.0.1:3425/v1), TWIG_API_KEY, TWIG_ROLES,\n       \
                 TWIG_CONFIG, TWIG_WORKERS (4), TWIG_WALL_SECS (300)"
            );
        }
    }
}

fn now_nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn workers() -> usize {
    std::env::var("TWIG_WORKERS").ok().and_then(|v| v.parse().ok()).unwrap_or(4)
}

/// Wall-clock limit per eval run. A run that outlives it is killed with its
/// whole process group and scored as a process failure.
fn wall() -> std::time::Duration {
    let secs = std::env::var("TWIG_WALL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    std::time::Duration::from_secs(secs)
}
