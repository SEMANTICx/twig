//! Differential verification: the previous behaviour is the oracle.
//!
//! You do not write expected outputs. You record real invocations against the
//! old implementation, replay them against the new one, and compare exactly.
//! Floats compare within 1e-9; wherever the original raised, the candidate must
//! raise the same class of error.
//!
//! The candidate is a *program*, not a closure: JSON on stdin, JSON on stdout.
//! That keeps the verifier language-agnostic, so the thing being verified can be
//! Rust, Python or anything else that speaks JSON.
//!
//! Two known ceilings, both enforced by `gate` rather than hidden:
//!   1. derived edge cases cover the value domain, not control flow — a branch
//!      the recording never took is not verified, so branch coverage is a gate
//!   2. a correct port that is slower per call is not worth enabling — cost is a
//!      gate too

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub program: String,
    pub args: Vec<String>,
}

impl Candidate {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self { program: program.into(), args }
    }

    /// JSON in, JSON out. A non-zero exit is the candidate's error channel.
    pub fn run(&self, input: &Value) -> Result<Value, String> {
        let mut cmd=Command::new(&self.program);
        cmd.args(&self.args);
        let out=crate::execution::run_process(&mut cmd,input.to_string().as_bytes(),
            std::time::Duration::from_secs(120),1_048_576).map_err(|e|format!("spawn {}: {e}",self.program))?;
        if out.timed_out {return Err("timeout: differential candidate exceeded 120s".into());}
        if out.truncated {return Err("candidate output exceeds 1 MiB".into());}
        if out.exit_code!=Some(0) {return Err(format!("exit {}: {}",out.exit_code.unwrap_or(-1),String::from_utf8_lossy(&out.stderr)));}
        serde_json::from_slice(&out.stdout).map_err(|e|format!("json: {e}"))
    }
}

/// Structural equality. Floats compare within `tol`; NaN equals NaN; a type
/// difference is a failure even when the values look similar.
pub fn eq_json(a: &Value, b: &Value, tol: f64) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => {
                if x.is_nan() && y.is_nan() {
                    true
                } else if x.is_nan() || y.is_nan() {
                    false
                } else {
                    (x - y).abs() <= tol
                }
            }
            _ => x == y,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| eq_json(p, q, tol))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| eq_json(v, w, tol)))
        }
        _ => a == b,
    }
}

/// Value-domain edge cases the recording is unlikely to contain.
pub fn derive_edges(seed: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = seed.to_vec();
    for v in seed {
        if let Some(n) = v.as_f64() {
            if let Some(num) = serde_json::Number::from_f64(-n) {
                out.push(Value::Number(num));
            }
            if let Some(num) = serde_json::Number::from_f64(n + 1.0) {
                out.push(Value::Number(num));
            }
            if let Some(num) = serde_json::Number::from_f64(n * 2.0) {
                out.push(Value::Number(num));
            }
        }
    }
    out.extend([
        json!(0),
        json!(-1),
        json!(1),
        json!(""),
        json!("üñï"),
        json!("\u{0}"),
        json!([]),
        json!({}),
        json!(null),
    ]);
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct Fail {
    pub input: Value,
    pub want: String,
    pub got: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub ok: bool,
    pub covered: usize,
    pub total: usize,
    pub fails: Vec<Fail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

pub fn record(path: &Path, cases: &[Case]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    for case in cases {
        body.push_str(&serde_json::to_string(case)?);
        body.push('\n');
    }
    std::fs::write(path, body)
}

pub fn load(path: &Path) -> std::io::Result<Vec<Case>> {
    let text = std::fs::read_to_string(path)?;
    let mut cases = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        cases.push(serde_json::from_str(line)?);
    }
    Ok(cases)
}

fn error_class(message: &str) -> &str {
    message.split(':').next().unwrap_or(message).trim()
}

pub fn verify(path: &Path, candidate: &Candidate, min_cases: usize) -> Verdict {
    let cases = match load(path) {
        Ok(c) => c,
        Err(e) => {
            return Verdict {
                ok: false,
                covered: 0,
                total: 0,
                fails: vec![],
                why: Some(format!("journal: {e}")),
            }
        }
    };
    let mut fails = Vec::new();
    let mut covered = 0usize;
    for case in &cases {
        let result = candidate.run(&case.input);
        match (&case.error, result) {
            (Some(want_err), Err(got_err)) => {
                if error_class(&got_err) == error_class(want_err) {
                    covered += 1;
                } else {
                    fails.push(Fail {
                        input: case.input.clone(),
                        want: want_err.clone(),
                        got: got_err,
                    });
                }
            }
            (Some(want_err), Ok(got)) => fails.push(Fail {
                input: case.input.clone(),
                want: want_err.clone(),
                got: got.to_string(),
            }),
            (None, Err(got_err)) => fails.push(Fail {
                input: case.input.clone(),
                want: case.output.as_ref().map(|o| o.to_string()).unwrap_or_default(),
                got: got_err,
            }),
            (None, Ok(got)) => {
                let want = case.output.clone().unwrap_or(Value::Null);
                if eq_json(&want, &got, 1e-9) {
                    covered += 1;
                } else {
                    fails.push(Fail {
                        input: case.input.clone(),
                        want: want.to_string(),
                        got: got.to_string(),
                    });
                }
            }
        }
    }
    let ok = fails.is_empty() && covered >= min_cases;
    let why = if ok {
        None
    } else if !fails.is_empty() {
        Some(format!("{} mismatching case(s)", fails.len()))
    } else {
        Some(format!("covered {covered} < {min_cases} required"))
    };
    Verdict {
        ok,
        covered,
        total: cases.len(),
        fails: fails.into_iter().take(5).collect(),
        why,
    }
}

/// Full admission gate: replay must pass, control flow must be covered, and the
/// candidate must not be slower per call than what it replaces.
pub fn gate(
    path: &Path,
    candidate: &Candidate,
    min_cases: usize,
    branch_coverage: Option<f64>,
    min_branch: f64,
    cost_ratio: Option<f64>,
) -> Verdict {
    let mut verdict = verify(path, candidate, min_cases);
    if let Some(cov) = branch_coverage {
        if cov < min_branch {
            verdict.ok = false;
            verdict.why = Some(format!(
                "branch coverage {:.0}% < {:.0}%",
                cov * 100.0,
                min_branch * 100.0
            ));
        }
    }
    if let Some(ratio) = cost_ratio {
        if ratio > 1.0 {
            verdict.ok = false;
            verdict.why = Some(format!("cost ratio {ratio:.2}x > 1.00x: correct but slower"));
        }
    }
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(cases: &[(&str, &str)]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "twig-verify-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cases: Vec<Case> = cases
            .iter()
            .map(|(input, output)| Case {
                input: serde_json::from_str(input).unwrap(),
                output: Some(serde_json::from_str(output).unwrap()),
                error: None,
            })
            .collect();
        record(&path, &cases).unwrap();
        path
    }

    /// `cat` is an identity candidate: JSON in, same JSON out. Perfect for
    /// testing the plumbing without inventing a language.
    fn identity() -> Candidate {
        Candidate::new("cat", vec![])
    }

    #[test]
    fn faithful_candidate_passes() {
        let path = journal(&[("{\"a\":1}", "{\"a\":1}")]);
        let v = verify(&path, &identity(), 1);
        assert!(v.ok, "{v:?}");
        assert_eq!(v.covered, 1);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn wrong_value_fails() {
        let path = journal(&[("{\"a\":1}", "{\"a\":2}")]);
        let v = verify(&path, &identity(), 1);
        assert!(!v.ok);
        assert_eq!(v.fails.len(), 1);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn missing_candidate_fails_cleanly() {
        let path = journal(&[("{\"a\":1}", "{\"a\":1}")]);
        let v = verify(&path, &Candidate::new("definitely-not-a-binary", vec![]), 1);
        assert!(!v.ok);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn float_tolerance() {
        assert!(eq_json(&json!(0.30000000000000004), &json!(0.3), 1e-9));
        assert!(!eq_json(&json!(0.3), &json!(0.31), 1e-9));
    }

    #[test]
    fn type_difference_is_a_failure() {
        assert!(!eq_json(&json!("1"), &json!(1), 1e-9));
        assert!(!eq_json(&json!([1]), &json!(1), 1e-9));
    }

    #[test]
    fn branch_gate_blocks() {
        let path = journal(&[("{\"a\":1}", "{\"a\":1}")]);
        let v = gate(&path, &identity(), 1, Some(0.4), 0.8, None);
        assert!(!v.ok);
        assert!(v.why.unwrap().contains("branch coverage"));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn cost_gate_blocks_slower_correct_ports() {
        let path = journal(&[("{\"a\":1}", "{\"a\":1}")]);
        let v = gate(&path, &identity(), 1, None, 0.0, Some(1.8));
        assert!(!v.ok, "correct but slower must not be enabled");
        assert!(v.why.unwrap().contains("slower"));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn edges_include_the_value_traps() {
        let edges = derive_edges(&[json!(7)]);
        let text: Vec<String> = edges.iter().map(|v| v.to_string()).collect();
        assert!(text.contains(&"0".to_string()));
        assert!(text.contains(&"null".to_string()));
        assert!(text.iter().any(|t| t.contains("üñï")));
    }
}
