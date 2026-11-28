//! Protected evidence-driven configuration optimizer (`twig improve`).
//!
//! Only `HarnessConfig` is editable. Source code, policy, judges and heldout
//! splits are out of reach of the proposer.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use super::ablation;
use super::dataset::{self, load_tasks, Task};
use super::lessons;
use super::runner::{run_all, summarize, Spec};
use crate::config::HarnessConfig;
use crate::llm::Llm;
use crate::record::Decisions;

#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    pub passes: usize,
    pub runs: usize,
    pub calls: f64,
    pub secs: f64,
    pub tokens: Option<f64>,
    pub models: BTreeSet<String>,
    pub per_task: BTreeMap<String, (usize, usize)>,
    pub axes: BTreeMap<String, (usize, usize)>,
    pub unknown: BTreeMap<String, usize>,
    pub run_models: BTreeMap<String, BTreeSet<String>>,
    pub papercuts: BTreeMap<String, usize>,
}

impl Score {
    pub fn of(rows: &[Value]) -> Self {
        let mut per_task = BTreeMap::new();
        for r in rows {
            let e = per_task
                .entry(r["task"].as_str().unwrap_or("?").to_string())
                .or_insert((0, 0));
            e.0 += usize::from(r["pass"] == json!(true));
            e.1 += 1;
        }
        let n = rows.len().max(1) as f64;
        let mean = |k: &str| {
            rows.iter()
                .map(|r| r[k].as_f64().unwrap_or(0.0))
                .sum::<f64>()
                / n
        };
        let mut axes = BTreeMap::new();
        let mut unknown = BTreeMap::new();
        let mut run_models = BTreeMap::new();
        let mut papercuts = BTreeMap::new();
        for row in rows {
            let task = row["task"].as_str().unwrap_or("?");
            run_models.insert(
                format!("{task}:{}", row["rep"]),
                row["usage"]["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            );
            for cut in row["papercuts"].as_array().into_iter().flatten() {
                *papercuts
                    .entry(cut["component"].as_str().unwrap_or("unknown").to_string())
                    .or_default() += 1;
            }
            for (axis, value) in [
                ("outcome", row.get("outcome_pass").unwrap_or(&row["pass"])),
                (
                    "trajectory",
                    row.pointer("/trajectory/pass").unwrap_or(&row["pass"]),
                ),
            ] {
                let e = axes.entry(format!("{task}:{axis}")).or_insert((0, 0));
                if value.is_null() {
                    *unknown.entry(format!("{task}:{axis}")).or_default() += 1;
                }
                e.0 += usize::from(*value == true);
                e.1 += 1;
            }
        }
        Self {
            passes: per_task.values().map(|v| v.0).sum(),
            runs: rows.len(),
            calls: mean("model_calls"),
            secs: mean("secs"),
            tokens: (!rows.is_empty() && rows.iter().all(|r| r["usage"]["complete"] == true)).then(
                || {
                    rows.iter()
                        .map(|r| {
                            r["usage"]["input_tokens"].as_u64().unwrap_or(0)
                                + r["usage"]["output_tokens"].as_u64().unwrap_or(0)
                        })
                        .sum::<u64>() as f64
                        / n
                },
            ),
            models: rows
                .iter()
                .flat_map(|r| r["usage"]["models"].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            per_task,
            axes,
            unknown,
            run_models,
            papercuts,
        }
    }
}

pub fn noise(rows: &[Value]) -> usize {
    let mut by: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for r in rows {
        let e = by.entry(r["task"].as_str().unwrap_or("?")).or_default();
        e.0 += usize::from(r["pass"] == json!(true));
        e.1 += 1;
    }
    by.values().map(|(p, n)| (*p).min(n - p)).sum()
}

fn non_regressing(cand: &Score, best: &Score) -> Result<(), String> {
    if cand.runs == 0
        || cand.runs != best.runs
        || cand.models != best.models
        || cand.run_models != best.run_models
        || cand.per_task.keys().ne(best.per_task.keys())
    {
        return Err("incomparable run sets or models".into());
    }
    if cand.run_models.values().any(|models| models.is_empty()) {
        return Err("inconclusive: model identity missing".into());
    }
    if !cand.unknown.is_empty() || !best.unknown.is_empty() {
        return Err("inconclusive: unknown quality evidence".into());
    }
    if cand.axes.keys().ne(best.axes.keys()) {
        return Err("incomparable quality axes".into());
    }
    for (axis, (passes, count)) in &best.axes {
        let (cp, cn) = cand.axes[axis];
        if cn != *count || cp < *passes {
            return Err(format!("regression on {axis}"));
        }
    }
    for (task, (passes, count)) in &best.per_task {
        let (cp, cn) = cand.per_task[task];
        if cn != *count || cp < *passes {
            return Err(format!("regression on {task}"));
        }
    }
    for (component, count) in &cand.papercuts {
        if *count > best.papercuts.get(component).copied().unwrap_or(0) {
            return Err(format!("papercut regression in {component}"));
        }
    }
    if cand.tokens.is_some() != best.tokens.is_some() {
        return Err("token usage coverage differs".into());
    }
    if let (Some(c), Some(b)) = (cand.tokens, best.tokens) {
        if c > b * 1.1 {
            return Err("token consumption regressed >10%".into());
        }
    }
    Ok(())
}

pub fn accept(cand: &Score, best: &Score, margin: usize) -> Result<String, String> {
    non_regressing(cand, best)?;
    if cand.passes >= best.passes + margin {
        return Ok(format!(
            "passes {} -> {} (margin {margin})",
            best.passes, cand.passes
        ));
    }
    if cand.passes == best.passes {
        if let (Some(c), Some(b)) = (cand.tokens, best.tokens) {
            if c <= b * 0.85 && cand.secs <= best.secs * 1.1 {
                return Ok(format!(
                    "same passes, mean tokens {b:.0} -> {c:.0} (not dollars)"
                ));
            }
        }
        if cand.secs <= best.secs * 0.8 && cand.calls <= best.calls * 1.1 {
            return Ok(format!(
                "same passes, mean secs {:.1} -> {:.1}",
                best.secs, cand.secs
            ));
        }
        if cand.calls <= best.calls * 0.85 && cand.secs <= best.secs * 1.1 {
            return Ok(format!(
                "same passes, mean calls {:.2} -> {:.2}",
                best.calls, cand.calls
            ));
        }
    }
    Err(format!(
        "passes {} vs {} (needs +{margin}); mean secs {:.1} vs {:.1}; mean calls {:.2} vs {:.2}",
        cand.passes, best.passes, cand.secs, best.secs, cand.calls, best.calls
    ))
}

/// Aggregate contrastive comparison across tasks that have both passing and
/// failing repetitions (isolates behavioral variance without leaking any task
/// name, prompt, command, or judge text).
fn contrastive_variance(rows: &[Value]) -> Value {
    let mut by_task: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for row in rows {
        if let Some(task) = row["task"].as_str() {
            by_task.entry(task.to_owned()).or_default().push(row);
        }
    }
    let mut mixed_tasks = 0usize;
    let mut pass_calls = Vec::new();
    let mut fail_calls = Vec::new();
    let mut pass_secs = Vec::new();
    let mut fail_secs = Vec::new();
    let mut pass_timeouts = 0u64;
    let mut fail_timeouts = 0u64;
    let mut fail_kinds = BTreeMap::<String, usize>::new();

    for group in by_task.values() {
        let has_pass = group.iter().any(|r| r["pass"] == true);
        let has_fail = group.iter().any(|r| r["pass"] == false);
        if !(has_pass && has_fail) {
            continue;
        }
        mixed_tasks += 1;
        for r in group {
            let calls = r["model_calls"].as_f64().unwrap_or(0.0);
            let secs = r["secs"].as_f64().unwrap_or(0.0);
            let timeouts = r["tool_timeouts"].as_u64().unwrap_or(0);
            if r["pass"] == true {
                pass_calls.push(calls);
                pass_secs.push(secs);
                pass_timeouts += timeouts;
            } else if r["pass"] == false {
                fail_calls.push(calls);
                fail_secs.push(secs);
                fail_timeouts += timeouts;
                let kind = match r["failure_kind"].as_str() {
                    Some("process") => "process",
                    Some("trajectory") => "trajectory",
                    _ => "content",
                };
                *fail_kinds.entry(kind.into()).or_default() += 1;
            }
        }
    }
    let mean = |xs: &[f64]| {
        if xs.is_empty() {
            0.0
        } else {
            xs.iter().sum::<f64>() / xs.len() as f64
        }
    };
    json!({
        "mixed_tasks": mixed_tasks,
        "pass_mean_calls": mean(&pass_calls),
        "fail_mean_calls": mean(&fail_calls),
        "pass_mean_secs": mean(&pass_secs),
        "fail_mean_secs": mean(&fail_secs),
        "pass_timeouts": pass_timeouts,
        "fail_timeouts": fail_timeouts,
        "fail_kinds": fail_kinds,
    })
}

pub fn digest(rows: &[Value], _tasks: &[Task], budget: usize) -> String {
    let mut failures = BTreeMap::<String, usize>::new();
    let mut cuts = BTreeMap::<String, usize>::new();
    for row in rows {
        if row["pass"] == false {
            let kind = match row["failure_kind"].as_str() {
                Some("process") => "process",
                Some("trajectory") => "trajectory",
                _ => "content",
            };
            *failures.entry(kind.into()).or_default() += 1;
        }
        for cut in row["papercuts"].as_array().into_iter().flatten() {
            let component = match cut["component"].as_str() {
                Some("bash") => "bash",
                Some("read") => "read",
                Some("context") => "context",
                _ => "other",
            };
            *cuts.entry(component.into()).or_default() += 1;
        }
    }
    let score = Score::of(rows);
    let evidence = json!({
        "training_runs": rows.len(),
        "passes": score.passes,
        "unknown": rows.iter().filter(|r| r["pass"].is_null()).count(),
        "failures": failures,
        "tool_timeouts": rows.iter().filter_map(|r| r["tool_timeouts"].as_u64()).sum::<u64>(),
        "mean_model_calls": score.calls,
        "mean_seconds": score.secs,
        "mean_reported_tokens": score.tokens,
        "papercuts_by_component": cuts,
        "contrastive_variance": contrastive_variance(rows)
    });
    let text = evidence.to_string();
    if text.len() <= budget {
        text
    } else {
        String::new()
    }
}

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

pub fn lint(cfg: &HarnessConfig, banned: &[String]) -> Result<(), String> {
    let text = format!(
        "{}\n{}\n{}",
        cfg.system, cfg.bash_description, cfg.read_description
    )
    .to_lowercase();
    match banned.iter().find(|b| text.contains(b.as_str())) {
        Some(hit) => Err(format!("names a scoring-set token {hit:?}")),
        None => Ok(()),
    }
}

const PROPOSER: &str = "\
You tune the configuration of a coding-agent harness. You may change only these
fields: system (1..=6000 bytes), bash_description (1..=1500 bytes),
read_description (1..=1500 bytes), max_output (1000..=64000),
read_max_lines (50..=2000), bash_timeout_secs (5..=600), max_turns (5..=80),
stall_turns (2..=20), context_max_bytes (8000..=2000000),
subagent_concurrency (1..=8). You cannot change code, tests, or how runs are scored.

You get the current configuration and evidence from failed or costly runs of a
scoring set. The scoring set is a sample of real work; the harness will be used
on other work you will never see. So:

- Find the general cause behind the evidence, then fix that cause. Use
  `contrastive_variance` (passing vs failing runs on the same task) to diagnose
  whether failures correlate with tool timeouts or excessive call loops. Never
  name a file, function, task, value or command from the evidence: a fix that
  only works for this sample is worse than no fix.
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

pub fn propose(
    llm: &dyn Llm,
    current: &HarnessConfig,
    evidence: &str,
    history: &str,
    k: usize,
) -> Result<(Vec<Proposal>, Vec<String>), String> {
    let user = format!(
        "Propose {k} candidates that differ from each other.\n\n# current configuration\n{}\n\n\
         # what was already tried\n{}\n\n# evidence\n{evidence}",
        serde_json::to_string_pretty(current).unwrap_or_default(),
        if history.is_empty() {
            "nothing yet"
        } else {
            history
        }
    );
    let model = llm.roles().get("plan").to_string();
    let reply = llm.complete(
        &model,
        PROPOSER,
        &[json!({"role": "user", "content": user})],
        None,
    )?;
    let text = reply.text;
    let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) else {
        return Err(format!(
            "proposer returned no JSON: {}",
            text.chars().take(200).collect::<String>()
        ));
    };
    let body: Value =
        serde_json::from_str(&text[a..=b]).map_err(|e| format!("proposer JSON: {e}"))?;
    let proposals = body["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .take(k)
        .map(|c| Proposal {
            patch: c["config"].clone(),
            rationale: c["rationale"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    let notes = body["code_suggestions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    Ok((proposals, notes))
}

pub struct Options<'a> {
    pub evals: &'a Path,
    pub twig: &'a Path,
    pub scratch: &'a Path,
    pub out: &'a Path,
    pub report: &'a Path,
    pub reps: u32,
    pub workers: usize,
    pub rounds: u32,
    pub k: usize,
    pub wall: Duration,
}

pub fn run(
    llm: &dyn Llm,
    start: &HarnessConfig,
    opt: &Options<'_>,
    log: &(dyn Fn(&str) + Sync),
) -> Result<HarnessConfig, String> {
    if opt.reps == 0 {
        return Err("improvement requires at least one repetition".into());
    }
    let tasks = load_tasks(opt.evals, None);
    let data = dataset::separate(tasks)?;
    let banned = forbidden(&data.train);
    let decisions =
        Decisions::open(opt.report.with_file_name("decisions.tsv")).map_err(|e| e.to_string())?;
    let lesson_path = opt.report.with_file_name("lessons.json");
    let mut history = lessons::context(&lesson_path)?;
    let mut report = format!(
        "# twig improve\n\nProtected splits: {} train, {} dev, {} final. Only aggregate training evidence reaches the proposer. Dev and final run after proposals stop.\n",
        data.train.len(),
        data.dev.len(),
        data.final_holdout.len()
    );
    let save = |report: &str| std::fs::write(opt.report, report).map_err(|e| e.to_string());
    let mut n = 0u32;
    let mut evaluate = |cfg: &HarnessConfig,
                        label: &str,
                        tasks: &[Task],
                        training: bool|
     -> Result<Vec<Value>, String> {
        n += 1;
        let dir = opt.scratch.join(format!("{n:02}-{label}"));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let cfg_path = dir.join("harness.json");
        cfg.save(&cfg_path).map_err(|e| e.to_string())?;
        log(&format!(
            "eval {label}: {} tasks x {} reps",
            tasks.len(),
            opt.reps
        ));
        let spec = Spec {
            twig: opt.twig,
            config: &cfg_path,
            scratch: &dir,
            wall: opt.wall,
        };
        let progress = |r: &Value| {
            log(&format!(
                "  {:<18} r{} {} {}s",
                r["task"].as_str().unwrap_or("?"),
                r["rep"],
                if r["pass"].is_null() {
                    "UNGRADED"
                } else if r["pass"] == true {
                    "PASS"
                } else {
                    "FAIL"
                },
                r["secs"]
            ))
        };
        let rows = run_all(tasks, opt.reps, opt.workers, &spec, &progress);
        let results = dir.join("results.jsonl");
        let body: String = rows.iter().map(|row| format!("{row}\n")).collect();
        std::fs::write(&results, body).map_err(|e| e.to_string())?;
        if training {
            let evidence = opt.report.with_file_name("improvement-evidence");
            std::fs::create_dir_all(&evidence).map_err(|e| e.to_string())?;
            let bytes = std::fs::read(&results).map_err(|e| e.to_string())?;
            let durable = evidence.join(format!("{}.jsonl", blake3::hash(&bytes).to_hex()));
            std::fs::write(&durable, bytes).map_err(|e| e.to_string())?;
            lessons::record(&lesson_path, &durable, &rows)?;
        }
        Ok(rows)
    };
    let baseline_rows = evaluate(start, "train-baseline", &data.train, true)?;
    let baseline = Score::of(&baseline_rows);
    let margin = noise(&baseline_rows).max(1);
    report.push_str(&format!(
        "\n## training baseline\n\n{}\n",
        summarize(&baseline_rows)
    ));
    save(&report)?;
    if baseline_rows
        .iter()
        .any(|row| row["pass"].is_null() || row["isolation"] != true)
    {
        report.push_str("\n**inconclusive**: baseline has unknown evidence or unavailable isolation; no config changed.\n");
        save(&report)?;
        return Ok(start.clone());
    }
    let (mut best_cfg, mut best, mut best_rows) = (start.clone(), baseline, baseline_rows);
    let mut notes = Vec::new();
    let mut adopted = false;
    for round in 1..=opt.rounds {
        let evidence = digest(&best_rows, &data.train, 14_000);
        let (mut proposals, round_notes) =
            match propose(llm, &best_cfg, &evidence, &history, opt.k) {
                Ok(proposals) => proposals,
                Err(error) => {
                    report.push_str(&format!(
                        "\nproposer failed: {error}; still testing removal control.\n"
                    ));
                    (Vec::new(), Vec::new())
                }
            };
        if let Some(control) = ablation::candidate(&best_cfg, round) {
            proposals.push(Proposal {
                patch: serde_json::to_value(control).map_err(|e| e.to_string())?,
                rationale: "Deletion-only control: remove existing words, change no limits.".into(),
            });
        }
        notes.extend(round_notes);
        report.push_str(&format!("\n## training round {round}\n"));
        let mut winner: Option<(HarnessConfig, Score, Vec<Value>, String)> = None;
        for (i, proposal) in proposals.iter().enumerate() {
            let label = format!("r{round}c{}", i + 1);
            let candidate = match best_cfg
                .merged(&proposal.patch)
                .and_then(|cfg| lint(&cfg, &banned).map(|_| cfg))
            {
                Ok(candidate) => candidate,
                Err(why) => {
                    report.push_str(&format!(
                        "\n### {label}: rejected before evaluation: {why}\n"
                    ));
                    history.push_str(&format!(
                        "- {label}: rejected by genericity/validation gate.\n"
                    ));
                    continue;
                }
            };
            let rows = evaluate(&candidate, &label, &data.train, true)?;
            let score = Score::of(&rows);
            let verdict = accept(&score, &best, margin);
            report.push_str(&format!(
                "\n### {label}: {verdict:?}\n> {}\n\n{}\n",
                proposal.rationale,
                summarize(&rows)
            ));
            history.push_str(&format!(
                "- {label}: {}. Training evidence: {}\n",
                if verdict.is_ok() {
                    "improved"
                } else {
                    "no proven improvement"
                },
                digest(&rows, &data.train, 4_000)
            ));
            if let Ok(why) = verdict {
                if winner.as_ref().is_none_or(|(_, previous, _, _)| {
                    (score.passes, -score.secs) > (previous.passes, -previous.secs)
                }) {
                    winner = Some((candidate, score, rows, why));
                }
            }
        }
        if let Some((cfg, score, rows, why)) = winner {
            decisions
                .log(
                    &format!("select training round {round}"),
                    "keep previous candidate",
                    &why,
                )
                .map_err(|e| e.to_string())?;
            (best_cfg, best, best_rows, adopted) = (cfg, score, rows, true);
        } else {
            report.push_str("\nNo training candidate demonstrated improvement.\n");
        }
        save(&report)?;
    }
    if adopted {
        let dev_base = evaluate(start, "dev-baseline", &data.dev, false)?;
        let dev_candidate = evaluate(&best_cfg, "dev-confirm", &data.dev, false)?;
        let confirmation = accept(
            &Score::of(&dev_candidate),
            &Score::of(&dev_base),
            noise(&dev_base).max(1),
        );
        report.push_str(&format!(
            "\n## dev confirmation\n\nBaseline:\n{}\nCandidate:\n{}\n{confirmation:?}\n",
            summarize(&dev_base),
            summarize(&dev_candidate)
        ));
        if let Err(why) = confirmation {
            report.push_str(&format!(
                "\n**not confirmed** ({why}): original config retained; final holdout untouched.\n"
            ));
            best_cfg = start.clone();
        } else {
            save(&report)?;
            dataset::reserve_final(opt.evals, &data.final_holdout)?;
            let final_base = evaluate(start, "final-baseline", &data.final_holdout, false)?;
            let final_candidate =
                evaluate(&best_cfg, "final-candidate", &data.final_holdout, false)?;
            let verdict = non_regressing(&Score::of(&final_candidate), &Score::of(&final_base))
                .and_then(|()| {
                    let candidate = Score::of(&final_candidate);
                    let baseline = Score::of(&final_base);
                    if candidate.secs > baseline.secs * 1.1
                        || candidate.calls > baseline.calls * 1.1
                    {
                        Err("final runtime or model-call cost regressed >10%".to_string())
                    } else {
                        Ok(())
                    }
                });
            report.push_str(&format!(
                "\n## final holdout (consumed once; never proposer feedback)\n\nBaseline:\n{}\nCandidate:\n{}\n",
                summarize(&final_base),
                summarize(&final_candidate)
            ));
            match verdict {
                Ok(()) => {
                    if opt.out.exists() {
                        std::fs::copy(opt.out, opt.out.with_extension("json.baseline"))
                            .map_err(|e| e.to_string())?;
                    }
                    best_cfg.save(opt.out).map_err(|e| e.to_string())?;
                    report.push_str("\n**confirmed**: dev improved and final has no quality/cost regressions. Candidate installed.\n");
                }
                Err(why) => {
                    decisions
                        .log("rollback final candidate", "install tuned config", &why)
                        .map_err(|e| e.to_string())?;
                    best_cfg = start.clone();
                    report.push_str(&format!(
                        "\n**final regression or inconclusive evidence** ({why}): rolled back to original config; live file was never changed.\n"
                    ));
                }
            }
        }
    } else {
        report.push_str("\n**no improvement**: original config retained; holdouts untouched.\n");
    }
    if !notes.is_empty() {
        report.push_str("\n## code changes the loop cannot make\n\n");
        for note in notes {
            report.push_str(&format!("- {note}\n"));
        }
    }
    save(&report)?;
    Ok(best_cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::dataset::{import_snapshot, ImportTask, Split};
    use crate::llm::stub::StubLlm;

    fn rows(spec: &[(&str, &[bool])], secs: f64, calls: f64) -> Vec<Value> {
        spec.iter()
            .flat_map(|(t, passes)| {
                passes.iter().enumerate().map(move |(i, p)| {
                    json!({"task": t, "rep": i + 1, "pass": p, "secs": secs, "model_calls": calls, "usage":{"models":["same"]}})
                })
            })
            .collect()
    }

    #[test]
    fn noise_counts_disagreeing_reps() {
        assert_eq!(
            noise(&rows(
                &[
                    ("a", &[true, true]),
                    ("b", &[true, false]),
                    ("c", &[false, false])
                ],
                1.0,
                1.0
            )),
            1
        );
    }

    #[test]
    fn more_passes_must_clear_the_margin() {
        let best = Score::of(&rows(
            &[("a", &[true, false]), ("b", &[false, false])],
            10.0,
            3.0,
        ));
        let one = Score::of(&rows(
            &[("a", &[true, true]), ("b", &[false, false])],
            10.0,
            3.0,
        ));
        let two = Score::of(&rows(
            &[("a", &[true, true]), ("b", &[true, false])],
            10.0,
            3.0,
        ));
        assert!(accept(&one, &best, 2).is_err());
        assert!(accept(&two, &best, 2).is_ok());
    }

    #[test]
    fn a_task_that_always_passed_may_not_start_failing() {
        let best = Score::of(&rows(
            &[
                ("a", &[true, true]),
                ("b", &[false, false]),
                ("c", &[false, false]),
            ],
            10.0,
            3.0,
        ));
        let cand = Score::of(&rows(
            &[
                ("a", &[false, false]),
                ("b", &[true, true]),
                ("c", &[true, true]),
            ],
            10.0,
            3.0,
        ));
        assert!(accept(&cand, &best, 1).unwrap_err().contains("regression"));
    }

    #[test]
    fn same_passes_is_kept_only_when_clearly_cheaper() {
        let best = Score::of(&rows(&[("a", &[true, true])], 100.0, 4.0));
        assert!(accept(
            &Score::of(&rows(&[("a", &[true, true])], 70.0, 4.0)),
            &best,
            1
        )
        .is_ok());
        assert!(accept(
            &Score::of(&rows(&[("a", &[true, true])], 95.0, 4.0)),
            &best,
            1
        )
        .is_err());
        assert!(accept(
            &Score::of(&rows(&[("a", &[true, true])], 70.0, 6.0)),
            &best,
            1
        )
        .is_err());
    }

    #[test]
    fn lint_rejects_exam_tokens_not_english() {
        let banned = vec![
            "numparse.py".to_string(),
            "test_sync".to_string(),
            "offline=1".to_string(),
        ];
        let mut cfg = HarnessConfig::default();
        cfg.system
            .push_str("\nPipe long output through tail or grep before reading it.");
        assert!(lint(&cfg, &banned).is_ok());
        cfg.system.push_str("\nAlways set OFFLINE=1.");
        assert!(lint(&cfg, &banned).is_err());
    }

    #[test]
    fn proposal_screen_uses_train_tasks_only() {
        let root = std::env::temp_dir().join(format!("twig-improve-split-{}", std::process::id()));
        let corpus = root.join("corpus");
        let mut tasks = Vec::new();
        for (name, split) in [
            ("train", Split::Train),
            ("dev", Split::Dev),
            ("final", Split::Final),
        ] {
            let source = root.join(name);
            std::fs::create_dir_all(&source).unwrap();
            std::fs::write(source.join("README.md"), format!("task {name}")).unwrap();
            let dir = import_snapshot(
                &corpus,
                &source,
                &ImportTask {
                    name: name.into(),
                    source: name.into(),
                    goal: "solve".into(),
                    split,
                    check: "true".into(),
                    forbid: vec![format!("secret-{name}")],
                    trajectory: vec![],
                    wip: vec![],
                },
            )
            .unwrap();
            tasks.push(Task {
                name: name.into(),
                dir,
                goal: "solve".into(),
                check: "true".into(),
                forbid: vec![format!("secret-{name}")],
                trajectory: vec![],
                split: Some(split),
                source: Some(name.into()),
            });
        }
        let data = dataset::separate(tasks).unwrap();
        let banned = forbidden(&data.train);
        assert!(banned.contains(&"secret-train".into()));
        assert!(!banned.contains(&"secret-final".into()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_proposal_keeps_unmentioned_fields() {
        let base = HarnessConfig {
            max_output: 12_000,
            ..HarnessConfig::default()
        };
        let merged = base.merged(&json!({"bash_timeout_secs": 30})).unwrap();
        assert_eq!(merged.bash_timeout_secs, 30);
        assert_eq!(merged.max_output, 12_000);
        assert!(base.merged(&json!({"bash_timeout_secs": 1})).is_err());
        assert!(base.merged(&json!({"deny_list": []})).is_err());
    }

    #[test]
    fn missing_usage_and_faster_token_regressions_do_not_win() {
        let mut base = rows(&[("a", &[true, true])], 10.0, 4.0);
        for row in &mut base {
            row["usage"] =
                json!({"complete":true,"input_tokens":800,"output_tokens":200,"models":["same"]});
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
            "secs":100,"model_calls":4,"usage":{"models":["same"]}});
        let cand = json!({"task":"a","pass":false,"outcome_pass":false,"trajectory":{"pass":false},
            "secs":1,"model_calls":1,"usage":{"models":["same"]}});
        assert!(accept(&Score::of(&[cand]), &Score::of(&[base]), 1).is_err());
    }

    #[test]
    fn unknown_trajectory_cannot_be_traded_for_a_proven_violation() {
        let base = json!({"task":"a","pass":null,"outcome_pass":true,"trajectory":{"pass":null},
            "secs":100,"model_calls":4,"usage":{"models":["same"]}});
        let cand = json!({"task":"a","pass":false,"outcome_pass":true,"trajectory":{"pass":false},
            "secs":1,"model_calls":1,"usage":{"models":["same"]}});
        assert!(accept(&Score::of(&[cand]), &Score::of(&[base]), 1).is_err());
    }

    #[test]
    fn proposer_digest_omits_all_source_and_judge_material() {
        let row = json!({"task":"SECRET", "pass":false, "failure_kind":"content", "goal":"SECRET",
            "outcome":"SECRET", "judge_tail":"SECRET", "trajectory":{"contract":"SECRET"},
            "steps":[{"args":{"command":"SECRET"},"output_head":"SECRET"}],
            "papercuts":[{"component":"SECRET","description":"SECRET"}]});
        let text = digest(&[row], &[], 14_000);
        assert!(text.contains("content"));
        assert!(!text.contains("SECRET"));
    }

    #[test]
    fn contrastive_variance_isolates_mixed_runs_without_leaking_task_names() {
        let rows = vec![
            json!({"task":"SECRET_MIXED","pass":true,"secs":4.0,"model_calls":2.0,"tool_timeouts":0}),
            json!({"task":"SECRET_MIXED","pass":false,"failure_kind":"process","secs":20.0,"model_calls":8.0,"tool_timeouts":2}),
            json!({"task":"SECRET_ALL_PASS","pass":true,"secs":3.0,"model_calls":1.0,"tool_timeouts":0}),
        ];
        let text = digest(&rows, &[], 14_000);
        assert!(!text.contains("SECRET"));
        let v: Value = serde_json::from_str(&text).unwrap();
        let cv = &v["contrastive_variance"];
        assert_eq!(cv["mixed_tasks"], 1);
        assert_eq!(cv["pass_mean_calls"], 2.0);
        assert_eq!(cv["fail_mean_calls"], 8.0);
        assert_eq!(cv["fail_timeouts"], 2);
        assert_eq!(cv["fail_kinds"]["process"], 1);
    }

    #[test]
    fn final_gates_reject_papercuts_and_unknown_model_identity() {
        let base = Score::of(&rows(&[("a", &[true])], 10.0, 2.0));
        assert!(non_regressing(&base, &base).is_ok());
        let mut candidate = base.clone();
        candidate.papercuts.insert("read".into(), 1);
        assert!(non_regressing(&candidate, &base).is_err());
        candidate = base.clone();
        candidate
            .run_models
            .values_mut()
            .for_each(|models| models.clear());
        assert!(non_regressing(&candidate, &candidate).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn final_regression_retains_original_file_and_never_reaches_proposer() {
        if !crate::exec::isolation::bwrap_userns_usable() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("twig-improvement-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let corpus = root.join("corpus");
        for (name, split) in [
            ("training", Split::Train),
            ("development", Split::Dev),
            ("SECRET_FINAL", Split::Final),
        ] {
            let source = root.join(name);
            std::fs::create_dir_all(&source).unwrap();
            std::fs::write(
                source.join("mode"),
                if split == Split::Final {
                    "final"
                } else {
                    "train"
                },
            )
            .unwrap();
            import_snapshot(
                &corpus,
                &source,
                &ImportTask {
                    name: name.into(),
                    source: format!("real:{name}"),
                    goal: format!("Solve {name}"),
                    split,
                    check: "test -f solved".into(),
                    forbid: vec![],
                    trajectory: vec![],
                    wip: vec![],
                },
            )
            .unwrap();
        }
        let executable = root.join("solver");
        std::fs::write(
            &executable,
            r##"#!/usr/bin/python3
import json, pathlib
config = json.loads(pathlib.Path('/harness.json').read_text())
tuned = config['max_output'] == 10001
final = pathlib.Path('/workspace/mode').read_text() == 'final'
if tuned != final:
    pathlib.Path('/workspace/solved').write_text('done')
row = {'kind':'model.reply','model':'same','usage':{'input_tokens':100,'output_tokens':10},'tool_calls':[]}
pathlib.Path('/runs/evidence.jsonl').write_text(json.dumps(row)+'\n')
print(json.dumps({'id':1,'result':{'reason':'final','text':'done'}}))
"##,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let start = HarnessConfig::default();
        let live = root.join("live.json");
        start.save(&live).unwrap();
        let original = std::fs::read(&live).unwrap();
        let scratch = root.join("scratch");
        let report = root.join("report.md");
        let llm = StubLlm::new(vec![StubLlm::text(
            r#"{"candidates":[{"config":{"max_output":10001},"rationale":"bounded output adjustment"}]}"#,
        )]);
        let options = Options {
            evals: &corpus,
            twig: &executable,
            scratch: &scratch,
            out: &live,
            report: &report,
            reps: 1,
            workers: 1,
            rounds: 1,
            k: 1,
            wall: Duration::from_secs(10),
        };
        let result = run(&llm, &start, &options, &|_| {}).unwrap();
        assert_eq!(result, start);
        assert_eq!(std::fs::read(live).unwrap(), original);
        assert!(std::fs::read_to_string(report)
            .unwrap()
            .contains("rolled back"));
        assert_eq!(llm.seen().len(), 1);
        assert!(!llm.seen()[0].contains("SECRET_FINAL"));
        assert!(dataset::separate(load_tasks(&corpus, None)).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
