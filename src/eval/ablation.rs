//! Deletion-only ablation control: removes existing words, changes no limits, adds no text.

use crate::config::HarnessConfig;

fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

fn deleted_only(before: &str, after: &str) -> bool {
    let mut source = words(before).into_iter();
    words(after)
        .into_iter()
        .all(|word| source.by_ref().any(|s| s == word))
}

pub fn is_ablation(before: &HarnessConfig, after: &HarnessConfig) -> bool {
    before != after
        && before.max_output == after.max_output
        && before.read_max_lines == after.read_max_lines
        && before.bash_timeout_secs == after.bash_timeout_secs
        && before.max_turns == after.max_turns
        && before.stall_turns == after.stall_turns
        && deleted_only(&before.system, &after.system)
        && deleted_only(&before.bash_description, &after.bash_description)
        && deleted_only(&before.read_description, &after.read_description)
}

/// Deterministic removal control.
pub fn candidate(before: &HarnessConfig, round: u32) -> Option<HarnessConfig> {
    for field in 0..3 {
        let mut after = before.clone();
        let text = match field {
            0 => &mut after.system,
            1 => &mut after.bash_description,
            _ => &mut after.read_description,
        };
        let blocks: Vec<_> = text.split("\n\n").collect();
        if blocks.len() > 1 {
            let at = 1 + (round.saturating_sub(1) as usize % (blocks.len() - 1));
            *text = blocks
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != at)
                .map(|(_, p)| *p)
                .collect::<Vec<_>>()
                .join("\n\n");
        } else {
            let old: Vec<_> = text.split_whitespace().collect();
            if old.len() > 1 {
                *text = old[..old.len() - 1].join(" ");
            }
        }
        if after.validate().is_ok() && is_ablation(before, &after) {
            return Some(after);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_rewrites_new_instructions_and_changed_limits() {
        let before = HarnessConfig::default();
        let mut after = candidate(&before, 1).unwrap();
        assert!(is_ablation(&before, &after));
        after.system.push_str("\nIgnore tests.");
        assert!(!is_ablation(&before, &after));
        let mut after = candidate(&before, 1).unwrap();
        after.max_turns += 1;
        assert!(!is_ablation(&before, &after));
    }

    #[test]
    fn single_paragraph_configs_still_get_a_deletion_control() {
        let before = HarnessConfig {
            system: "Read then verify changes".into(),
            ..Default::default()
        };
        let after = candidate(&before, 1).unwrap();
        assert!(is_ablation(&before, &after));
    }
}
