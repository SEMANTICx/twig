//! `twig improve`: run the scoring set, read the failures, let a model propose
//! config changes, keep a change only if it beats rerun variance.
//!
//! Only `HarnessConfig` is editable. A failure that needs a code change is
//! written to the report for a human; the loop never patches its own source,
//! its checks, or its scoring set.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::HarnessConfig;
use crate::decisions::Decisions;
use crate::eval::{run_all, summarize, Spec, Task};
use crate::provider::Llm;

#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    pub passes: usize,
    pub runs: usize,
    /// mean model calls per run
    pub calls: f64,
    /// mean wall seconds per run
    pub secs: f64,
    /// Available only when every model reply in every run reported usage.
    /// This is input + output tokens, not an estimate of money spent.
    pub tokens: Option<f64>,
    pub models: std::collections::BTreeSet<String>,
    pub per_task: BTreeMap<String, (usize, usize)>,
    pub axes: BTreeMap<String, (usize, usize)>,
    pub unknown: BTreeMap<String, usize>,
}

impl Score {
    pub fn of(rows: &[Value]) -> Self {
        let mut per_task = BTreeMap::new();
        for r in rows {
            let e = per_task.entry(r["task"].as_str().unwrap_or("?").to_string()).or_insert((0, 0));
            e.0 += usize::from(r["pass"] == json!(true));
            e.1 += 1;
        }
        let n = rows.len().max(1) as f64;
        let mean = |k: &str| rows.iter().map(|r| r[k].as_f64().unwrap_or(0.0)).sum::<f64>() / n;
        let mut axes = BTreeMap::new();
        let mut unknown = BTreeMap::new();
        for row in rows {
            let task = row["task"].as_str().unwrap_or("?");
            for (axis, value) in [("outcome", row.get("outcome_pass").unwrap_or(&row["pass"])),
                ("trajectory", row.pointer("/trajectory/pass").unwrap_or(&row["pass"]))] {
                let e = axes.entry(format!("{task}:{axis}")).or_insert((0, 0));
                if value.is_null() { *unknown.entry(format!("{task}:{axis}")).or_default() += 1; }
                e.0 += usize::from(*value == true);
                e.1 += 1;
            }
        }
        Self {
            passes: per_task.values().map(|v| v.0).sum(),
            runs: rows.len(),
            calls: mean("model_calls"),
            secs: mean("secs"),
            tokens: (!rows.is_empty() && rows.iter().all(|r| r["usage"]["complete"] == true))
                .then(|| rows.iter().map(|r| r["usage"]["input_tokens"].as_u64().unwrap_or(0)
                    + r["usage"]["output_tokens"].as_u64().unwrap_or(0)).sum::<u64>() as f64 / n),
            models: rows.iter().flat_map(|r| r["usage"]["models"].as_array().into_iter().flatten())
                .filter_map(Value::as_str).map(str::to_string).collect(),
            per_task,
            axes,
            unknown,
        }
    }
}

/// Runs that disagree with their sibling reps when nothing changed. A candidate
/// has to beat this, not merely differ from the baseline.
pub fn noise(rows: &[Value]) -> usize {
    let mut by: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for r in rows {
        let e = by.entry(r["task"].as_str().unwrap_or("?")).or_default();
        e.0 += usize::from(r["pass"] == json!(true));
        e.1 += 1;
    }
    by.values().map(|(p, n)| (*p).min(n - p)).sum()
}

/// The acceptance rule, in priority order:
///   1. no task that always passed may start always failing
///   2. more passes, by at least the noise margin
///   3. or the same passes, clearly cheaper, without getting worse on the other cost
pub fn accept(cand: &Score, best: &Score, margin: usize) -> Result<String, String> {
    if cand.runs != best.runs || cand.models != best.models || cand.per_task.keys().ne(best.per_task.keys()) {
        return Err("incomparable run sets or models".into());
    }
    if cand.unknown != best.unknown { return Err("incomparable trajectory evidence coverage".into()); }
    if cand.axes.keys().ne(best.axes.keys()) { return Err("incomparable quality axes".into()); }
    for (axis, (passes, count)) in &best.axes {
        let (cp, cn) = cand.axes[axis];
        if cn != *count || cp < *passes { return Err(format!("regression on {axis}")); }
    }
    for (task, (p, n)) in &best.per_task {
        let (cp, _) = cand.per_task.get(task).copied().unwrap_or((0, *n));
        if cp < *p {
            return Err(format!("regression: {task} went from {p}/{n} to {cp}/{n}"));
        }
    }
    if cand.passes >= best.passes + margin {
        return Ok(format!("passes {} -> {} (margin {margin})", best.passes, cand.passes));
    }
    if cand.passes == best.passes {
        if cand.tokens.is_some() != best.tokens.is_some() {
            return Err("token usage coverage differs; cannot establish a cost improvement".into());
        }
        if let (Some(c), Some(b)) = (cand.tokens, best.tokens) {
            if c <= b * 0.85 && cand.secs <= best.secs * 1.1 {
                return Ok(format!("same passes, mean tokens {b:.0} -> {c:.0} (not dollars)"));
            }
            if c > b * 1.1 { return Err("same passes but token consumption regressed >10%".into()); }
        }
        if cand.secs <= best.secs * 0.8 && cand.calls <= best.calls * 1.1 {
            return Ok(format!("same passes, mean secs {:.1} -> {:.1}", best.secs, cand.secs));
        }
        if cand.calls <= best.calls * 0.85 && cand.secs <= best.secs * 1.1 {
            return Ok(format!("same passes, mean calls {:.2} -> {:.2}", best.calls, cand.calls));
        }
    }
    Err(format!(
        "passes {} vs {} (needs +{margin}); mean secs {:.1} vs {:.1}; mean calls {:.2} vs {:.2}",
        cand.passes, best.passes, cand.secs, best.secs, cand.calls, best.calls
    ))
}

/// Evidence for the proposer: failed runs, and passing runs that hit a tool
/// timeout or took more than twice the median. Never the check or its output.
pub fn digest(rows: &[Value], tasks: &[Task], budget: usize) -> String {
    let mut secs: Vec<f64> = rows.iter().filter_map(|r| r["secs"].as_f64()).collect();
    secs.sort_by(|a, b| a.total_cmp(b));
    let median = secs.get(secs.len() / 2).copied().unwrap_or(0.0).max(1.0);
    let goals: HashMap<&str, &str> = tasks.iter().map(|t| (t.name.as_str(), t.goal.as_str())).collect();
    let mut out = String::new();
    for row in rows {
        let pass = row["pass"] == json!(true);
        let timeouts = row["tool_timeouts"].as_u64().unwrap_or(0);
        let slow = row["secs"].as_f64().unwrap_or(0.0) > 2.0 * median;
        if pass && timeouts == 0 && !slow && row["papercuts"].as_array().is_none_or(|p| p.is_empty()) {
            continue;
        }
        let task = row["task"].as_str().unwrap_or("?");
        let label = if row["pass"].is_null() {
            "UNGRADED (trajectory evidence unsupported)".to_string()
        } else if pass {
            "PASSED BUT COSTLY".to_string()
        } else {
            format!("FAILED ({})", row["failure_kind"].as_str().unwrap_or("?"))
        };
        let mut block = format!(
            "\n## run {task} rep {}: {label}\nended: {} | {}s | {} model calls | {timeouts} tool timeouts\ngoal: {}\nsteps:\n",
            row["rep"], row["outcome"].as_str().unwrap_or("?"), row["secs"], row["model_calls"],
            goals.get(task).copied().unwrap_or("?"));
        block.push_str(&format!("outcome_pass: {}; trajectory assessment: {}\n",
            row["outcome_pass"], row["trajectory"]));
        if let Some(papercuts) = row["papercuts"].as_array().filter(|p| !p.is_empty()) {
            block.push_str(&format!("agent-filed papercuts (untrusted observations): {papercuts:?}\n"));
        }
        for step in row["steps"].as_array().into_iter().flatten() {
            if let Some(level) = step.get("guard") {
                block.push_str(&format!("  - guard tripped: {level}\n"));
                continue;
            }
            let arg = step["args"]["command"].as_str().or(step["args"]["path"].as_str()).unwrap_or("");
            let arg: String = arg.chars().take(300).collect();
            let head: String = step["output_head"].as_str().unwrap_or("").chars().take(200).collect();
            let bytes = step["bytes"].as_u64().map(|b| format!("{b} bytes")).unwrap_or_else(|| "-".into());
            block.push_str(&format!("  - {} [{}] {arg}\n    -> ({bytes}) {}\n",
                step["tool"].as_str().unwrap_or("?"), step["tier"].as_str().unwrap_or("-"),
                head.replace('\n', " | ")));
        }
        if let Some(said) = row["agent_said"].as_str() {
            block.push_str(&format!("agent's final claim: {said}\n"));
        }
        if out.len() + block.len() > budget {
            out.push_str("\n(further runs omitted for length)\n");
            break;
        }
        out.push_str(&block);
    }
    out
}

/// Tokens a tuned prompt must not contain. File names (`numparse.py`) and
/// underscored identifiers (`test_sync`) are task-specific by construction;
/// a task's explicit `forbid` list covers answers like the env var its README
/// documents. Plain words such as "duration" stay allowed: they are English.
pub fn forbidden(tasks: &[Task]) -> Vec<String> {
    const GENERIC: &[&str] = &["readme.md", "contributing.md", "__init__.py"];
    fn walk(dir: &Path, out: &mut HashSet<String>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name == ".git" {
                continue;
            }
            if entry.path().is_dir() {
                walk(&entry.path(), out);
                continue;
            }
            if !GENERIC.contains(&name.as_str()) {
                if let Some((stem, _)) = name.rsplit_once('.') {
                    if stem.contains('_') {
                        out.insert(stem.to_string());
                    }
                }
                out.insert(name);
            }
        }
    }
    let mut out = HashSet::new();
    for task in tasks {
        walk(&task.dir.join("repo"), &mut out);
        out.extend(task.forbid.iter().map(|f| f.to_lowercase()));
    }
    let mut v: Vec<String> = out.into_iter().collect();
    v.sort();
    v
}

/// Reject a proposal that names the exam. Generic advice passes; a prompt that
/// mentions `test_numparse.py` or `OFFLINE=1` has memorised a task.
pub fn lint(cfg: &HarnessConfig, banned: &[String]) -> Result<(), String> {
    let text = format!("{}\n{}\n{}", cfg.system, cfg.bash_description, cfg.read_description).to_lowercase();
    match banned.iter().find(|b| text.contains(b.as_str())) {
        Some(hit) => Err(format!("names a scoring-set token {hit:?}")),
        None => Ok(()),
    }
}

const PROPOSER: &str = "\
You tune the configuration of a coding-agent harness. You may change only these
fields: system, bash_description, read_description, max_output (1000..64000),
read_max_lines (50..2000), bash_timeout_secs (5..600), max_turns (5..80),
stall_turns (2..20). You cannot change code, tests, or how runs are scored.

You get the current configuration and evidence from failed or costly runs of a
scoring set. The scoring set is a sample of real work; the harness will be used
on other work you will never see. So:

- Find the general cause behind the evidence, then fix that cause. Never name a
  file, function, task, value or command from the evidence: a fix that only
  works for this sample is worse than no fix.
- Prefer the smallest change that addresses the cause. One idea per candidate.
- Raise a limit only if the evidence shows the limit was hit.
- If a failure comes from a bug in the harness that configuration cannot fix,
  say so in code_suggestions. You may still add guidance that helps the agent
  avoid triggering the bug.
- Keep `system` under 2500 characters.

Reply with JSON only:
{\"candidates\": [{\"config\": {<only the fields you change>}, \"rationale\": \"...\"}],
 \"code_suggestions\": [\"...\"]}";

pub struct Proposal {
    pub patch: Value,
    pub rationale: String,
}

pub fn propose(llm: &dyn Llm, current: &HarnessConfig, evidence: &str, history: &str, k: usize)
    -> Result<(Vec<Proposal>, Vec<String>), String> {
    let user = format!(
        "Propose {k} candidates that differ from each other.\n\n# current configuration\n{}\n\n\
         # what was already tried\n{}\n\n# evidence\n{evidence}",
        serde_json::to_string_pretty(current).unwrap_or_default(),
        if history.is_empty() { "nothing yet" } else { history });
    let model = llm.roles().get("plan").to_string();
    let reply = llm.complete(&model, PROPOSER, &[json!({"role": "user", "content": user})], None)?;
    let text = reply.text;
    let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) else {
        return Err(format!("proposer returned no JSON: {}", text.chars().take(200).collect::<String>()));
    };
    let body: Value = serde_json::from_str(&text[a..=b]).map_err(|e| format!("proposer JSON: {e}"))?;
    let proposals = body["candidates"].as_array().into_iter().flatten().take(k)
        .map(|c| Proposal { patch: c["config"].clone(),
                            rationale: c["rationale"].as_str().unwrap_or("").to_string() })
        .collect();
    let notes = body["code_suggestions"].as_array().into_iter().flatten()
        .filter_map(Value::as_str).map(str::to_string).collect();
    Ok((proposals, notes))
}

pub struct Options<'a> {
    pub evals: &'a Path,
    pub twig: &'a Path,
    pub scratch: &'a Path,
    /// The live config. Adopted changes are written here; the original is kept
    /// alongside as `.baseline` so a change that fails confirmation can be undone.
    pub out: &'a Path,
    pub report: &'a Path,
    pub reps: u32,
    pub workers: usize,
    pub rounds: u32,
    pub k: usize,
    pub wall: Duration,
}

pub fn run(llm: &dyn Llm, start: &HarnessConfig, opt: &Options<'_>, log: &(dyn Fn(&str) + Sync))
    -> Result<HarnessConfig, String> {
    let tasks = crate::eval::load_tasks(opt.evals, None);
    if tasks.is_empty() {
        return Err(format!("no tasks under {}", opt.evals.display()));
    }
    let banned = forbidden(&tasks);
    let decisions = Decisions::open(opt.report.with_file_name("decisions.tsv")).map_err(|e| e.to_string())?;
    let mut report = String::from("# twig improve\n");
    let save = |r: &str| std::fs::write(opt.report, r).ok();
    let mut n = 0u32;
    let mut evaluate = |cfg: &HarnessConfig, label: &str| -> Vec<Value> {
        n += 1;
        let dir = opt.scratch.join(format!("{n:02}-{label}"));
        std::fs::create_dir_all(&dir).ok();
        let cfg_path = dir.join("harness.json");
        cfg.save(&cfg_path).ok();
        log(&format!("eval {label}: {} tasks x {} reps", tasks.len(), opt.reps));
        let spec = Spec { twig: opt.twig, config: &cfg_path, scratch: &dir, wall: opt.wall };
        let progress = |r: &Value| log(&format!("  {:<18} r{} {} {}s", r["task"].as_str().unwrap_or("?"),
            r["rep"], if r["pass"].is_null() { "UNGRADED" } else if r["pass"] == json!(true) { "PASS" } else { "FAIL" }, r["secs"]));
        let rows = run_all(&tasks, opt.reps, opt.workers, &spec, &progress);
        let body: String = rows.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(dir.join("results.jsonl"), body).ok();
        rows
    };

    if opt.out.exists() {
        std::fs::copy(opt.out, opt.out.with_extension("json.baseline")).ok();
    }
    let baseline_rows = evaluate(start, "baseline");
    let baseline = Score::of(&baseline_rows);
    let noise_runs = noise(&baseline_rows);
    report.push_str(&format!("\n## baseline\n\nrerun disagreement: {noise_runs} run(s)\n\n{}\n",
                             summarize(&baseline_rows)));
    save(&report);

    let (mut best_cfg, mut best, mut best_rows) = (start.clone(), baseline.clone(), baseline_rows);
    let mut history = String::new();
    let mut notes: Vec<String> = Vec::new();
    let mut adopted = false;
    for round in 1..=opt.rounds {
        let margin = noise_runs.max(1);
        let evidence = digest(&best_rows, &tasks, 14_000);
        // Always include a deterministic removal control, even when all tasks pass.
        let (mut proposals, round_notes) = if evidence.trim().is_empty() {
            (Vec::new(), Vec::new())
        } else {
            match propose(llm, &best_cfg, &evidence, &history, opt.k) {
                Ok(p) => p,
                Err(e) => {
                    report.push_str(&format!("\nproposer failed: {e}; still testing ablation control.\n"));
                    (Vec::new(), Vec::new())
                }
            }
        };
        if let Some(c) = crate::ablation::candidate(&best_cfg, round) {
            proposals.push(Proposal { patch: serde_json::to_value(c).map_err(|e| e.to_string())?,
                rationale: "Deletion-only control: remove one existing prompt paragraph, change no limits.".into() });
        }
        notes.extend(round_notes);
        report.push_str(&format!("\n## round {round} (needs +{margin} passes, or same passes and clearly cheaper)\n"));
        let mut winner: Option<(HarnessConfig, Score, Vec<Value>, String)> = None;
        for (i, p) in proposals.iter().enumerate() {
            let label = format!("r{round}c{}", i + 1);
            let cand = match best_cfg.merged(&p.patch).and_then(|c| lint(&c, &banned).map(|_| c)) {
                Ok(c) => c,
                Err(why) => {
                    report.push_str(&format!("\n### {label}: rejected before running: {why}\n> {}\n", p.rationale));
                    history.push_str(&format!("- {label} rejected ({why}): {}\n", p.rationale));
                    decisions.log(&format!("reject {label}"), "-", &why).ok();
                    continue;
                }
            };
            let rows = evaluate(&cand, &label);
            let score = Score::of(&rows);
            let verdict = accept(&score, &best, margin);
            report.push_str(&format!("\n### {label}: {}\n> {}\n\nchanges: {}\n\n{}\n",
                match &verdict { Ok(w) => format!("accepted, {w}"), Err(w) => format!("rejected, {w}") },
                p.rationale, best_cfg.diff(&cand).join("; "), summarize(&rows)));
            history.push_str(&format!("- {label} ({}): {} => {}/{} passes\n", best_cfg.diff(&cand).join("; "),
                p.rationale, score.passes, score.runs));
            save(&report);
            if let Ok(why) = verdict {
                let better = winner.as_ref().is_none_or(|(_, w, _, _)| {
                    (score.passes, -score.secs) > (w.passes, -w.secs)
                });
                if better {
                    winner = Some((cand, score, rows, why));
                }
            }
        }
        match winner {
            Some((cfg, score, rows, why)) => {
                decisions.log(&format!("adopt round {round}"), "keep previous config", &why).ok();
                cfg.save(opt.out).map_err(|e| e.to_string())?;
                report.push_str(&format!("\n**adopted** ({why}), written to {}\n", opt.out.display()));
                (best_cfg, best, best_rows, adopted) = (cfg, score, rows, true);
            }
            None => report.push_str("\nno candidate beat the current config.\n"),
        }
        save(&report);
    }

    if adopted {
        // Winner's curse: the best of several noisy evaluations is optimistic.
        // Re-run it once and keep it only if it still beats the baseline.
        let rows = evaluate(&best_cfg, "confirm");
        let confirm = Score::of(&rows);
        let verdict = accept(&confirm, &baseline, noise_runs.max(1));
        report.push_str(&format!("\n## confirmation re-run\n\n{}\n", summarize(&rows)));
        match verdict {
            Ok(why) => report.push_str(&format!("confirmed against baseline: {why}\n")),
            Err(why) => {
                let backup = opt.out.with_extension("json.baseline");
                if backup.exists() {
                    std::fs::copy(&backup, opt.out).ok();
                } else {
                    std::fs::remove_file(opt.out).ok();
                }
                decisions.log("revert to baseline", "keep tuned config", &why).ok();
                report.push_str(&format!("**not confirmed** ({why}): reverted to the baseline config.\n"));
                best_cfg = start.clone();
            }
        }
    }
    if !notes.is_empty() {
        report.push_str("\n## code changes the loop cannot make\n\n");
        for n in &notes {
            report.push_str(&format!("- {n}\n"));
        }
    }
    save(&report);
    Ok(best_cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(spec: &[(&str, &[bool])], secs: f64, calls: f64) -> Vec<Value> {
        spec.iter().flat_map(|(t, passes)| passes.iter().enumerate().map(move |(i, p)|
            json!({"task": t, "rep": i + 1, "pass": p, "secs": secs, "model_calls": calls})))
            .collect()
    }

    #[test]
    fn noise_counts_disagreeing_reps() {
        assert_eq!(noise(&rows(&[("a", &[true, true]), ("b", &[true, false]), ("c", &[false, false])], 1.0, 1.0)), 1);
    }

    #[test]
    fn more_passes_must_clear_the_margin() {
        let best = Score::of(&rows(&[("a", &[true, false]), ("b", &[false, false])], 10.0, 3.0));
        let one = Score::of(&rows(&[("a", &[true, true]), ("b", &[false, false])], 10.0, 3.0));
        let two = Score::of(&rows(&[("a", &[true, true]), ("b", &[true, false])], 10.0, 3.0));
        assert!(accept(&one, &best, 2).is_err(), "+1 is inside a noise margin of 2");
        assert!(accept(&two, &best, 2).is_ok());
    }

    #[test]
    fn a_task_that_always_passed_may_not_start_failing() {
        let best = Score::of(&rows(&[("a", &[true, true]), ("b", &[false, false]), ("c", &[false, false])], 10.0, 3.0));
        let cand = Score::of(&rows(&[("a", &[false, false]), ("b", &[true, true]), ("c", &[true, true])], 10.0, 3.0));
        assert!(accept(&cand, &best, 1).unwrap_err().contains("regression"));
    }

    #[test]
    fn same_passes_is_kept_only_when_clearly_cheaper() {
        let best = Score::of(&rows(&[("a", &[true, true])], 100.0, 4.0));
        assert!(accept(&Score::of(&rows(&[("a", &[true, true])], 70.0, 4.0)), &best, 1).is_ok());
        assert!(accept(&Score::of(&rows(&[("a", &[true, true])], 95.0, 4.0)), &best, 1).is_err(), "5% is noise");
        assert!(accept(&Score::of(&rows(&[("a", &[true, true])], 70.0, 6.0)), &best, 1).is_err(),
                "faster but 50% more calls is not cheaper");
    }

    #[test]
    fn lint_rejects_exam_tokens_not_english() {
        let banned = vec!["numparse.py".to_string(), "test_sync".to_string(), "offline=1".to_string()];
        let mut cfg = HarnessConfig::default();
        cfg.system.push_str("\nPipe long output through tail or grep before reading it.");
        assert!(lint(&cfg, &banned).is_ok());
        cfg.system.push_str("\nAlways set OFFLINE=1.");
        assert!(lint(&cfg, &banned).is_err());
    }

    #[test]
    fn partial_proposal_keeps_unmentioned_fields() {
        let base = HarnessConfig { max_output: 12_000, ..HarnessConfig::default() };
        let merged = base.merged(&json!({"bash_timeout_secs": 30})).unwrap();
        assert_eq!(merged.bash_timeout_secs, 30);
        assert_eq!(merged.max_output, 12_000, "an omitted field must not reset to its default");
        assert!(base.merged(&json!({"bash_timeout_secs": 1})).is_err(), "out of bounds is rejected, not clamped");
        assert!(base.merged(&json!({"deny_list": []})).is_err(), "unknown fields are rejected");
    }
    #[test]
    fn missing_usage_and_faster_token_regressions_do_not_win() {
        let mut base = rows(&[("a", &[true, true])], 10.0, 4.0);
        for row in &mut base {
            row["usage"] = json!({"complete":true,"input_tokens":800,"output_tokens":200,"models":["same"]});
        }
        let best = Score::of(&base);
        let mut candidate = best.clone();
        candidate.secs = 5.0;
        candidate.tokens = Some(1400.0);
        assert!(accept(&candidate, &best, 1).is_err());
        candidate.tokens = None;
        assert!(accept(&candidate, &best, 1).is_err());
        candidate.tokens = Some(700.0);
        assert!(accept(&candidate, &best, 1).is_ok());
    }
    #[test]
    fn trajectory_failure_cannot_hide_a_new_outcome_regression() {
        let base = json!({"task":"a","pass":false,"outcome_pass":true,"trajectory":{"pass":false},
            "secs":100,"model_calls":4});
        let cand = json!({"task":"a","pass":false,"outcome_pass":false,"trajectory":{"pass":false},
            "secs":1,"model_calls":1});
        assert!(accept(&Score::of(&[cand]), &Score::of(&[base]), 1).is_err());
    }
    #[test]
    fn unknown_trajectory_cannot_be_traded_for_a_proven_violation() {
        let base = json!({"task":"a","pass":null,"outcome_pass":true,"trajectory":{"pass":null},
            "secs":100,"model_calls":4});
        let cand = json!({"task":"a","pass":false,"outcome_pass":true,"trajectory":{"pass":false},
            "secs":1,"model_calls":1});
        assert!(accept(&Score::of(&[cand]), &Score::of(&[base]), 1).is_err());
    }
}
