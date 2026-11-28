//! Recursive Collective Intelligence (RCI) primitives for `twig`:
//! 1. **Equal-Budget Ladder (`grade_ladder`)**: grades multi-agent runs against solo runs at
//!    matched completion-token budget `n_B = B_team / B_solo`:
//!      - Level 0: `P_team <= P_1` (net coordination overhead)
//!      - Level 1: `P_1 < P_team <= V(n_B)` (beats 1 agent, within majority vote)
//!      - Level 2: `V(n_B) < P_team <= pass@n_B` (beats vote, within independent oracle selection)
//!      - Level 3: `P_team > pass@n_B` (genuine collective emergence beyond independent sampling)
//! 2. **Label-Free Trajectory Harvesting (`harvest_sessions`)**: filters tamper-evident session
//!    ledgers using automatic verification outcomes (`verify.passed` / `verify.completed`) and
//!    attributes credit across subagent roles (`scout`, `code`, `test`, `advisor`, `reviewer`).
//! 3. **Gated Role-Card Recursion (`distill_role_notes` & `gate_generation`)**: synthesizes compact
//!    `<= 1,200`-char role protocol cards under `.twig/notes/<role>.md` with anti-overfitting
//!    token checks and promotes a new generation only when confirmation runs clear non-regression
//!    and equal-budget efficiency gates.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::paths;
use crate::llm::{Llm, Provider};
use crate::record::Record;
use crate::runtime::blackboard::{
    baseline_role_note, extract_decisive_facts, status_report, validate_role_note, ROLES,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LadderReport {
    pub tasks: usize,
    pub solo_runs: usize,
    pub team_runs: usize,
    pub solo_unit_tokens: f64,
    pub team_unit_tokens: f64,
    pub budget_multiple_nb: f64,
    pub p_single: f64,
    pub v_majority: f64,
    pub pass_at_nb: f64,
    pub p_team: f64,
    pub margin_over_vote: f64,
    pub margin_over_pass_at_nb: f64,
    pub equiv_solo_budget: f64,
    pub token_saving_pct: f64,
    pub level: u8,
    pub level_label: String,
    pub task_wins: usize,
    pub task_losses: usize,
    pub task_ties: usize,
    pub sign_test_p: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoleCredit {
    pub invocations: usize,
    pub verified_sessions: usize,
    pub fact_citations: usize,
    pub patches_produced: usize,
    pub blockers_caught: usize,
    pub timeouts_or_failures: usize,
    pub credit_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarvestReport {
    pub scanned_files: usize,
    pub chain_valid_files: usize,
    pub verified_collective_sessions: usize,
    pub role_credits: BTreeMap<String, RoleCredit>,
    pub sample_facts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateVerdict {
    pub promoted: bool,
    pub generation: u32,
    pub reason: String,
    pub prev_accuracy: f64,
    pub cand_accuracy: f64,
    pub prev_nb: f64,
    pub cand_nb: f64,
    pub regressions: Vec<String>,
}

fn run_cost(row: &Value) -> f64 {
    let out_tok = row
        .pointer("/usage/output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if out_tok > 0 {
        return out_tok as f64;
    }
    let in_tok = row
        .pointer("/usage/input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if in_tok > 0 {
        return in_tok as f64;
    }
    row.get("model_calls")
        .and_then(Value::as_f64)
        .unwrap_or(1.0)
        .max(1.0)
}

fn binom(n: usize, k: usize) -> f64 {
    if k > n {
        return 0.0;
    }
    let k = k.min(n - k);
    let mut acc = 1.0f64;
    for i in 1..=k {
        acc *= (n + 1 - i) as f64 / (i as f64);
    }
    acc
}

/// Exact majority-vote pass probability `V(m)` for `m` i.i.d. draws of accuracy `p`
/// (splitting exact ties 50/50 when `m` is even), linearly interpolated for fractional `n_B`.
pub fn majority_vote_prob(p: f64, nb: f64) -> f64 {
    let p = p.clamp(0.0, 1.0);
    let nb = nb.max(1.0);
    let lo = nb.floor() as usize;
    let hi = nb.ceil() as usize;
    let frac = nb - (lo as f64);
    let vote_int = |m: usize| -> f64 {
        let m = m.clamp(1, 64);
        let mut sum = 0.0f64;
        for k in 0..=m {
            let prob = binom(m, k) * p.powi(k as i32) * (1.0 - p).powi((m - k) as i32);
            if k * 2 > m {
                sum += prob;
            } else if k * 2 == m {
                sum += 0.5 * prob;
            }
        }
        sum.clamp(0.0, 1.0)
    };
    if lo == hi {
        vote_int(lo)
    } else {
        (1.0 - frac) * vote_int(lo) + frac * vote_int(hi)
    }
}

/// Unbiased combinatorial `pass@k` when `n_samples >= k`, falling back to `1 - (1 - p)^k`
/// when `k` exceeds recorded solo repetitions; interpolated smoothly for non-integer `n_B`.
pub fn pass_at_nb_task(n_samples: usize, c_passes: usize, nb: f64) -> f64 {
    if n_samples == 0 {
        return 0.0;
    }
    let p = (c_passes as f64 / n_samples as f64).clamp(0.0, 1.0);
    let nb = nb.max(1.0);
    let lo = nb.floor() as usize;
    let hi = nb.ceil() as usize;
    let frac = nb - (lo as f64);
    let at_k = |k: usize| -> f64 {
        if k == 0 {
            return 0.0;
        }
        if c_passes == 0 {
            return 0.0;
        }
        if c_passes >= n_samples {
            return 1.0;
        }
        if k <= n_samples {
            if n_samples - c_passes < k {
                1.0
            } else {
                (1.0 - binom(n_samples - c_passes, k) / binom(n_samples, k)).clamp(0.0, 1.0)
            }
        } else {
            (1.0 - (1.0 - p).powi(k as i32)).clamp(0.0, 1.0)
        }
    };
    if lo == hi {
        at_k(lo)
    } else {
        (1.0 - frac) * at_k(lo) + frac * at_k(hi)
    }
}

/// Two-sided exact binomial sign-test p-value (`H0: p = 0.5`) on paired task wins vs losses.
fn exact_sign_test_p(wins: usize, losses: usize) -> f64 {
    let n = wins + losses;
    if n == 0 {
        return 1.0;
    }
    let extreme = wins.min(losses);
    let mut tail = 0.0f64;
    let half_n = 0.5f64.powi(n as i32);
    for k in 0..=extreme {
        tail += binom(n, k) * half_n;
    }
    (2.0 * tail).min(1.0)
}

/// Grade collective (`team_rows`) vs independent (`solo_rows`) JSONL evaluation runs
/// on the Stanford RCI Equal-Budget Ladder (`Level 0..=3`).
pub fn grade_ladder(solo_rows: &[Value], team_rows: &[Value]) -> Result<LadderReport, String> {
    if solo_rows.is_empty() || team_rows.is_empty() {
        return Err("both solo and team result sets must be non-empty".into());
    }
    let mut solo_by_task: BTreeMap<String, (usize, usize, Vec<f64>)> = BTreeMap::new();
    for r in solo_rows {
        let Some(task) = r["task"].as_str() else {
            continue;
        };
        let entry = solo_by_task
            .entry(task.to_owned())
            .or_insert_with(|| (0, 0, Vec::new()));
        entry.0 += usize::from(r["pass"] == true);
        entry.1 += 1;
        entry.2.push(run_cost(r));
    }

    let mut team_by_task: BTreeMap<String, (usize, usize, Vec<f64>)> = BTreeMap::new();
    for r in team_rows {
        let Some(task) = r["task"].as_str() else {
            continue;
        };
        let entry = team_by_task
            .entry(task.to_owned())
            .or_insert_with(|| (0, 0, Vec::new()));
        entry.0 += usize::from(r["pass"] == true);
        entry.1 += 1;
        entry.2.push(run_cost(r));
    }

    let shared_tasks: Vec<String> = solo_by_task
        .keys()
        .filter(|k| team_by_task.contains_key(*k))
        .cloned()
        .collect();
    if shared_tasks.is_empty() {
        return Err("no overlapping `task` IDs between solo and team JSONL files".into());
    }

    let solo_costs: Vec<f64> = shared_tasks
        .iter()
        .flat_map(|t| solo_by_task[t].2.iter().copied())
        .collect();
    let team_costs: Vec<f64> = shared_tasks
        .iter()
        .flat_map(|t| team_by_task[t].2.iter().copied())
        .collect();
    let solo_unit = solo_costs.iter().sum::<f64>() / (solo_costs.len().max(1) as f64);
    let team_unit = team_costs.iter().sum::<f64>() / (team_costs.len().max(1) as f64);
    let nb = (team_unit / solo_unit.max(1e-6)).max(1.0);

    let mut sum_p1 = 0.0f64;
    let mut sum_vote = 0.0f64;
    let mut sum_pass_nb = 0.0f64;
    let mut sum_team = 0.0f64;
    let mut wins = 0usize;
    let mut losses = 0usize;
    let mut ties = 0usize;

    for t in &shared_tasks {
        let (sc, sn, _) = &solo_by_task[t];
        let (tc, tn, _) = &team_by_task[t];
        let p1 = *sc as f64 / (*sn as f64);
        let pt = *tc as f64 / (*tn as f64);
        let p_nb = pass_at_nb_task(*sn, *sc, nb);
        sum_p1 += p1;
        sum_vote += majority_vote_prob(p1, nb);
        sum_pass_nb += p_nb;
        sum_team += pt;
        if pt > p_nb + 1e-9 {
            wins += 1;
        } else if pt + 1e-9 < p_nb {
            losses += 1;
        } else {
            ties += 1;
        }
    }

    let k = shared_tasks.len() as f64;
    let p_single = sum_p1 / k;
    let v_majority = sum_vote / k;
    let pass_at_nb = sum_pass_nb / k;
    let p_team = sum_team / k;
    let eps = 1e-6;
    let (level, label) = if p_team > pass_at_nb + eps {
        (3, "Level 3 (Emergence > pass@n_B)")
    } else if p_team > v_majority + eps {
        (2, "Level 2 (Beats Equal-Budget Majority Vote)")
    } else if p_team > p_single + eps {
        (1, "Level 1 (Beats Single Agent)")
    } else {
        (0, "Level 0 (Below Single Agent — Coordination Tax)")
    };

    // Equivalent independent-agent budget `n_eq` required for `1 - (1 - P_1)^n_eq == P_team`.
    let equiv_solo = if p_single <= 1e-6 {
        if p_team > 1e-6 { nb * 2.0 } else { 1.0 }
    } else if p_single >= 1.0 - 1e-6 {
        1.0
    } else {
        let clamped_team = p_team.clamp(0.0, 0.995);
        ((1.0 - clamped_team).ln() / (1.0 - p_single).ln()).max(0.5)
    };
    let saving_pct = if equiv_solo > 1e-6 {
        ((equiv_solo - nb) / equiv_solo) * 100.0
    } else {
        0.0
    };

    let round4 = |x: f64| (x * 10_000.0).round() / 10_000.0;
    Ok(LadderReport {
        tasks: shared_tasks.len(),
        solo_runs: solo_costs.len(),
        team_runs: team_costs.len(),
        solo_unit_tokens: (solo_unit * 10.0).round() / 10.0,
        team_unit_tokens: (team_unit * 10.0).round() / 10.0,
        budget_multiple_nb: round4(nb),
        p_single: round4(p_single),
        v_majority: round4(v_majority),
        pass_at_nb: round4(pass_at_nb),
        p_team: round4(p_team),
        margin_over_vote: round4(p_team - v_majority),
        margin_over_pass_at_nb: round4(p_team - pass_at_nb),
        equiv_solo_budget: round4(equiv_solo),
        token_saving_pct: (saving_pct * 10.0).round() / 10.0,
        level,
        level_label: label.to_owned(),
        task_wins: wins,
        task_losses: losses,
        task_ties: ties,
        sign_test_p: round4(exact_sign_test_p(wins, losses)),
    })
}

pub fn format_ladder_table(r: &LadderReport) -> String {
    format!(
        "RCI Equal-Budget Ladder ({} paired tasks · solo={} runs, team={} runs)\n\
         ──────────────────────────────────────────────────────────────────────\n\
           Unit budget      : solo {:.1} tok  |  team {:.1} tok  (n_B = {:.2}x)\n\
           P(1 solo agent)  : {:.1}%\n\
           V(n_B) maj. vote : {:.1}%   (team margin: {:+.1} pts)\n\
           pass@n_B ceiling : {:.1}%   (team margin: {:+.1} pts)\n\
           P(team)          : {:.1}%   -> {}\n\
           Equiv solo n_eq  : {:.2}x   (effective budget delta: {:+.1}%)\n\
           Paired tasks     : {}W / {}L / {}T vs pass@n_B (sign-test p = {:.4})\n",
        r.tasks,
        r.solo_runs,
        r.team_runs,
        r.solo_unit_tokens,
        r.team_unit_tokens,
        r.budget_multiple_nb,
        r.p_single * 100.0,
        r.v_majority * 100.0,
        r.margin_over_vote * 100.0,
        r.pass_at_nb * 100.0,
        r.margin_over_pass_at_nb * 100.0,
        r.p_team * 100.0,
        r.level_label,
        r.equiv_solo_budget,
        -r.token_saving_pct,
        r.task_wins,
        r.task_losses,
        r.task_ties,
        r.sign_test_p
    )
}

fn read_jsonl_rows(path: &Path) -> Result<Vec<Value>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut rows = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            rows.push(v);
        }
    }
    Ok(rows)
}

/// Label-free harvest over `<sessions_dir>/*.jsonl`: keeps ONLY hash-chain-verified
/// sessions where automatic verification passed (`verify.passed` or `verify.completed(passed=true)`),
/// attributing credit across subagent roles by downstream fact reuse and patch/review impact.
pub fn harvest_sessions(sessions_dir: &Path) -> Result<HarvestReport, String> {
    let mut scanned = 0usize;
    let mut valid_chains = 0usize;
    let mut verified_collective = 0usize;
    let mut credits: BTreeMap<String, RoleCredit> = ROLES
        .iter()
        .map(|&r| (r.to_owned(), RoleCredit::default()))
        .collect();
    let mut sample_facts = Vec::new();

    let entries = std::fs::read_dir(sessions_dir)
        .map_err(|e| format!("read sessions dir {}: {e}", sessions_dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        scanned += 1;
        let Ok(rows) = Record::verify_rows(&path) else {
            continue;
        };
        valid_chains += 1;
        let auto_verified = rows.iter().any(|r| {
            let kind = r["kind"].as_str().unwrap_or("");
            kind == "verify.passed"
                || (kind == "verify.completed" && r["verdict"]["passed"] == true)
        });
        let sub_results: Vec<&Value> = rows
            .iter()
            .filter(|r| r["kind"] == "tool.result" && r["tool"] == "subagent")
            .collect();
        if !auto_verified || sub_results.is_empty() {
            continue;
        }
        verified_collective += 1;

        // Collect final assistant & applied patch text to measure downstream fact survival (Shapley proxy).
        let downstream_corpus: String = rows
            .iter()
            .filter(|r| r["kind"] == "message" && r["message"]["role"] == "assistant")
            .filter_map(|r| r["message"]["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");

        for sr in sub_results {
            let role = sr["args"]["role"]
                .as_str()
                .filter(|r| ROLES.contains(r))
                .unwrap_or("scout")
                .to_owned();
            let slice = sr["args"]["slice"].as_str();
            let out_txt = sr["output"].as_str().unwrap_or("");
            let entry = credits.entry(role.clone()).or_default();
            entry.invocations += 1;
            entry.verified_sessions += 1;

            if out_txt.starts_with("subagent error:") || out_txt.contains("\"timed_out\": true") {
                entry.timeouts_or_failures += 1;
                entry.credit_score -= 0.5;
                continue;
            }
            let parsed_out = serde_json::from_str::<Value>(out_txt).ok();
            let inner_text = parsed_out
                .as_ref()
                .and_then(|v| v.pointer("/result/text"))
                .and_then(Value::as_str)
                .unwrap_or(out_txt);

            let facts = extract_decisive_facts(&role, slice, inner_text);
            let mut reused = 0usize;
            for f in &facts {
                if sample_facts.len() < 12 && !sample_facts.contains(f) {
                    sample_facts.push(f.clone());
                }
                // Check if file path token in the fact was cited in downstream assistant synthesis.
                let cited = f
                    .split_whitespace()
                    .find(|tok| tok.contains(':') && tok.contains('.'))
                    .and_then(|loc| loc.split(':').next())
                    .is_some_and(|file_part| downstream_corpus.contains(file_part));
                if cited {
                    reused += 1;
                }
            }
            entry.fact_citations += facts.len();
            if parsed_out
                .as_ref()
                .and_then(|v| v.get("patch_artifact"))
                .is_some_and(|p| !p.is_null())
            {
                entry.patches_produced += 1;
                entry.credit_score += 1.5;
            }
            let blockers = parsed_out
                .as_ref()
                .and_then(|v| v.get("blockers"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if blockers > 0 {
                entry.blockers_caught += blockers;
                entry.credit_score += 1.25 * (blockers as f64);
            }
            entry.credit_score += 0.4 * (facts.len() as f64) + 0.8 * (reused as f64);
        }
    }

    for c in credits.values_mut() {
        c.credit_score = (c.credit_score * 100.0).round() / 100.0;
    }

    Ok(HarvestReport {
        scanned_files: scanned,
        chain_valid_files: valid_chains,
        verified_collective_sessions: verified_collective,
        role_credits: credits,
        sample_facts,
    })
}

/// Distill compact `.twig/notes/<role>.md` protocol cards from verified collective harvest
/// (uses `plan` LLM when `online == true` and verified sessions exist, with deterministic
/// RCI baseline fallback and strict anti-overfit validation).
pub fn distill_role_notes(
    repo: &Path,
    sessions_dir: &Path,
    online: bool,
) -> Result<(HarvestReport, Vec<String>), String> {
    let harvest = harvest_sessions(sessions_dir)?;
    let notes_dir = repo.join(".twig").join("notes");
    std::fs::create_dir_all(&notes_dir).map_err(|e| format!("create .twig/notes: {e}"))?;

    // Collect repo-specific file stems (`foo_bar`) to block task memorization inside protocol cards!
    let mut banned = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(repo.join("src")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if let Some((stem, _)) = name.rsplit_once('.') {
                if stem.len() >= 5 && stem != "mod" && stem != "lib" && stem != "main" {
                    banned.insert(stem.to_owned());
                }
            }
        }
    }
    let banned_vec: Vec<String> = banned.into_iter().collect();

    let mut proposed: BTreeMap<String, String> = BTreeMap::new();
    if online && harvest.verified_collective_sessions > 0 {
        let provider = Provider::new();
        let model = provider.roles().get("plan").to_owned();
        let credit_json = serde_json::to_string(&harvest.role_credits).unwrap_or_default();
        let prompt = format!(
            "Distill generalizable multi-agent coordination role cards (<= 600 chars per role) from this label-free verified telemetry: {credit_json}.\n\
             Enforce RCI principles: disjoint slice discipline, verbatim `path:line` fact-forwarding, zero shared-context debate.\n\
             NEVER mention repository-specific filenames or identifiers.\n\
             Return JSON object mapping role (`scout`,`code`,`test`,`advisor`,`reviewer`) to markdown bullets."
        );
        if let Ok(reply) = provider.complete(
            &model,
            "You author concise, domain-agnostic multi-agent protocol cards. JSON only.",
            &[json!({"role": "user", "content": prompt})],
            None,
        ) {
            let trimmed = reply.text.trim();
            if let (Some(s), Some(e)) = (trimmed.find('{'), trimmed.rfind('}')) {
                if let Ok(obj) = serde_json::from_str::<Value>(&trimmed[s..=e]) {
                    for &role in ROLES {
                        if let Some(card) = obj.get(role).and_then(Value::as_str) {
                            if validate_role_note(card.trim(), &banned_vec).is_ok() {
                                proposed.insert(role.to_owned(), card.trim().to_owned());
                            }
                        }
                    }
                }
            }
        }
    }

    let mut written = Vec::new();
    for &role in ROLES {
        let body = proposed
            .remove(role)
            .unwrap_or_else(|| baseline_role_note(role).to_owned());
        validate_role_note(&body, &banned_vec)?;
        let dst = notes_dir.join(format!("{role}.md"));
        std::fs::write(&dst, format!("# RCI Role Protocol: {role}\n\n{body}\n"))
            .map_err(|e| format!("write {}: {e}", dst.display()))?;
        written.push(format!(".twig/notes/{role}.md"));
    }

    let state_path = notes_dir.join("state.json");
    let prev_gen = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["generation"].as_u64())
        .unwrap_or(0);
    let meta = json!({
        "generation": prev_gen,
        "candidate_ready": true,
        "verified_sessions_harvested": harvest.verified_collective_sessions,
        "roles": written,
    });
    let _ = std::fs::write(
        &state_path,
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );

    Ok((harvest, written))
}

/// Gated recursion release check (`generation g -> g + 1`):
/// Promotes candidate collective run over previous generation ONLY if:
/// 1. Zero task-level regressions (`cand_pass_rate >= prev_pass_rate` on every task that previously passed 100%), AND
/// 2. Strictly higher overall accuracy (`cand > prev`) OR equal accuracy with `>= 15%` lower `n_B` token budget!
pub fn gate_generation(
    repo: &Path,
    solo_rows: &[Value],
    prev_team_rows: &[Value],
    cand_team_rows: &[Value],
) -> Result<GateVerdict, String> {
    let prev_ladder = grade_ladder(solo_rows, prev_team_rows)?;
    let cand_ladder = grade_ladder(solo_rows, cand_team_rows)?;

    let task_rates = |rows: &[Value]| -> BTreeMap<String, f64> {
        let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for r in rows {
            if let Some(t) = r["task"].as_str() {
                let e = counts.entry(t.to_owned()).or_default();
                e.0 += usize::from(r["pass"] == true);
                e.1 += 1;
            }
        }
        counts
            .into_iter()
            .filter(|(_, (_, n))| *n > 0)
            .map(|(k, (p, n))| (k, p as f64 / n as f64))
            .collect()
    };
    let prev_map = task_rates(prev_team_rows);
    let cand_map = task_rates(cand_team_rows);
    let mut regressions = Vec::new();
    for (task, &prev_r) in &prev_map {
        let cand_r = cand_map.get(task).copied().unwrap_or(0.0);
        if prev_r >= 1.0 - 1e-6 && cand_r < prev_r - 1e-6 {
            regressions.push(task.clone());
        }
    }

    let state_path = repo.join(".twig").join("notes").join("state.json");
    let cur_gen = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["generation"].as_u64())
        .unwrap_or(0) as u32;

    let acc_gain = cand_ladder.p_team > prev_ladder.p_team + 1e-6
        && cand_ladder.budget_multiple_nb <= prev_ladder.budget_multiple_nb * 1.15;
    let budget_gain = (cand_ladder.p_team - prev_ladder.p_team).abs() <= 1e-6
        && cand_ladder.budget_multiple_nb <= prev_ladder.budget_multiple_nb * 0.85;

    let (promoted, next_gen, reason) = if !regressions.is_empty() {
        (
            false,
            cur_gen,
            format!(
                "blocked by release gate: regressed on {} previously solved task(s): {}",
                regressions.len(),
                regressions.join(", ")
            ),
        )
    } else if acc_gain {
        (
            true,
            cur_gen + 1,
            format!(
                "promoted gen {} -> {}: accuracy {:.1}% -> {:.1}% (Ladder L{} -> L{}, n_B {:.2}x -> {:.2}x)",
                cur_gen,
                cur_gen + 1,
                prev_ladder.p_team * 100.0,
                cand_ladder.p_team * 100.0,
                prev_ladder.level,
                cand_ladder.level,
                prev_ladder.budget_multiple_nb,
                cand_ladder.budget_multiple_nb
            ),
        )
    } else if budget_gain {
        (
            true,
            cur_gen + 1,
            format!(
                "promoted gen {} -> {}: accuracy held at {:.1}% while budget dropped n_B {:.2}x -> {:.2}x",
                cur_gen,
                cur_gen + 1,
                cand_ladder.p_team * 100.0,
                prev_ladder.budget_multiple_nb,
                cand_ladder.budget_multiple_nb
            ),
        )
    } else {
        (
            false,
            cur_gen,
            format!(
                "held at gen {}: accuracy {:.1}% vs {:.1}%, budget n_B {:.2}x vs {:.2}x did not clear release gate",
                cur_gen,
                cand_ladder.p_team * 100.0,
                prev_ladder.p_team * 100.0,
                cand_ladder.budget_multiple_nb,
                prev_ladder.budget_multiple_nb
            ),
        )
    };

    if promoted {
        let _ = std::fs::create_dir_all(repo.join(".twig").join("notes"));
        let _ = std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&json!({
                "generation": next_gen,
                "candidate_ready": false,
                "accuracy": cand_ladder.p_team,
                "ladder_level": cand_ladder.level,
                "budget_multiple_nb": cand_ladder.budget_multiple_nb,
            }))
            .unwrap_or_default(),
        );
    }

    Ok(GateVerdict {
        promoted,
        generation: next_gen,
        reason,
        prev_accuracy: prev_ladder.p_team,
        cand_accuracy: cand_ladder.p_team,
        prev_nb: prev_ladder.budget_multiple_nb,
        cand_nb: cand_ladder.budget_multiple_nb,
        regressions,
    })
}

/// Unified CLI handler for `twig ladder ...` and `twig rci ...`.
pub fn run_cli(cmd: &str, args: &[String]) -> Result<String, String> {
    let has_json = args.iter().any(|a| a == "--json");
    let pos: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .map(String::as_str)
        .collect();

    if cmd == "ladder" {
        if pos.len() < 2 {
            return Err(
                "usage: twig ladder <solo_results.jsonl> <team_results.jsonl> [--json]".into(),
            );
        }
        let solo = read_jsonl_rows(Path::new(pos[0]))?;
        let team = read_jsonl_rows(Path::new(pos[1]))?;
        let report = grade_ladder(&solo, &team)?;
        return if has_json {
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())
        } else {
            Ok(format_ladder_table(&report))
        };
    }

    let sub = pos.first().copied().unwrap_or("status");
    let repo_flag = args
        .iter()
        .position(|a| a == "--repo")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let sessions_dir = paths::state_dir().join("sessions");

    match sub {
        "status" => Ok(format!("{}\n", status_report(&repo_flag))),
        "harvest" => {
            let dir = pos
                .get(1)
                .map(PathBuf::from)
                .unwrap_or(sessions_dir);
            let rep = harvest_sessions(&dir)?;
            serde_json::to_string_pretty(&rep).map_err(|e| e.to_string())
        }
        "distill" => {
            let online = args.iter().any(|a| a == "--online");
            let dir = pos
                .get(1)
                .map(PathBuf::from)
                .unwrap_or(sessions_dir);
            let (harvest, files) = distill_role_notes(&repo_flag, &dir, online)?;
            if has_json {
                Ok(json!({"harvest": harvest, "written": files}).to_string())
            } else {
                Ok(format!(
                    "Distilled {} RCI role protocol cards ({} verified collective sessions harvested):\n  {}\n",
                    files.len(),
                    harvest.verified_collective_sessions,
                    files.join("\n  ")
                ))
            }
        }
        "gate" => {
            if pos.len() < 4 {
                return Err(
                    "usage: twig rci gate <solo.jsonl> <prev_team.jsonl> <cand_team.jsonl> [--repo DIR] [--json]"
                        .into(),
                );
            }
            let solo = read_jsonl_rows(Path::new(pos[1]))?;
            let prev = read_jsonl_rows(Path::new(pos[2]))?;
            let cand = read_jsonl_rows(Path::new(pos[3]))?;
            let verdict = gate_generation(&repo_flag, &solo, &prev, &cand)?;
            if has_json {
                serde_json::to_string_pretty(&verdict).map_err(|e| e.to_string())
            } else {
                Ok(format!("{}\n", verdict.reason))
            }
        }
        _ => Err(
            "usage: twig rci <status | harvest [sessions_dir] | distill [sessions_dir] [--online] | gate <solo> <prev> <cand>> [--repo DIR] [--json]"
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::blackboard::load_role_note;

    #[test]
    fn equal_budget_ladder_distinguishes_debate_tax_from_level3_emergence() {
        // Solo baseline: 4 tasks, 4 reps each, 50% accuracy (2/4 per task), 1000 completion tokens each.
        let mut solo = Vec::new();
        for t in ["t1", "t2", "t3", "t4"] {
            for rep in 0..4 {
                solo.push(json!({
                    "task": t,
                    "rep": rep,
                    "pass": rep < 2,
                    "usage": {"output_tokens": 1000}
                }));
            }
        }

        // Case A: Shared-info debate spends 3x tokens (`n_B = 3`) and gets 62.5% -> beats P(1)=50% & V(3)=50%,
        // but stays BELOW combinatorial pass@3 (1 - C(2,3)/C(4,3) = 1.0) -> Level 2, NOT Level 3!
        let mut debate = Vec::new();
        for (idx, t) in ["t1", "t2", "t3", "t4"].iter().enumerate() {
            debate.push(json!({
                "task": t,
                "rep": 0,
                "pass": idx < 3,
                "usage": {"output_tokens": 3000}
            }));
        }
        let r_debate = grade_ladder(&solo, &debate).unwrap();
        assert_eq!(r_debate.budget_multiple_nb, 3.0);
        assert_eq!(r_debate.level, 2);

        // Case B: Split-fact blackboard team spends 2x tokens (`n_B = 2`) where pass@2 = 1 - C(2,2)/C(4,2) = 5/6 = 0.8333,
        // and achieves 100% (4/4) -> genuinely exceeds pass@n_B -> Level 3 Emergence!
        let mut split_team = Vec::new();
        for t in ["t1", "t2", "t3", "t4"] {
            split_team.push(json!({
                "task": t,
                "rep": 0,
                "pass": true,
                "usage": {"output_tokens": 2000}
            }));
        }
        let r_emergent = grade_ladder(&solo, &split_team).unwrap();
        assert_eq!(r_emergent.level, 3);
        assert!(r_emergent.margin_over_pass_at_nb > 0.15);
        assert!(r_emergent.equiv_solo_budget > r_emergent.budget_multiple_nb);

        // Release gate promotes split_team over debate, and blocks if a 100% task regresses!
        let tmp = std::env::temp_dir().join(format!("twig-rci-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let gate_ok = gate_generation(&tmp, &solo, &debate, &split_team).unwrap();
        assert!(gate_ok.promoted);
        assert_eq!(gate_ok.generation, 1);

        let mut regressed = split_team.clone();
        regressed[0]["pass"] = json!(false);
        let gate_bad = gate_generation(&tmp, &solo, &split_team, &regressed).unwrap();
        assert!(!gate_bad.promoted);
        assert_eq!(gate_bad.regressions, vec!["t1".to_owned()]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn label_free_harvest_requires_valid_chain_and_auto_verification_and_distills_cards() {
        let dir = std::env::temp_dir().join(format!("twig-rci-harvest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sess_dir = dir.join("sessions");
        let repo_dir = dir.join("repo");
        std::fs::create_dir_all(&sess_dir).unwrap();
        std::fs::create_dir_all(repo_dir.join("src")).unwrap();
        std::fs::write(repo_dir.join("src/secret_parser.rs"), "fn x() {}\n").unwrap();

        // 1. Unverified session with subagent MUST be ignored by label-free filter.
        {
            let mut rec = Record::open(sess_dir.join("unverified.jsonl"), "unverified").unwrap();
            rec.append(
                "tool.result",
                json!({"id": "s1", "tool": "subagent", "args": {"role": "scout"}, "output": "[Fact] src/a.rs:10 bug"}),
            )
            .unwrap();
        }

        // 2. Verified collective session IS harvested and attributes credit to scout + reviewer.
        {
            let mut rec = Record::open(sess_dir.join("verified.jsonl"), "verified").unwrap();
            let sub_json = json!({
                "result": {"text": "Facts:\n- src/engine.rs:42 off-by-one bounds check"}
            });
            rec.append(
                "tool.result",
                json!({
                    "id": "s2",
                    "tool": "subagent",
                    "args": {"role": "scout", "slice": "src/engine.rs"},
                    "output": sub_json.to_string()
                }),
            )
            .unwrap();
            rec.append(
                "message",
                json!({"message": {"role": "assistant", "content": "Fixed bounds check at src/engine.rs:42."}}),
            )
            .unwrap();
            rec.append("verify.passed", json!({"verdict": {"ok": true}}))
                .unwrap();
        }

        let (harvest, files) = distill_role_notes(&repo_dir, &sess_dir, false).unwrap();
        assert_eq!(harvest.scanned_files, 2);
        assert_eq!(harvest.verified_collective_sessions, 1);
        assert!(harvest.role_credits["scout"].credit_score > 1.0);
        assert_eq!(files.len(), 5);
        assert!(load_role_note(&repo_dir, "scout").unwrap().contains("slice"));

        // Anti-overfit validator blocks leaking repo-specific stem `secret_parser`.
        assert!(
            validate_role_note("Always check secret_parser first", &["secret_parser".into()])
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
