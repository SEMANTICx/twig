//! Differential verification: the previous behaviour is the oracle.
//!
//! Covers both recorded journal replay (`Candidate` / `verify` / `gate`) and
//! JSON-lines differential suites (`run_differential`). Depends strictly
//! downward on `crate::exec`.

use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::exec::run_process;

const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_CANDIDATE_TIMEOUT: Duration = Duration::from_secs(120);

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
    timeout: Duration,
    raw_cli: bool,
    ignore_fields: Vec<String>,
}

impl Candidate {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            timeout: DEFAULT_CANDIDATE_TIMEOUT,
            raw_cli: false,
            ignore_fields: Vec::new(),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_millis(1));
        self
    }

    pub fn with_raw_cli(mut self, raw: bool) -> Self {
        self.raw_cli = raw;
        self
    }

    pub fn with_ignore_fields(mut self, fields: Vec<String>) -> Self {
        self.ignore_fields = fields;
        self
    }

    /// JSON in, JSON out by default; when `raw_cli` is enabled or `input` carries
    /// `{"_cli": {"args": [...], "stdin": "..."}}`, captures `{exit_code, stdout, stderr}`.
    pub fn run(&self, input: &Value) -> Result<Value, String> {
        let cli_env = input.get("_cli");
        let use_raw = self.raw_cli || cli_env.is_some();
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        let stdin_bytes: Vec<u8> = if let Some(spec) = cli_env {
            if let Some(extra) = spec.get("args").and_then(Value::as_array) {
                for a in extra.iter().filter_map(Value::as_str) {
                    cmd.arg(a);
                }
            }
            spec.get("stdin")
                .and_then(Value::as_str)
                .unwrap_or("")
                .as_bytes()
                .to_vec()
        } else if use_raw {
            match input {
                Value::Null => Vec::new(),
                Value::String(s) => s.as_bytes().to_vec(),
                other => other.to_string().into_bytes(),
            }
        } else {
            input.to_string().into_bytes()
        };

        let out = run_process(&mut cmd, &stdin_bytes, self.timeout, 1_048_576)
            .map_err(|e| format!("spawn {}: {e}", self.program))?;
        if out.timed_out {
            return Err(format!(
                "timeout: differential candidate exceeded {}s",
                self.timeout.as_secs()
            ));
        }
        if out.truncated {
            return Err("candidate output exceeds 1 MiB".into());
        }
        if use_raw {
            return Ok(json!({
                "exit_code": out.exit_code.unwrap_or(-1),
                "stdout": String::from_utf8_lossy(&out.stdout),
                "stderr": String::from_utf8_lossy(&out.stderr),
            }));
        }
        if out.exit_code != Some(0) {
            return Err(format!(
                "exit {}: {}",
                out.exit_code.unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        serde_json::from_slice(&out.stdout).map_err(|e| format!("json: {e}"))
    }
}

fn strip_ignored_keys(val: &Value, ignored: &[String]) -> Value {
    if ignored.is_empty() {
        return val.clone();
    }
    match val {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if !ignored.iter().any(|ig| ig == k) {
                    out.insert(k.clone(), strip_ignored_keys(v, ignored));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(|v| strip_ignored_keys(v, ignored)).collect()),
        other => other.clone(),
    }
}

/// Dynamically run an oracle program against `inputs` and save its black-box behavior as a journal.
pub fn record_oracle(path: &Path, oracle: &Candidate, inputs: &[Value]) -> io::Result<usize> {
    let mut cases = Vec::with_capacity(inputs.len());
    for inp in inputs {
        match oracle.run(inp) {
            Ok(out) => cases.push(Case {
                input: inp.clone(),
                output: Some(out),
                error: None,
            }),
            Err(err) => cases.push(Case {
                input: inp.clone(),
                output: None,
                error: Some(err),
            }),
        }
    }
    record(path, &cases)?;
    Ok(cases.len())
}

/// Structural equality. Exact integers compare without 53-bit `f64` mantissa loss;
/// floats compare within `tol`; NaN equals NaN; type differences fail.
pub fn eq_json(a: &Value, b: &Value, tol: f64) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let x_int = x
                .as_i64()
                .map(i128::from)
                .or_else(|| x.as_u64().map(i128::from));
            let y_int = y
                .as_i64()
                .map(i128::from)
                .or_else(|| y.as_u64().map(i128::from));
            if let (Some(xi), Some(yi)) = (x_int, y_int) {
                return if tol >= 1.0 {
                    xi.abs_diff(yi) <= (tol as u128)
                } else {
                    xi == yi
                };
            }
            match (x.as_f64(), y.as_f64()) {
                (Some(xf), Some(yf)) => {
                    if xf.is_nan() && yf.is_nan() {
                        true
                    } else if xf.is_nan() || yf.is_nan() {
                        false
                    } else {
                        if let Some(int_v) = x_int.or(y_int) {
                            let flt_v = if x_int.is_some() { yf } else { xf };
                            if int_v.unsigned_abs() > (1u128 << 53) {
                                return flt_v.is_finite()
                                    && flt_v.fract() == 0.0
                                    && (flt_v as i128) == int_v;
                            }
                        }
                        (xf - yf).abs() <= tol
                    }
                }
                _ => x == y,
            }
        }
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

pub fn record(path: &Path, cases: &[Case]) -> io::Result<()> {
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

pub fn load(path: &Path) -> io::Result<Vec<Case>> {
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
                want: case
                    .output
                    .as_ref()
                    .map(|o| o.to_string())
                    .unwrap_or_default(),
                got: got_err,
            }),
            (None, Ok(got)) => {
                let want = strip_ignored_keys(
                    &case.output.clone().unwrap_or(Value::Null),
                    &candidate.ignore_fields,
                );
                let actual = strip_ignored_keys(&got, &candidate.ignore_fields);
                if eq_json(&want, &actual, 1e-9) {
                    covered += 1;
                } else {
                    fails.push(Fail {
                        input: case.input.clone(),
                        want: want.to_string(),
                        got: actual.to_string(),
                    });
                }
            }
        }
    }
    let required = min_cases.max(1);
    let ok = !cases.is_empty() && fails.is_empty() && covered >= required;
    let why = if ok {
        None
    } else if cases.is_empty() {
        Some("journal is empty (0 cases)".into())
    } else if !fails.is_empty() {
        Some(format!("{} mismatching case(s)", fails.len()))
    } else {
        Some(format!("covered {covered} < {required} required"))
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
            verdict.why = Some(format!(
                "cost ratio {ratio:.2}x > 1.00x: correct but slower"
            ));
        }
    }
    verdict
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DifferentialCase {
    pub input: Value,
    pub expected: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct DifferentialResult {
    pub passed: bool,
    pub cases: usize,
    pub failures: Vec<usize>,
    pub output: String,
}

/// JSON-lines differential replay; malformed or extra output is a failure.
pub fn run_differential(
    cwd: &Path,
    command: &str,
    cases: &[DifferentialCase],
    timeout: Duration,
    max_output_bytes: usize,
) -> io::Result<DifferentialResult> {
    let mut payload = Vec::new();
    for case in cases {
        serde_json::to_writer(&mut payload, &case.input)?;
        payload.push(b'\n');
        if payload.len() > MAX_INPUT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "differential input exceeds 8 MiB",
            ));
        }
    }
    let mut cmd = crate::exec::shell_command(command);
    cmd.current_dir(cwd);
    let out = run_process(&mut cmd, &payload, timeout, max_output_bytes)?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let actual: Result<Vec<Value>, _> = text.lines().map(serde_json::from_str).collect();
    let failures = cases
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            actual
                .as_ref()
                .ok()
                .and_then(|a| a.get(i))
                .is_none_or(|a| !eq_json(a, &c.expected, 1e-9))
                .then_some(i)
        })
        .collect::<Vec<_>>();
    Ok(DifferentialResult {
        passed: !cases.is_empty()
            && !out.timed_out
            && !out.truncated
            && out.exit_code == Some(0)
            && failures.is_empty()
            && actual.as_ref().is_ok_and(|a| a.len() == cases.len()),
        cases: cases.len(),
        failures,
        output: text,
    })
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

    #[test]
    fn differential_replay_is_strict() {
        let cwd = Path::new("/tmp");
        let cases = [DifferentialCase {
            input: json!({"x": 1}),
            expected: json!({"x": 1}),
        }];
        assert!(
            run_differential(cwd, "cat", &cases, Duration::from_secs(1), 1024)
                .unwrap()
                .passed
        );
        assert!(
            !run_differential(
                cwd,
                "echo not-json; cat",
                &cases,
                Duration::from_secs(1),
                1024
            )
            .unwrap()
            .passed
        );
    }

    #[test]
    fn raw_cli_oracle_recording_and_ignored_fields_work() {
        let jpath = std::env::temp_dir().join(format!("twig-oracle-rec-{}.jsonl", std::process::id()));
        let oracle = Candidate::new("tr", vec!["a-z".into(), "A-Z".into()]).with_raw_cli(true);
        let seeds = vec![
            json!({"_cli": {"args": [], "stdin": "hello twig\n"}}),
            json!("clean room\n"),
        ];
        let count = record_oracle(&jpath, &oracle, &seeds).unwrap();
        assert_eq!(count, 2);
        let verdict = verify(&jpath, &oracle, 2);
        assert!(verdict.ok, "{verdict:?}");
        let _ = std::fs::remove_file(&jpath);

        let noisy_path = journal(&[(
            "{\"q\":1}",
            "{\"q\":1,\"timestamp\":100}",
        )]);
        let masked = identity().with_ignore_fields(vec!["timestamp".into()]);
        assert!(verify(&noisy_path, &masked, 1).ok);
        let _ = std::fs::remove_file(&noisy_path);

        // Large 64-bit integers (> 2^53) must never collide via f64 rounding, and empty journals fail even if min_cases=0.
        assert!(!eq_json(
            &json!(9_007_199_254_740_992_i64),
            &json!(9_007_199_254_740_993_i64),
            1e-9
        ));
        assert!(eq_json(
            &json!(9_007_199_254_740_993_i64),
            &json!(9_007_199_254_740_993_i64),
            1e-9
        ));
        let empty_j = std::env::temp_dir().join(format!("twig-empty-j-{}.jsonl", std::process::id()));
        std::fs::write(&empty_j, "").unwrap();
        let empty_verdict = verify(&empty_j, &identity(), 0);
        assert!(!empty_verdict.ok);
        let _ = std::fs::remove_file(&empty_j);
    }
}
