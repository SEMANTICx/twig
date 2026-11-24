//! Failure classification: process versus content.
//!
//! Before investing in harness work, measure what your failures actually are.
//! The evidence is blunt about the split:
//!
//!   process failures (blocked calls, loops, exhausted step budgets) are what
//!   harness evolution repairs; content failures (a delivered plan that is poor)
//!   are what weight training is for.
//!
//! So the first firing signal in a trajectory decides the bucket. If content
//! failures dominate, building a cleverer harness will not move the number.

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

    /// What to do next, stated as a decision rather than a suggestion.
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
            // a call was refused: process
            "approval.requested" => return Some(FailureKind::Process),
            // the guard narrowed the toolset or ran out of budget: process.
            // `warn` is advice, not a failure.
            "guard.tripped" if row["level"] != "warn" => return Some(FailureKind::Process),
            // a tool hit its time budget or was refused: process
            "tool.result"
                if row["output"]
                    .as_str()
                    .is_some_and(|o| o.starts_with("timeout:") || o.starts_with("refused:")) =>
            {
                return Some(FailureKind::Process)
            }
            // replay disagreed with the recorded oracle: content
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
        assert!(Breakdown::default().lever().contains("measure before building"));
    }
}
