//! Isolated evaluation runner, external judge, and summary formatter.

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::classify::{classify_rows, FailureKind};
use super::dataset::{copy_all, Task};
use super::trajectory;
use crate::config::paths;
use crate::exec::{isolation, run_check, run_process, Check};
use crate::llm::{summarize_usage, Llm, Provider};

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

/// Run the check in its own bounded process group and reap all descendants afterwards.
pub fn judge(worktree: &Path, check: &str) -> (bool, String) {
    let spec = Check {
        command: check.to_owned(),
        timeout_ms: 120_000,
        max_output_bytes: 1_048_576,
    };
    let result = match run_check(worktree, &spec) {
        Ok(r) => r,
        Err(e) => return (false, format!("judge spawn: {e}")),
    };
    let tail: Vec<char> = result.output.chars().rev().take(300).collect();
    let tail = tail
        .into_iter()
        .rev()
        .collect::<String>()
        .trim()
        .to_string();
    if result.timed_out {
        (false, format!("check timed out after 120s\n{tail}"))
    } else {
        (result.passed, tail)
    }
}

fn resolve_eval_env() -> Vec<(&'static str, String)> {
    let mut envs = Vec::new();
    for name in [
        "TWIG_BASE_URL",
        "TWIG_API_KEY",
        "TWIG_ROLES",
        "TWIG_MODEL",
        "TWIG_EFFORT",
    ] {
        if let Ok(value) = std::env::var(name) {
            envs.push((name, value));
        }
    }
    if std::env::var_os("TWIG_ROLES").is_none() && std::env::var_os("TWIG_MODEL").is_none() {
        let provider = Provider::new();
        if let Ok(roles_json) = serde_json::to_string(&provider.roles().0) {
            envs.push(("TWIG_ROLES", roles_json));
        }
    }
    if std::env::var_os("TWIG_EFFORT").is_none() {
        let effort_file = paths::state_dir().join("effort.txt");
        if let Ok(raw) = std::fs::read_to_string(&effort_file) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                envs.push(("TWIG_EFFORT", trimmed.to_owned()));
            }
        }
    }
    envs
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
    let started = Instant::now();
    let env_owned = resolve_eval_env();
    let env_refs: Vec<(&str, &str)> = env_owned
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect();
    let mut command =
        match isolation::eval_command(spec.twig, spec.config, &worktree, &runs, &env_refs) {
            Ok(command) => command,
            Err(e) => {
                return json!({"task": task.name, "rep": rep, "pass": null, "outcome": e, "isolation": false})
            }
        };
    let request = format!(
        "{}\n",
        json!({"id": 1, "method": "turn", "params": {"input": task.goal}})
    );
    let capture = match run_process(&mut command, request.as_bytes(), spec.wall, 4 * 1024 * 1024) {
        Ok(c) => c,
        Err(e) => {
            return json!({"task": task.name, "rep": rep, "pass": null, "outcome": format!("sandbox spawn: {e}"), "isolation": false})
        }
    };
    let _ = std::fs::write(runs.join("stderr.log"), &capture.stderr);
    let killed = capture.timed_out;
    let stdout_text = String::from_utf8_lossy(&capture.stdout);
    let secs = started.elapsed().as_secs_f64();
    let result = stdout_text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("ev").is_none());
    if result.is_none() && !killed {
        let error = String::from_utf8_lossy(&capture.stderr);
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
