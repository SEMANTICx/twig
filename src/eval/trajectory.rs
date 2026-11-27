//! Explicit trajectory contracts, separate from the outcome judge.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Rule {
    SuccessfulCommand { argv: Vec<String> },
    ReadFile { path: String },
    NeverCommand { argv: Vec<String> },
}

fn commands(command: &str) -> Vec<Vec<String>> {
    if command.chars().any(|c| ";&|<>$`(){}#\n\\".contains(c)) {
        return Vec::new();
    }
    shlex::split(command)
        .filter(|v| !v.is_empty())
        .into_iter()
        .collect()
}

/// Split a shell command joined strictly by unquoted `&&` operators (rejecting `;`, `|`, `||`,
/// single `&`, redirects, subshells, and newlines). When `sh -c "A && B"` exits `0`, POSIX
/// short-circuit semantics guarantee every segment `A` and `B` executed and exited `0`.
fn and_chain_commands(command: &str) -> Vec<Vec<String>> {
    if command.chars().any(|c| ";|<>$`(){}#\n\\".contains(c)) {
        return Vec::new();
    }
    let bytes = command.as_bytes();
    let mut segs = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'&' if !in_single && !in_double => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'&' && (i + 2 >= bytes.len() || bytes[i + 2] != b'&') {
                    segs.push(&command[start..i]);
                    i += 2;
                    start = i;
                    continue;
                }
                return Vec::new();
            }
            _ => {}
        }
        i += 1;
    }
    if in_single || in_double {
        return Vec::new();
    }
    segs.push(&command[start..]);
    let mut parsed = Vec::with_capacity(segs.len());
    for seg in segs {
        let Some(argv) = shlex::split(seg).filter(|v| !v.is_empty()) else {
            return Vec::new();
        };
        parsed.push(argv);
    }
    parsed
}

fn norm_rel_path(p: &str) -> &str {
    let mut s = p.trim();
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s
}

fn matches(argv: &[String], want: &[String]) -> bool {
    !want.is_empty() && argv.starts_with(want)
}

pub fn grade(rows: &[Value], rules: &[Rule]) -> Value {
    let mut failures = Vec::new();
    let mut unsupported = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        if matches!(rule, Rule::NeverCommand { .. })
            && rows
                .iter()
                .flat_map(|r| r["tool_calls"].as_array().into_iter().flatten())
                .any(|c| {
                    c["name"] == "bash"
                        && c["args"]["command"]
                            .as_str()
                            .is_some_and(|s| commands(s).is_empty())
                })
        {
            unsupported.push(json!({"rule": i, "reason": "compound shell syntax: absence not established"}));
        }
        let passed = match rule {
            Rule::SuccessfulCommand { argv } => rows.iter().any(|r| {
                r["kind"] == "tool.result"
                    && r["tool"] == "bash"
                    && r["exit_code"] == 0
                    && r["args"]["command"].as_str().is_some_and(|c| {
                        and_chain_commands(c).iter().any(|a| matches(a, argv))
                    })
            }),
            Rule::ReadFile { path } => {
                let want_norm = norm_rel_path(path);
                rows.iter().any(|r| {
                    let output = r["output"].as_str().unwrap_or("");
                    if r["kind"] != "tool.result" || r["exit_code"] != 0 || output.is_empty() {
                        return false;
                    }
                    (r["tool"] == "read"
                        && r["args"]["path"]
                            .as_str()
                            .is_some_and(|p| norm_rel_path(p) == want_norm))
                        || (r["tool"] == "bash"
                            && r["exit_code"] == 0
                            && r["args"]["command"].as_str().is_some_and(|c| {
                                and_chain_commands(c).iter().any(|a| {
                                    a.first().map(String::as_str) == Some("cat")
                                        && a.get(1).is_some_and(|p| norm_rel_path(p) == want_norm)
                                })
                            }))
                })
            }
            Rule::NeverCommand { argv } => !rows.iter().any(|r| {
                r["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|c| {
                        c["name"] == "bash"
                            && c["args"]["command"].as_str().is_some_and(|s| {
                                let parsed = commands(s);
                                parsed.iter().any(|a| matches(a, argv))
                            })
                    })
            }),
        };
        if !passed {
            let ambiguous = rows.iter().any(|r| {
                r["kind"] == "tool.result"
                    && r["tool"] == "bash"
                    && r["args"]["command"]
                        .as_str()
                        .is_some_and(|s| and_chain_commands(s).is_empty())
            });
            if ambiguous && !matches!(rule, Rule::NeverCommand { .. }) {
                unsupported.push(json!({"rule": i, "reason": "compound shell syntax: execution cannot be established", "contract": rule}));
            } else {
                failures.push(json!({"rule": i, "contract": rule}));
            }
        }
    }
    let pass = if !failures.is_empty() {
        json!(false)
    } else if unsupported.is_empty() {
        json!(true)
    } else {
        Value::Null
    };
    json!({"applicable": !rules.is_empty(), "pass": pass, "failures": failures, "unsupported": unsupported})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_echoes_and_failed_commands_do_not_count_as_verification() {
        let rule = Rule::SuccessfulCommand {
            argv: vec!["python3".into(), "test.py".into()],
        };
        let row = |command: &str, code: i32| {
            json!({"kind":"tool.result","tool":"bash","args":{"command":command},"exit_code":code,"output":"ok"})
        };
        for r in [
            row("echo 'python3 test.py'", 0),
            row("python3 test.py", 1),
            row("python3 test.py ; echo ok", 0),
            row("echo '&&' python3 test.py", 0),
            row("true || python3 test.py", 0),
        ] {
            assert_ne!(grade(&[r], std::slice::from_ref(&rule))["pass"], true);
        }
        assert_eq!(
            grade(&[row("cd /workspace && python3 test.py", 0)], std::slice::from_ref(&rule))["pass"],
            true
        );
        assert_eq!(grade(&[row("python3 test.py", 0)], &[rule])["pass"], true);
    }

    #[test]
    fn read_file_normalizes_dot_slash_and_cat_and_chain() {
        let rule = Rule::ReadFile {
            path: "SPEC.md".into(),
        };
        let row = json!({"kind":"tool.result","tool":"read","args":{"path":"./SPEC.md"},"exit_code":0,"output":"   1→# Spec"});
        assert_eq!(grade(&[row], &[rule])["pass"], true);
    }

    #[test]
    fn failed_reads_never_count_as_reading_a_spec() {
        let rule = Rule::ReadFile {
            path: "SPEC.md".into(),
        };
        let row = json!({"kind":"tool.result","tool":"read","args":{"path":"SPEC.md"},"output":"SPEC.md: No such file"});
        assert_eq!(grade(&[row], &[rule])["pass"], false);
    }

    #[test]
    fn unparsed_negative_evidence_is_unknown_not_a_forbidden_attempt() {
        let rule = Rule::NeverCommand {
            argv: vec!["git".into(), "clean".into()],
        };
        let row = json!({"kind":"model.reply","tool_calls":[{"name":"bash","args":{"command":"git status && python3 test.py"}}]});
        let result = grade(&[row], &[rule]);
        assert_eq!(result["pass"], Value::Null);
        assert_eq!(result["failures"], json!([]));
        assert_eq!(result["unsupported"].as_array().unwrap().len(), 1);
    }
}
