//! Explicit trajectory contracts. Separate from the outcome judge: a successful
//! result does not prove the requested verification ran. Rules inspect real tool
//! results, executable argv and exit codes, never assistant claims or echo text.
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
    // This is evidence grading, not a shell parser. Any control flow, expansion,
    // redirection or comment makes the invocation ambiguous, including quoted
    // operators: shlex alone cannot distinguish them after quote removal.
    if command.chars().any(|c| ";&|<>$`(){}#\n\\".contains(c)) {
        return Vec::new();
    }
    shlex::split(command).filter(|v| !v.is_empty()).into_iter().collect()
}

fn matches(argv: &[String], want: &[String]) -> bool {
    !want.is_empty() && argv.starts_with(want)
}

pub fn grade(rows: &[Value], rules: &[Rule]) -> Value {
    let mut failures = Vec::new();
    let mut unsupported = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        if matches!(rule, Rule::NeverCommand { .. }) && rows.iter()
            .flat_map(|r| r["tool_calls"].as_array().into_iter().flatten())
            .any(|c| c["name"] == "bash" && c["args"]["command"].as_str().is_some_and(|s| commands(s).is_empty())) {
            unsupported.push(json!({"rule": i, "reason":"compound shell syntax: absence not established"}));
        }
        let passed = match rule {
            Rule::SuccessfulCommand { argv } => rows.iter().any(|r| {
                r["kind"] == "tool.result" && r["tool"] == "bash" && r["exit_code"] == 0
                    && r["args"]["command"].as_str().is_some_and(|c| {
                        let commands = commands(c);
                        // A compound command's exit code proves only its last
                        // command. Earlier successes cannot be inferred from it.
                        commands.last().is_some_and(|a| matches(a, argv))
                    })
            }),
            Rule::ReadFile { path } => rows.iter().any(|r| {
                let output = r["output"].as_str().unwrap_or("");
                if r["kind"] != "tool.result" || r["exit_code"] != 0 || output.is_empty() { return false; }
                (r["tool"] == "read" && r["args"]["path"] == path.as_str())
                    || (r["tool"] == "bash" && r["exit_code"] == 0
                        && r["args"]["command"].as_str().is_some_and(|c| {
                            let want = vec!["cat".to_string(), path.clone()];
                            commands(c).last().is_some_and(|a| matches(a, &want))
                        }))
            }),
            Rule::NeverCommand { argv } => !rows.iter().any(|r| {
                // Includes blocked attempts: the instruction concerns what the
                // agent attempted, not merely what the policy allowed.
                let calls = r["tool_calls"].as_array();
                calls.into_iter().flatten().any(|c| c["name"] == "bash"
                    && c["args"]["command"].as_str().is_some_and(|s| {
                        let parsed = commands(s);
                        parsed.iter().any(|a| matches(a, argv))
                    }))
            }),
        };
        if !passed {
            let ambiguous = rows.iter().any(|r| r["kind"] == "tool.result" && r["tool"] == "bash"
                && r["args"]["command"].as_str().is_some_and(|s| commands(s).is_empty()));
            if ambiguous && !matches!(rule, Rule::NeverCommand { .. }) {
                unsupported.push(json!({"rule":i,"reason":"compound shell syntax: execution cannot be established","contract":rule}));
            } else { failures.push(json!({"rule": i, "contract": rule})); }
        }
    }
    let pass = if !failures.is_empty() { json!(false) } else if unsupported.is_empty() { json!(true) } else { Value::Null };
    json!({"applicable": !rules.is_empty(), "pass": pass, "failures": failures, "unsupported": unsupported})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claims_echoes_and_failed_commands_do_not_count_as_verification() {
        let rule = Rule::SuccessfulCommand { argv: vec!["python3".into(), "test.py".into()] };
        let row = |command: &str, code: i32| json!({"kind":"tool.result","tool":"bash",
            "args":{"command":command},"exit_code":code,"output":"ok"});
        for r in [row("echo 'python3 test.py'", 0), row("python3 test.py", 1), row("python3 test.py ; echo ok", 0),
            row("echo '&&' python3 test.py", 0), row("true || python3 test.py", 0)] {
            assert_ne!(grade(&[r], std::slice::from_ref(&rule))["pass"], true);
        }
        assert_eq!(grade(&[row("python3 test.py", 0)], &[rule])["pass"], true);
    }
    #[test]
    fn failed_reads_never_count_as_reading_a_spec() {
        let rule = Rule::ReadFile { path: "SPEC.md".into() };
        let row = json!({"kind":"tool.result","tool":"read","args":{"path":"SPEC.md"},
            "output":"SPEC.md: No such file"});
        assert_eq!(grade(&[row], &[rule])["pass"], false);
    }
    #[test]
    fn unparsed_negative_evidence_is_unknown_not_a_forbidden_attempt() {
        let rule = Rule::NeverCommand { argv: vec!["git".into(), "clean".into()] };
        let row = json!({"kind":"model.reply","tool_calls":[{"name":"bash",
            "args":{"command":"git status && python3 test.py"}}]});
        let result = grade(&[row], &[rule]);
        assert_eq!(result["pass"], Value::Null);
        assert_eq!(result["failures"], json!([]));
        assert_eq!(result["unsupported"].as_array().unwrap().len(), 1);
    }
}
