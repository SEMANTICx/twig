//! `twig eval`: the scoring set. Write the exam before the code.
//!
//! Each task is `task.json` ({goal, optional check}), an optional `check.sh`
//! that stays outside the agent's copy, and a `repo/`. Every run copies the repo
//! fresh (including `.git` and uncommitted work), starts `twig serve` in a child
//! process, gives it only the goal, then runs the check in the copy.
//!
//! The check is the judge. The agent's claim is never consulted. Each run is a
//! separate process with its own process group, because the only reliable way
//! to stop a hung run (and every server it started) is to kill the group.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::classify::{classify_rows, FailureKind};

pub struct Task {
    pub name: String,
    pub dir: PathBuf,
    pub goal: String,
    pub check: String,
    /// Answers to this task that must not leak into a tuned prompt, e.g. the
    /// env var a task's README documents. The improver may fix the harness,
    /// not memorise the exam.
    pub forbid: Vec<String>,
    pub trajectory: Vec<crate::trajectory::Rule>,
}

pub fn load_tasks(root: &Path, only: Option<&str>) -> Vec<Task> {
    // Absolute, because the judge runs the check from inside the worktree.
    let Ok(root) = root.canonicalize() else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .map(|e| e.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    dirs.sort();
    let mut tasks = Vec::new();
    for dir in dirs {
        let name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if only.is_some_and(|o| !name.contains(o)) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(dir.join("task.json")) else { continue };
        let Ok(spec) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(goal) = spec["goal"].as_str() else { continue };
        let script = dir.join("check.sh");
        let check = if script.exists() {
            format!("sh '{}'", script.display().to_string().replace('\'', r"'\''"))
        } else if let Some(c) = spec["check"].as_str() {
            c.to_string()
        } else {
            continue;
        };
        let forbid = spec["forbid"].as_array().into_iter().flatten()
            .filter_map(Value::as_str).map(str::to_string).collect();
        let trajectory = match spec.get("trajectory") {
            Some(rules) => serde_json::from_value(rules.clone()).unwrap_or_else(|e| {
                panic!("{}: invalid trajectory contract: {e}", dir.display())
            }),
            None => Vec::new(),
        };
        tasks.push(Task { name, goal: goal.into(), check, forbid, trajectory, dir });
    }
    tasks
}

fn copy_all(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_all(&src, &dst)?;
        } else if kind.is_file() {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Kill a process group. The group outlives its leader while any member lives,
/// so its id cannot be reused by an unrelated process in the meantime.
fn kill_group(pgid: u32) {
    crate::process::kill_tree(pgid);
}

/// Run the check in its own process group and kill the group afterwards. A
/// check that starts a server and forgets to stop it must not leave it running,
/// and must not hang the judge: `.output()` waits for every holder of the pipe.
pub fn judge(worktree: &Path, check: &str) -> (bool, String) {
    let child = Command::new("sh").arg("-c").arg(check).current_dir(worktree)
        .process_group(0).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (false, format!("judge spawn: {e}")),
    };
    let pgid = child.id();
    let drain = |r: Option<Box<dyn std::io::Read + Send>>| std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = r {
            std::io::Read::read_to_end(&mut r, &mut buf).ok();
        }
        buf
    });
    let out = drain(child.stdout.take().map(|s| Box::new(s) as _));
    let err = drain(child.stderr.take().map(|s| Box::new(s) as _));
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => break None,
        }
    };
    kill_group(pgid); // reaps leaked servers, which also closes the pipes they held
    child.wait().ok();
    let mut text = String::from_utf8_lossy(&out.join().unwrap_or_default()).to_string();
    text.push_str(&String::from_utf8_lossy(&err.join().unwrap_or_default()));
    let tail: Vec<char> = text.chars().rev().take(300).collect();
    let tail = tail.into_iter().rev().collect::<String>().trim().to_string();
    match status {
        Some(s) => (s.success(), tail),
        None => (false, format!("check timed out after 120s\n{tail}")),
    }
}

pub struct Spec<'a> {
    pub twig: &'a Path,
    pub config: &'a Path,
    pub scratch: &'a Path,
    pub wall: Duration,
}

/// What the agent did, compact: the improver's evidence and a human's summary.
fn steps(rows: &[Value]) -> (Vec<Value>, u64) {
    let mut args = std::collections::HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut timeouts = 0;
    for row in rows {
        match row["kind"].as_str() {
            Some("model.reply") => {
                for c in row["tool_calls"].as_array().into_iter().flatten() {
                    args.insert(c["id"].as_str().unwrap_or("").to_string(),
                                (c["name"].clone(), c["args"].clone()));
                }
            }
            Some("tool.result") => {
                let id = row["id"].as_str().unwrap_or("").to_string();
                let output = row["output"].as_str().unwrap_or("");
                timeouts += u64::from(output.starts_with("timeout:"));
                let head: String = output.chars().take(240).collect();
                out.push(json!({"tool": row["tool"], "args": row["args"], "tier": row["tier"],
                                "bytes": output.len(), "output_head": head, "exit_code": row["exit_code"]}));
                seen.insert(id);
            }
            Some("guard.tripped") => out.push(json!({"guard": row["level"]})),
            _ => {}
        }
    }
    for (id, (name, a)) in args {
        if !seen.contains(&id) {
            out.push(json!({"tool": name, "args": a, "output_head": "(never returned: run was killed)"}));
        }
    }
    (out, timeouts)
}

pub fn run_one(task: &Task, rep: u32, spec: &Spec<'_>) -> Value {
    let id = format!("{}-r{rep}", task.name);
    let worktree = spec.scratch.join(&id);
    let runs = spec.scratch.join("runs").join(&id);
    std::fs::remove_dir_all(&worktree).ok();
    std::fs::remove_dir_all(&runs).ok();
    if let Err(e) = copy_all(&task.dir.join("repo"), &worktree).and_then(|_| std::fs::create_dir_all(&runs)) {
        return json!({"task": task.name, "rep": rep, "pass": false, "outcome": format!("setup: {e}")});
    }
    let stderr = std::fs::File::create(runs.join("stderr.log")).map(Stdio::from).unwrap_or(Stdio::null());
    let started = Instant::now();
    let spawned = Command::new(spec.twig)
        .arg("serve").arg(&worktree).arg(&runs)
        .env("TWIG_CONFIG", spec.config)
        .process_group(0) // one group per run: killing it takes every server it started
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(stderr)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return json!({"task": task.name, "rep": rep, "pass": false, "outcome": format!("spawn: {e}")}),
    };
    let pgid = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        let request = json!({"id": 1, "method": "turn", "params": {"input": task.goal}});
        writeln!(stdin, "{request}").ok();
    } // dropped: EOF after one request, so serve exits when the turn ends
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        BufReader::new(stdout).lines().map_while(Result::ok).collect::<Vec<_>>()
    });
    let mut killed = false;
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        if started.elapsed() >= spec.wall {
            killed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    kill_group(pgid); // also reaps anything a finished run left running
    child.wait().ok();
    let lines = reader.join().unwrap_or_default();
    let secs = started.elapsed().as_secs_f64();
    let result = lines.iter().filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("ev").is_none());
    let rows: Vec<Value> = std::fs::read_dir(&runs).into_iter().flatten().flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .flat_map(|p| std::fs::read_to_string(p).unwrap_or_default().lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect::<Vec<_>>())
        .collect();
    let (trace, tool_timeouts) = steps(&rows);
    let (outcome_pass, judge_tail) = judge(&worktree, &task.check);
    let trajectory = crate::trajectory::grade(&rows, &task.trajectory);
    let pass = if killed || !outcome_pass { json!(false) } else { trajectory["pass"].clone() };
    let usage = crate::telemetry::summarize(&rows);
    let papercuts: Vec<_> = rows.iter().filter(|r| r["kind"] == "papercut").map(|r| r["report"].clone()).collect();

    let reason = result.as_ref().and_then(|r| r["result"]["reason"].as_str().map(str::to_string));
    let outcome = if killed {
        format!("killed after {}s wall", spec.wall.as_secs())
    } else if let Some(err) = result.as_ref().and_then(|r| r["error"].as_str()) {
        format!("error: {}", err.chars().take(160).collect::<String>())
    } else {
        reason.clone().unwrap_or_else(|| "no reply".into())
    };
    // First firing signal decides; a run that never finished is a process failure.
    let failure_kind = if pass != false {
        Value::Null
    } else if killed || reason.as_deref() != Some("final") {
        json!("process")
    } else if outcome_pass && trajectory["pass"] == false {
        json!("trajectory")
    } else {
        match classify_rows(&rows) {
            Some(FailureKind::Process) => json!("process"),
            _ => json!("content"),
        }
    };
    let mut tiers = json!({"auto": 0, "audit": 0, "deny": 0});
    for row in &rows {
        let tier = match row["kind"].as_str() {
            Some("tool.result") => row["tier"].as_str().unwrap_or("auto"),
            Some("approval.requested") => "deny",
            _ => continue,
        };
        tiers[tier] = json!(tiers[tier].as_u64().unwrap_or(0) + 1);
    }
    let model_calls = rows.iter().filter(|r| r["kind"] == "model.reply").count() as u64 + u64::from(killed);
    json!({
        "task": task.name, "rep": rep, "pass": pass, "outcome": outcome,
        "outcome_pass": outcome_pass, "trajectory": trajectory,
        "usage": usage, "papercuts": papercuts,
        "failure_kind": failure_kind, "killed": killed,
        "model_calls": model_calls, "tool_calls": trace.iter().filter(|s| s.get("tool").is_some()).count(),
        "tool_timeouts": tool_timeouts, "tiers": tiers,
        "secs": (secs * 10.0).round() / 10.0,
        "agent_said": result.as_ref().and_then(|r| r["result"]["text"].as_str())
            .map(|t| t.chars().take(400).collect::<String>()),
        "steps": trace,
        "judge_tail": judge_tail, // for humans only; never shown to the improver
    })
}

/// Run every task `reps` times on `workers` threads. Two runs of the same task
/// never overlap: a task that starts a server would otherwise race its sibling
/// for the port and measure the collision instead of the harness.
pub fn run_all(tasks: &[Task], reps: u32, workers: usize, spec: &Spec<'_>,
               progress: &(dyn Fn(&Value) + Sync)) -> Vec<Value> {
    let mut jobs: Vec<(usize, u32)> = (1..=reps)
        .flat_map(|r| (0..tasks.len()).map(move |t| (t, r)))
        .collect();
    jobs.reverse(); // rposition below then takes the earliest runnable job
    let queue = Mutex::new((jobs, std::collections::HashSet::<usize>::new()));
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..workers.max(1) {
            s.spawn(|| loop {
                let next = {
                    let mut q = queue.lock().expect("queue");
                    if q.0.is_empty() {
                        break;
                    }
                    let busy = q.1.clone();
                    let at = q.0.iter().rposition(|(t, _)| !busy.contains(t));
                    at.map(|i| {
                        let job = q.0.remove(i);
                        q.1.insert(job.0);
                        job
                    })
                };
                let Some((t, rep)) = next else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                let row = run_one(&tasks[t], rep, spec);
                progress(&row);
                results.lock().expect("results").push(row);
                queue.lock().expect("queue").1.remove(&t);
            });
        }
    });
    let mut rows = results.into_inner().expect("results");
    rows.sort_by(|a, b| (a["task"].as_str(), a["rep"].as_u64()).cmp(&(b["task"].as_str(), b["rep"].as_u64())));
    rows
}

pub fn summarize(rows: &[Value]) -> String {
    use std::collections::BTreeMap;
    let mut by: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for row in rows {
        by.entry(row["task"].as_str().unwrap_or("?")).or_default().push(row);
    }
    let mut out = String::from(
        "| task | pass | calls | secs | tool timeouts | killed | failure |\n|---|---|---|---|---|---|---|\n");
    let (mut passed, mut secs) = (0usize, 0.0f64);
    for (task, rs) in &by {
        let n = rs.len() as f64;
        let p = rs.iter().filter(|r| r["pass"] == json!(true)).count();
        let mean = |k: &str| rs.iter().map(|r| r[k].as_f64().unwrap_or(0.0)).sum::<f64>() / n;
        let sum = |k: &str| rs.iter().map(|r| r[k].as_u64().unwrap_or(u64::from(r[k] == json!(true)))).sum::<u64>();
        let kinds: Vec<&str> = rs.iter().filter_map(|r| r["failure_kind"].as_str()).collect();
        out.push_str(&format!("| {task} | {p}/{} | {:.1} | {:.1} | {} | {} | {} |\n",
            rs.len(), mean("model_calls"), mean("secs"), sum("tool_timeouts"), sum("killed"),
            if kinds.is_empty() { "-".to_string() } else { kinds.join(", ") }));
        passed += p;
        secs += rs.iter().map(|r| r["secs"].as_f64().unwrap_or(0.0)).sum::<f64>();
    }
    out.push_str(&format!("\n**{passed}/{} passed**, {:.0}s of agent time\n", rows.len(), secs));
    let ungraded = rows.iter().filter(|r| r["pass"].is_null()).count();
    out.push_str(&format!("\nUngraded (trajectory evidence unknown): {ungraded}. These are not proven failures.\n"));
    let total = |field: &str| {
        let known = rows.iter().filter(|r| r["usage"][field].is_u64()).count();
        if known == rows.len() && known > 0 {
            rows.iter().filter_map(|r| r["usage"][field].as_u64()).sum::<u64>().to_string()
        } else { format!("unknown ({known}/{} runs reported)", rows.len()) }
    };
    let (inputs, outputs, cached) = (total("input_tokens"), total("output_tokens"), total("cached_input_tokens"));
    let complete = rows.iter().filter(|r| r["usage"]["complete"] == true).count();
    out.push_str(&format!("\nReported input/output/cached tokens: {inputs}/{outputs}/{cached}. Complete usage: {complete}/{} runs. No dollar estimate.\n", rows.len()));
    let mut cuts = std::collections::BTreeMap::<String, usize>::new();
    for p in rows.iter().flat_map(|r| r["papercuts"].as_array().into_iter().flatten()) {
        *cuts.entry(p["component"].as_str().unwrap_or("unknown").to_string()).or_default() += 1;
    }
    if !cuts.is_empty() { out.push_str(&format!("\nPapercuts by component: {cuts:?}\n")); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(pid: &str) -> bool {
        // A killed process can linger as a zombie until it is reaped; that is dead.
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .is_ok_and(|s| !s.lines().any(|l| l.starts_with("State:") && l.contains('Z')))
    }

    #[test]
    fn check_scripts_resolve_from_inside_the_worktree() {
        // load_tasks was given a relative root and produced `sh evals/x/check.sh`,
        // which the judge, running inside the worktree, could not find.
        let tasks = load_tasks(Path::new("evals"), Some("10-run"));
        assert_eq!(tasks.len(), 1);
        let path = tasks[0].check.trim_start_matches("sh '").trim_end_matches('\'');
        assert!(Path::new(path).is_absolute() && Path::new(path).exists(), "{}", tasks[0].check);
    }

    #[test]
    fn judge_reaps_what_a_check_leaves_running() {
        let dir = std::env::temp_dir().join(format!("twig-judge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Holds stdout open and never exits: before the fix this hung the judge.
        let (ok, _) = judge(&dir, "sleep 300 & echo $! > leaked.pid; echo done");
        assert!(ok);
        let pid = std::fs::read_to_string(dir.join("leaked.pid")).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!alive(pid.trim()), "judge left pid {pid} running");
    }
    #[test]
    fn missing_output_usage_stays_unknown_in_the_human_report() {
        let row = json!({"task":"a","pass":true,"usage":{"input_tokens":10,"output_tokens":null}});
        let summary = summarize(&[row]);
        assert!(summary.contains("10/unknown (0/1 runs reported)/unknown"), "{summary}");
    }
}
