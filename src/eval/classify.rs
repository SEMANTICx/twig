//! Failure classification: process versus content.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureKind {
    Process,
    Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Breakdown {
    pub process: usize,
    pub content: usize,
}

impl Breakdown {
    pub fn total(&self) -> usize {
        self.process + self.content
    }

    pub fn process_share(&self) -> f64 {
        if self.total() == 0 {
            0.0
        } else {
            self.process as f64 / self.total() as f64
        }
    }

    pub fn lever(&self) -> &'static str {
        if self.total() == 0 {
            "no failures recorded yet: measure before building"
        } else if self.process_share() >= 0.6 {
            "process failures dominate: harness work pays (propose-probe-compose)"
        } else if self.process_share() <= 0.2 {
            "content failures dominate: harness evolution will not move this; \
             narrow the task or change the model"
        } else {
            "mixed: fix the cheap process failures first, then re-measure"
        }
    }
}

/// Classify one trajectory (its record rows, in order) by its first firing signal.
pub fn classify_rows(rows: &[Value]) -> Option<FailureKind> {
    for row in rows {
        match row.get("kind").and_then(Value::as_str).unwrap_or("") {
            "approval.requested" => return Some(FailureKind::Process),
            "guard.tripped" if row["level"] != "warn" => return Some(FailureKind::Process),
            "tool.result"
                if row["output"]
                    .as_str()
                    .is_some_and(|o| o.starts_with("timeout:") || o.starts_with("refused:")) =>
            {
                return Some(FailureKind::Process)
            }
            "verify.failed" => return Some(FailureKind::Content),
            _ => {}
        }
    }
    None
}

pub fn summarize<I>(trajectories: I) -> Breakdown
where
    I: IntoIterator<Item = Vec<Value>>,
{
    let mut out = Breakdown::default();
    for rows in trajectories {
        match classify_rows(&rows) {
            Some(FailureKind::Process) => out.process += 1,
            Some(FailureKind::Content) => out.content += 1,
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn first_signal_decides() {
        let rows = vec![
            json!({"kind":"turn.started"}),
            json!({"kind":"approval.requested","id":"t1"}),
            json!({"kind":"verify.failed"}),
        ];
        assert_eq!(classify_rows(&rows), Some(FailureKind::Process));
    }

    #[test]
    fn content_failure_is_content() {
        let rows = vec![
            json!({"kind":"turn.started"}),
            json!({"kind":"verify.failed","why":"diff empty"}),
        ];
        assert_eq!(classify_rows(&rows), Some(FailureKind::Content));
    }

    #[test]
    fn clean_run_is_unclassified() {
        assert_eq!(classify_rows(&[json!({"kind":"turn.completed"})]), None);
    }

    #[test]
    fn lever_follows_the_share() {
        assert_eq!(
            summarize(vec![
                vec![json!({"kind":"guard.tripped"})],
                vec![json!({"kind":"approval.requested"})],
                vec![json!({"kind":"verify.failed"})],
            ])
            .lever(),
            "process failures dominate: harness work pays (propose-probe-compose)"
        );
        assert!(Breakdown::default()
            .lever()
            .contains("measure before building"));
    }
}
