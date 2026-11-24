//! Three-tier command policy.
//!
//! T0 `Auto`  — read-only and executes no repo code: runs, not audited
//! T1 `Audit` — default. Runs immediately, recorded, audited out of band
//! T2 `Deny`  — hard-deny list, the only tier that interrupts a human
//!
//! Risk is classified once per turn and cached by command *shape*, so `cat a.txt`
//! and `cat b.txt` share one verdict and every later call of a shape is free.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Auto,
    Audit,
    Deny,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Auto => "auto",
            Tier::Audit => "audit",
            Tier::Deny => "deny",
        }
    }
}

/// Irreversible or machine-destroying. Keep this short: every rule costs a human
/// interruption, so each one must earn its place.
pub const HARD_DENY: &[&str] = &[
    "rm -rf /",
    "rm -rf ~",
    "rm -rf *",
    "git push --force",
    "git reset --hard",
    "git clean -fdx",
    "dd of=/dev/",
    "mkfs",
    ":(){",
    "chmod -R 777 /",
    "shutdown",
    "reboot",
];

/// Programs that only read, unless given one of the listed flags. Matching on
/// the program name rather than the exact shape means `wc -l x`, `rg -n x` and
/// `git diff --stat` are recognised without enumerating every flag combination.
const READ_ONLY: &[(&str, &[&str])] = &[
    ("ls", &[]),
    ("cat", &[]),
    ("head", &[]),
    ("tail", &[]),
    ("wc", &[]),
    ("pwd", &[]),
    ("cd", &[]),
    ("echo", &[]),
    ("grep", &[]),
    ("rg", &["--pre"]),
    ("fd", &["-x", "--exec", "-X", "--exec-batch"]),
    ("find", &["-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprintf", "-fls"]),
    ("sort", &["-o", "--output"]),
    ("uniq", &[]),
    ("diff", &[]),
    ("stat", &[]),
    ("file", &[]),
    ("tree", &[]),
    ("which", &[]),
];

/// Read-only git subcommands. `--output` writes a file, so it disqualifies.
const GIT_READ: &[&str] = &["status", "diff", "log", "show", "blame", "rev-parse", "ls-files"];

fn read_only(seg: &[&str]) -> bool {
    let Some(program) = seg.first() else { return false };
    if *program == "git" {
        return seg.get(1).is_some_and(|sub| GIT_READ.contains(sub))
            && !seg.iter().any(|t| t.starts_with("--output"));
    }
    READ_ONLY.iter().any(|(name, writes)| {
        name == program
            && !seg.iter().any(|tok| writes.iter().any(|w| tok == w || tok.starts_with(&format!("{w}="))))
    })
}

const SHELL_OPS: &[&str] = &["|", "&&", "||", ";", ">", ">>", "<", "2>", "&"];
const WRITE_OPS: &[&str] = &[">", ">>", "2>"];

/// Programs whose first positional argument is a verb, not data.
const SUBCOMMANDS: &[&str] = &["git", "cargo", "npm", "pnpm", "yarn", "go", "uv", "pip", "docker"];

/// Normalize a command to its shape: literals become `*`, structure survives.
///
/// The program name and flags are structural; arguments are not. Shapes are the
/// cache key, which is what makes repeated commands free.
pub fn shape(cmd: &str) -> String {
    let tokens = match shlex::split(cmd) {
        Some(t) => t,
        None => return cmd.trim().to_string(), // unbalanced quotes: fail closed
    };
    let mut out = Vec::with_capacity(tokens.len());
    let mut expect_program = true;
    let mut expect_sub = false;
    for tok in tokens {
        if SHELL_OPS.contains(&tok.as_str()) {
            out.push(tok);
            expect_program = true;
            expect_sub = false;
        } else if tok.starts_with('-') {
            out.push(tok);
            expect_program = false;
        } else if expect_program {
            expect_sub = SUBCOMMANDS.contains(&tok.as_str());
            out.push(tok); // argv[0] is structural
            expect_program = false;
        } else if expect_sub {
            out.push(tok); // `git diff` and `git push` must not fold to the same shape
            expect_sub = false;
        } else {
            out.push("*".to_string());
        }
    }
    out.join(" ")
}

pub struct Policy {
    cache: Mutex<HashMap<String, Tier>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Default for Policy {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Deny rules read the real command; allow rules read the folded shape.
    ///
    /// Folding is lossy by design, so anything destructive has to be recognised
    /// before the arguments are erased.
    pub fn classify(&self, cmd: &str) -> Tier {
        if HARD_DENY.iter().any(|d| cmd.contains(d)) {
            return Tier::Deny;
        }
        let sh = shape(cmd);
        if let Some(tier) = self.cache.lock().expect("policy cache").get(&sh) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return *tier;
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let tier = self.compound(&sh);
        self.cache
            .lock()
            .expect("policy cache")
            .insert(sh, tier);
        tier
    }

    /// A compound command is only as safe as its least safe segment. Agents chain
    /// reads (`ls && cat x && wc -l y`), and matching the whole line against the
    /// allow-list would send every such chain to audit and drown the audit log.
    fn compound(&self, sh: &str) -> Tier {
        let mut segment: Vec<&str> = Vec::new();
        let mut all_auto = true;
        let flush = |seg: &mut Vec<&str>, all: &mut bool| {
            if !seg.is_empty() && !read_only(seg) {
                *all = false;
            }
            seg.clear();
        };
        for tok in sh.split(' ') {
            if WRITE_OPS.contains(&tok) {
                return Tier::Audit; // a redirect writes a file
            }
            if SHELL_OPS.contains(&tok) {
                flush(&mut segment, &mut all_auto);
            } else {
                segment.push(tok);
            }
        }
        flush(&mut segment, &mut all_auto);
        if all_auto {
            Tier::Auto
        } else {
            Tier::Audit
        }
    }

    /// (hits, misses, cache size)
    pub fn stats(&self) -> (u64, u64, usize) {
        let size = self.cache.lock().expect("policy cache").len();
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
            size,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_folds_arguments_but_keeps_structure() {
        assert_eq!(shape("cat a.txt"), "cat *");
        assert_eq!(shape("rg -n foo"), "rg -n *");
        assert_eq!(shape("pwd"), "pwd");
        assert_eq!(shape("cat a | wc -l"), "cat * | wc -l");
    }

    #[test]
    fn subcommands_survive_folding() {
        assert_eq!(shape("git diff --stat HEAD~1"), "git diff --stat *");
        assert_ne!(shape("git diff"), shape("git push"));
        assert_eq!(shape("cargo test"), "cargo test");
    }

    #[test]
    fn every_allow_entry_is_reachable() {
        // A dead allow-list entry looks like policy and does nothing. These were
        // dead for a whole release because folding erased the subcommand.
        let p = Policy::new();
        for example in ["git status", "git diff HEAD", "git log -5", "ls", "pwd", "wc -l x"] {
            assert_eq!(p.classify(example), Tier::Auto, "{example} should be auto");
        }
        // Auto means read-only and runs no repo code. Test runners execute test code
        // and build scripts, so they are audited. Audit never blocks, so this costs
        // no latency; it only keeps the audit log honest.
        for example in ["python3 script.py", "pytest -x tests", "cargo test", "npm test"] {
            assert_eq!(p.classify(example), Tier::Audit, "{example} runs repo code");
        }
    }

    #[test]
    fn tiers() {
        let p = Policy::new();
        assert_eq!(p.classify("cat README.md"), Tier::Auto);
        assert_eq!(p.classify("make deploy"), Tier::Audit);
        assert_eq!(p.classify("rm -rf /"), Tier::Deny);
    }

    #[test]
    fn compound_reads_stay_auto() {
        let p = Policy::new();
        assert_eq!(p.classify("ls && cat test_calc.py && wc -l calc.py"), Tier::Auto);
        assert_eq!(p.classify("cat a | grep foo"), Tier::Auto);
        assert_eq!(p.classify("cd src && ls"), Tier::Auto);
    }

    #[test]
    fn flags_do_not_defeat_the_allow_list() {
        let p = Policy::new();
        assert_eq!(p.classify("wc -l calc.py"), Tier::Auto);
        assert_eq!(p.classify("rg -n --hidden foo src"), Tier::Auto);
        assert_eq!(p.classify("git diff --stat HEAD~1"), Tier::Auto);
    }

    #[test]
    fn write_flags_on_read_programs_are_audited() {
        let p = Policy::new();
        assert_eq!(p.classify("find . -name '*.pyc' -delete"), Tier::Audit);
        assert_eq!(p.classify("find . -exec touch {} ;"), Tier::Audit);
        assert_eq!(p.classify("sort -o out.txt in.txt"), Tier::Audit);
        assert_eq!(p.classify("git diff --output=patch.diff"), Tier::Audit);
        assert_eq!(p.classify("git commit -m x"), Tier::Audit);
    }

    #[test]
    fn one_unsafe_segment_taints_the_chain() {
        let p = Policy::new();
        assert_eq!(p.classify("ls && python3 build.py"), Tier::Audit);
        assert_eq!(p.classify("cat a > b"), Tier::Audit, "a redirect writes");
        assert_eq!(p.classify("echo hi >> log"), Tier::Audit);
        assert_eq!(p.classify("ls; rm -rf /"), Tier::Deny);
    }

    #[test]
    fn deny_is_checked_before_folding() {
        let p = Policy::new();
        // folding would erase the "/" that makes this destructive
        assert_eq!(shape("rm -rf /"), "rm -rf *");
        assert_eq!(p.classify("rm -rf /"), Tier::Deny);
    }

    #[test]
    fn distinct_commands_of_one_shape_cost_one_miss() {
        let p = Policy::new();
        p.classify("cat one.txt");
        let (_, before, _) = p.stats();
        p.classify("cat two.txt");
        let (hits, after, _) = p.stats();
        assert_eq!(before, after, "second command of the same shape must hit cache");
        assert!(hits >= 1);
    }
}
