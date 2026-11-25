//! Isolated evaluation runner, external judge, and summary formatter.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::classify::{classify_rows, FailureKind};
use super::dataset::{copy_all, Task};
use super::trajectory;
use crate::exec::{isolation, kill_tree};
use crate::llm::summarize_usage;

fn initialize_repo(workspace: &Path, task: &Task) -> Result<(), String> {
    if workspace.join(".git").is_dir() {
        return Ok(());
    }
    let git = |args: &[&str]| -> Result<(), String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(workspace)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .map_err(|e| format!("git fixture setup: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "git fixture setup: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    };
    git(&["init", "--quiet"])?;
    let metadata: Value = serde_json::from_slice(
        &std::fs::read(task.dir.join("task.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let mut args = vec!["add", "--all", "--", "."];
    let exclusions: Vec<String> = metadata["wip"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|path| format!(":(exclude,literal){path}"))
        .collect();
    args.extend(exclusions.iter().map(String::as_str));
    git(&args)?;
    git(&[
        "-c",
        "user.name=Twig evaluation",
        "-c",
        "user.email=eval@localhost",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "Evaluation snapshot baseline",
    ])
}

/// Run the check in its own process group and kill the group afterwards.
pub fn judge(worktree: &Path, check: &str) -> (bool, String) {
    let child = Command::new("sh")
        .arg("-c")
        .arg(check)
        .current_dir(worktree)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (false, format!("judge spawn: {e}")),
    };
    let pgid = child.id();
    let drain = |r: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                std::io::Read::read_to_end(&mut r, &mut buf).ok();
            }
            buf
        })
    };
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
    kill_tree(pgid);
    child.wait().ok();
    let mut text = String::from_utf8_lossy(&out.join().unwrap_or_default()).to_string();
    text.push_str(&String::from_utf8_lossy(&err.join().unwrap_or_default()));
    let tail: Vec<char> = text.chars().rev().take(300).collect();
    let tail = tail
        .into_iter()
        .rev()
        .collect::<String>()
        .trim()
        .to_string();
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

fn steps(rows: &[Value]) -> (Vec<Value>, u64) {
    let mut args = std::collections::HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut timeouts = 0;
    for row in rows {
        match row["kind"].as_str() {
            Some("model.reply") => {
                for c in row["tool_calls"].as_array().into_iter().flatten() {
                    args.insert(
                        c["id"].as_str().unwrap_or("").to_string(),
                        (c["name"].clone(), c["args"].clone()),
                    );
                }
            }
            Some("tool.result") => {
                let id = row["id"].as_str().unwrap_or("").to_string();
                let output = row["output"].as_str().unwrap_or("");
                timeouts += u64::from(output.starts_with("timeout:"));
                let head: String = output.chars().take(240).collect();
                out.push(json!({
                    "tool": row["tool"],
                    "args": row["args"],
                    "tier": row["tier"],
                    "bytes": output.len(),
                    "output_head": head,
                    "exit_code": row["exit_code"]
                }));
                seen.insert(id);
            }
            Some("guard.tripped") => out.push(json!({"guard": row["level"]})),
            _ => {}
        }
    }
    for (id, (name, a)) in args {
        if !seen.contains(&id) {
            out.push(
                json!({"tool": name, "args": a, "output_head": "(never returned: run was killed)"}),
            );
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
    if let Err(e) =
        copy_all(&task.dir.join("repo"), &worktree).and_then(|_| std::fs::create_dir_all(&runs))
    {
        return json!({"task": task.name, "rep": rep, "pass": null, "outcome": format!("setup: {e}")});
    }
    if let Err(e) = initialize_repo(&worktree, task) {
        return json!({"task": task.name, "rep": rep, "pass": null, "outcome": e});
    }
    let stderr = std::fs::File::create(runs.join("stderr.log"))
        .map(Stdio::from)
        .unwrap_or(Stdio::null());
    let started = Instant::now();
    let mut command = match isolation::eval_command(spec.twig, spec.config, &worktree, &runs) {
        Ok(command) => command,
        Err(e) => {
            return json!({"task": task.name, "rep": rep, "pass": null, "outcome": e, "isolation": false})
        }
    };
    let spawned = command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            return json!({"task": task.name, "rep": rep, "pass": null, "outcome": format!("sandbox spawn: {e}"), "isolation": false})
        }
    };
    let pgid = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        let request = json!({"id": 1, "method": "turn", "params": {"input": task.goal}});
        writeln!(stdin, "{request}").ok();
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .collect::<Vec<_>>()
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
    kill_tree(pgid);
    child.wait().ok();
    let lines = reader.join().unwrap_or_default();
    let secs = started.elapsed().as_secs_f64();
    let result = lines
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("ev").is_none());
    if result.is_none() && !killed {
        let error = std::fs::read_to_string(runs.join("stderr.log")).unwrap_or_default();
        return json!({"task": task.name, "rep": rep, "pass": null, "outcome": format!("sandbox/agent exited without response: {error}"), "isolation": false});
    }
    let rows: Vec<Value> = std::fs::read_dir(&runs)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .flat_map(|p| {
            std::fs::read_to_string(p)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .collect::<Vec<_>>()
        })
        .collect();
    let (trace, tool_timeouts) = steps(&rows);
    let (outcome_pass, judge_tail) = judge(&worktree, &task.check);
    let traj = trajectory::grade(&rows, &task.trajectory);
    let pass = if killed || !outcome_pass {
        json!(false)
    } else {
        traj["pass"].clone()
    };
    let usage = summarize_usage(&rows);
    let papercuts: Vec<_> = rows
        .iter()
        .filter(|r| r["kind"] == "papercut")
        .map(|r| r["report"].clone())
        .collect();

    let reason = result
        .as_ref()
        .and_then(|r| r["result"]["reason"].as_str().map(str::to_string));
    let outcome = if killed {
        format!("killed after {}s wall", spec.wall.as_secs())
    } else if let Some(err) = result.as_ref().and_then(|r| r["error"].as_str()) {
        format!("error: {}", err.chars().take(160).collect::<String>())
    } else {
        reason.clone().unwrap_or_else(|| "no reply".into())
    };
    let failure_kind = if pass != false {
        Value::Null
    } else if killed || reason.as_deref() != Some("final") {
        json!("process")
    } else if outcome_pass && traj["pass"] == false {
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
    let model_calls =
        rows.iter().filter(|r| r["kind"] == "model.reply").count() as u64 + u64::from(killed);
    json!({
        "task": task.name,
        "rep": rep,
        "pass": pass,
        "outcome": outcome,
        "isolation": true,
        "outcome_pass": outcome_pass,
        "trajectory": traj,
        "usage": usage,
        "papercuts": papercuts,
        "failure_kind": failure_kind,
        "killed": killed,
        "model_calls": model_calls,
        "tool_calls": trace.iter().filter(|s| s.get("tool").is_some()).count(),
        "tool_timeouts": tool_timeouts,
        "tiers": tiers,
        "secs": (secs * 10.0).round() / 10.0,
        "agent_said": result.as_ref().and_then(|r| r["result"]["text"].as_str())
            .map(|t| t.chars().take(400).collect::<String>()),
        "steps": trace,
        "judge_tail": judge_tail,
    })
}

pub fn run_all(
    tasks: &[Task],
    reps: u32,
    workers: usize,
    spec: &Spec<'_>,
    progress: &(dyn Fn(&Value) + Sync),
) -> Vec<Value> {
    let mut jobs: Vec<(usize, u32)> = (1..=reps)
        .flat_map(|r| (0..tasks.len()).map(move |t| (t, r)))
        .collect();
    jobs.reverse();
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
    rows.sort_by(|a, b| {
        (a["task"].as_str(), a["rep"].as_u64()).cmp(&(b["task"].as_str(), b["rep"].as_u64()))
    });
    rows
}

pub fn summarize(rows: &[Value]) -> String {
    use std::collections::BTreeMap;
    let mut by: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for row in rows {
        by.entry(row["task"].as_str().unwrap_or("?"))
            .or_default()
            .push(row);
    }
    let mut out = String::from(
        "| task | pass | calls | secs | tool timeouts | killed | failure |\n|---|---|---|---|---|---|---|\n",
    );
    let (mut passed, mut secs) = (0usize, 0.0f64);
    for (task, rs) in &by {
        let n = rs.len() as f64;
        let p = rs.iter().filter(|r| r["pass"] == json!(true)).count();
        let mean = |k: &str| rs.iter().map(|r| r[k].as_f64().unwrap_or(0.0)).sum::<f64>() / n;
        let sum = |k: &str| {
            rs.iter()
                .map(|r| r[k].as_u64().unwrap_or(u64::from(r[k] == json!(true))))
                .sum::<u64>()
        };
        let kinds: Vec<&str> = rs
            .iter()
            .filter_map(|r| r["failure_kind"].as_str())
            .collect();
        out.push_str(&format!(
            "| {task} | {p}/{} | {:.1} | {:.1} | {} | {} | {} |\n",
            rs.len(),
            mean("model_calls"),
            mean("secs"),
            sum("tool_timeouts"),
            sum("killed"),
            if kinds.is_empty() {
                "-".to_string()
            } else {
                kinds.join(", ")
            }
        ));
        passed += p;
        secs += rs
            .iter()
            .map(|r| r["secs"].as_f64().unwrap_or(0.0))
            .sum::<f64>();
    }
    out.push_str(&format!(
        "\n**{passed}/{} passed**, {:.0}s of agent time\n",
        rows.len(),
        secs
    ));
    let ungraded = rows.iter().filter(|r| r["pass"].is_null()).count();
    out.push_str(&format!(
        "\nUngraded (trajectory evidence unknown): {ungraded}. These are not proven failures.\n"
    ));
    let total = |field: &str| {
        let known = rows.iter().filter(|r| r["usage"][field].is_u64()).count();
        if known == rows.len() && known > 0 {
            rows.iter()
                .filter_map(|r| r["usage"][field].as_u64())
                .sum::<u64>()
                .to_string()
        } else {
            format!("unknown ({known}/{} runs reported)", rows.len())
        }
    };
    let (inputs, outputs, cached) = (
        total("input_tokens"),
        total("output_tokens"),
        total("cached_input_tokens"),
    );
    let complete = rows
        .iter()
        .filter(|r| r["usage"]["complete"] == true)
        .count();
    out.push_str(&format!(
        "\nReported input/output/cached tokens: {inputs}/{outputs}/{cached}. Complete usage: {complete}/{} runs. No dollar estimate.\n",
        rows.len()
    ));
    let mut cuts = std::collections::BTreeMap::<String, usize>::new();
    for p in rows
        .iter()
        .flat_map(|r| r["papercuts"].as_array().into_iter().flatten())
    {
        *cuts
            .entry(p["component"].as_str().unwrap_or("unknown").to_string())
            .or_default() += 1;
    }
    if !cuts.is_empty() {
        out.push_str(&format!("\nPapercuts by component: {cuts:?}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::dataset::load_tasks;

    fn alive(pid: &str) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|s| {
            !s.lines()
                .any(|l| l.starts_with("State:") && l.contains('Z'))
        })
    }

    #[test]
    fn judge_reaps_what_a_check_leaves_running() {
        let dir = std::env::temp_dir().join(format!("twig-judge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        assert!(
            summary.contains("10/unknown (0/1 runs reported)/unknown"),
            "{summary}"
        );
    }

    #[test]
    fn fixture_git_baseline_preserves_declared_untracked_work() {
        let task = load_tasks(Path::new("evals"), Some("05-keep-wip")).remove(0);
        let workspace = std::env::temp_dir().join(format!("twig-wip-{}", std::process::id()));
        copy_all(&task.dir.join("repo"), &workspace).unwrap();
        initialize_repo(&workspace, &task).unwrap();
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("?? notes.txt"));
        let tracked = Command::new("git")
            .args(["ls-files", "greet.py"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&tracked.stdout).trim(), "greet.py");
        std::fs::remove_dir_all(workspace).unwrap();
    }
}
