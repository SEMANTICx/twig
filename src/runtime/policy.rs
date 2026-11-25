//! Three-tier command policy.
//!
//! T0 `Auto`  — read-only and executes no repo code: runs, not audited
//! T1 `Audit` — default. Runs immediately, recorded, audited out of band
//! T2 `Deny`  — hard-deny list, the only tier that interrupts a human
//!
//! Shell tokenization respects quotes and splits unquoted shell metacharacters
//! (including without whitespace, e.g. `cat a;python3 b.py`), and treats
//! command/process substitution (`$(...)`, backticks, `<(...)`) as non-auto.

use std::collections::{HashMap, HashSet};
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

/// Programs that only read, unless given one of the listed flags.
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
    (
        "find",
        &[
            "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprintf", "-fls",
        ],
    ),
    ("sort", &["-o", "--output"]),
    ("uniq", &[]),
    ("diff", &[]),
    ("stat", &[]),
    ("file", &[]),
    ("tree", &[]),
    ("which", &[]),
];

/// Read-only git subcommands. `--output` writes a file, so it disqualifies.
const GIT_READ: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "rev-parse",
    "ls-files",
];

fn read_only(seg: &[&str]) -> bool {
    let Some(program) = seg.first() else {
        return false;
    };
    if *program == "git" {
        return seg.get(1).is_some_and(|sub| GIT_READ.contains(sub))
            && !seg.iter().any(|t| t.starts_with("--output"));
    }
    READ_ONLY.iter().any(|(name, writes)| {
        name == program
            && !seg
                .iter()
                .any(|tok| writes.iter().any(|w| tok == w || tok.starts_with(&format!("{w}="))))
    })
}

const SHELL_OPS: &[&str] = &["|", "&&", "||", ";", ">", ">>", "<", "2>", "&>", "&"];
const WRITE_OPS: &[&str] = &[">", ">>", "2>", "&>"];
const SUBCOMMANDS: &[&str] = &[
    "git", "cargo", "npm", "pnpm", "yarn", "go", "uv", "pip", "docker",
];

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Op(&'static str),
    Unsafe,
}

/// Quote-aware shell lexer that separates unquoted metacharacters even without
/// surrounding spaces, and flags command/process substitutions as `Unsafe`.
fn lex_shell(cmd: &str) -> Option<Vec<Token>> {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;

    let flush = |cur: &mut String, has_token: &mut bool, out: &mut Vec<Token>| {
        if *has_token {
            out.push(Token::Word(std::mem::take(cur)));
            *has_token = false;
        }
    };

    while i < bytes.len() {
        let ch = cmd[i..].chars().next()?;
        let len = ch.len_utf8();

        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                cur.push(ch);
            }
            i += len;
            continue;
        }

        if in_double {
            match ch {
                '"' => {
                    in_double = false;
                    i += 1;
                }
                '\\' if i + 1 < bytes.len() => {
                    let next = cmd[i + 1..].chars().next()?;
                    cur.push(next);
                    i += 1 + next.len_utf8();
                }
                '`' => return Some(vec![Token::Unsafe]),
                '$' if bytes.get(i + 1) == Some(&b'(') => return Some(vec![Token::Unsafe]),
                _ => {
                    cur.push(ch);
                    i += len;
                }
            }
            continue;
        }

        match ch {
            '\'' => {
                in_single = true;
                has_token = true;
                i += 1;
            }
            '"' => {
                in_double = true;
                has_token = true;
                i += 1;
            }
            '\\' if i + 1 < bytes.len() => {
                let next = cmd[i + 1..].chars().next()?;
                cur.push(next);
                has_token = true;
                i += 1 + next.len_utf8();
            }
            c if c.is_whitespace() => {
                flush(&mut cur, &mut has_token, &mut out);
                if c == '\n' {
                    out.push(Token::Op(";"));
                }
                i += len;
            }
            '`' => return Some(vec![Token::Unsafe]),
            '$' if bytes.get(i + 1) == Some(&b'(') => return Some(vec![Token::Unsafe]),
            '(' | ')' => return Some(vec![Token::Unsafe]),
            '<' if bytes.get(i + 1) == Some(&b'(') => return Some(vec![Token::Unsafe]),
            '>' if bytes.get(i + 1) == Some(&b'(') => return Some(vec![Token::Unsafe]),
            '#' if !has_token => break, // unquoted comment at word boundary
            '&' => {
                flush(&mut cur, &mut has_token, &mut out);
                if bytes.get(i + 1) == Some(&b'&') {
                    out.push(Token::Op("&&"));
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'>') {
                    out.push(Token::Op("&>"));
                    i += 2;
                } else {
                    out.push(Token::Op("&"));
                    i += 1;
                }
            }
            '|' => {
                flush(&mut cur, &mut has_token, &mut out);
                if bytes.get(i + 1) == Some(&b'|') {
                    out.push(Token::Op("||"));
                    i += 2;
                } else {
                    out.push(Token::Op("|"));
                    i += 1;
                }
            }
            ';' => {
                flush(&mut cur, &mut has_token, &mut out);
                out.push(Token::Op(";"));
                i += 1;
            }
            '>' => {
                // Handle fd-prefixed redirection like `2>` or `1>>`
                if has_token && cur.chars().all(|c| c.is_ascii_digit()) {
                    cur.clear();
                    has_token = false;
                    if bytes.get(i + 1) == Some(&b'>') {
                        out.push(Token::Op(">>"));
                        i += 2;
                    } else {
                        out.push(Token::Op("2>"));
                        i += 1;
                    }
                } else {
                    flush(&mut cur, &mut has_token, &mut out);
                    if bytes.get(i + 1) == Some(&b'>') {
                        out.push(Token::Op(">>"));
                        i += 2;
                    } else {
                        out.push(Token::Op(">"));
                        i += 1;
                    }
                }
            }
            '<' => {
                flush(&mut cur, &mut has_token, &mut out);
                out.push(Token::Op("<"));
                i += 1;
            }
            _ => {
                cur.push(ch);
                has_token = true;
                i += len;
            }
        }
    }

    if in_single || in_double {
        return None; // unbalanced quotes: fail closed
    }
    flush(&mut cur, &mut has_token, &mut out);
    Some(out)
}

/// Normalize a command to its shape: literals become `*`, structure survives.
pub fn shape(cmd: &str) -> String {
    let tokens = match lex_shell(cmd) {
        Some(t) => t,
        None => return cmd.trim().to_string(),
    };
    if tokens.contains(&Token::Unsafe) {
        return "<unsafe>".to_string();
    }
    let mut out = Vec::with_capacity(tokens.len());
    let mut expect_program = true;
    let mut expect_sub = false;
    for tok in tokens {
        match tok {
            Token::Op(op) => {
                out.push(op.to_string());
                expect_program = true;
                expect_sub = false;
            }
            Token::Word(w) => {
                if w.starts_with('-') {
                    out.push(w);
                    expect_program = false;
                } else if expect_program {
                    expect_sub = SUBCOMMANDS.contains(&w.as_str());
                    out.push(w);
                    expect_program = false;
                } else if expect_sub {
                    out.push(w);
                    expect_sub = false;
                } else {
                    out.push("*".to_string());
                }
            }
            Token::Unsafe => return "<unsafe>".to_string(),
        }
    }
    out.join(" ")
}

pub struct Policy {
    cache: Mutex<HashMap<String, Tier>>,
    session_allows: Mutex<HashSet<String>>,
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
            session_allows: Mutex::new(HashSet::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Remember a user-approved command shape for the remainder of this session.
    /// Moves future matching commands from `Deny` to `Audit` (still recorded, no repeat prompt).
    pub fn allow_for_session(&self, cmd: &str) -> String {
        let sh = shape(cmd);
        let key = if sh.is_empty() || sh == "<unsafe>" {
            cmd.trim().to_owned()
        } else {
            sh
        };
        if !key.is_empty() {
            self.session_allows
                .lock()
                .expect("policy session allows")
                .insert(key.clone());
        }
        key
    }

/// Token-level destructive command check so flag reordering, extra whitespace, or
/// short flags (e.g. `rm -fr /`, `git push -f`, `git push origin main --force`,
/// `git clean -xdf`) cannot slip past substring rules.
fn matches_semantic_deny(cmd: &str) -> bool {
    let Some(tokens) = lex_shell(cmd) else {
        return false;
    };
    let mut seg: Vec<&str> = Vec::new();
    let check_seg = |raw: &[&str]| -> bool {
        let mut s = raw;
        while let Some(first) = s.first() {
            if matches!(*first, "sudo" | "env" | "command" | "builtin" | "noglob")
                || (first.contains('=') && !first.starts_with('-'))
            {
                s = &s[1..];
            } else {
                break;
            }
        }
        let Some(&prog) = s.first() else {
            return false;
        };
        let rest = &s[1..];
        let has_short = |ch: char| {
            rest.iter().any(|t| {
                t.starts_with('-') && !t.starts_with("--") && t[1..].contains(ch)
            })
        };
        match prog {
            "shutdown" | "reboot" | "poweroff" | "halt" => true,
            p if p == "mkfs" || p.starts_with("mkfs.") => true,
            "dd" => rest.iter().any(|t| t.starts_with("of=/dev/")),
            "chmod" => {
                let recursive = has_short('R') || rest.contains(&"--recursive");
                recursive && rest.contains(&"777") && rest.contains(&"/")
            }
            "rm" => {
                let recursive =
                    has_short('r') || has_short('R') || rest.contains(&"--recursive");
                let force = has_short('f') || rest.contains(&"--force");
                let dangerous_target = rest.iter().any(|t| {
                    matches!(
                        *t,
                        "/" | "/*" | "~" | "~/" | "~/*" | "$HOME" | "${HOME}" | "*" | "." | ".."
                    )
                });
                recursive && force && dangerous_target
            }
            "git" => match rest.first().copied() {
                Some("push") => rest[1..].iter().any(|t| {
                    *t == "-f"
                        || *t == "--force"
                        || t.starts_with("--force-")
                        || (t.starts_with('-') && !t.starts_with("--") && t[1..].contains('f'))
                }),
                Some("reset") => rest[1..].contains(&"--hard"),
                Some("clean") => {
                    let sub = &rest[1..];
                    let sub_short = |ch: char| {
                        sub.iter().any(|t| {
                            t.starts_with('-') && !t.starts_with("--") && t[1..].contains(ch)
                        })
                    };
                    let force = sub_short('f') || sub.contains(&"--force");
                    let dirs_or_ignored = sub_short('d') || sub_short('x') || sub_short('X');
                    force && dirs_or_ignored
                }
                _ => false,
            },
            _ => false,
        }
    };
    for tok in &tokens {
        match tok {
            Token::Word(w) => seg.push(w.as_str()),
            Token::Op(_) => {
                if check_seg(&seg) {
                    return true;
                }
                seg.clear();
            }
            Token::Unsafe => {}
        }
    }
    check_seg(&seg)
}

    /// Deny rules read the real command; allow rules read the folded shape.
    pub fn classify(&self, cmd: &str) -> Tier {
        let sh = shape(cmd);
        if HARD_DENY.iter().any(|d| cmd.contains(d)) || Self::matches_semantic_deny(cmd) {
            let allows = self.session_allows.lock().expect("policy session allows");
            if allows.contains(&sh) || allows.contains(cmd.trim()) {
                return Tier::Audit;
            }
            return Tier::Deny;
        }
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

    fn compound(&self, sh: &str) -> Tier {
        if sh.is_empty() || sh == "<unsafe>" {
            return Tier::Audit;
        }
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
                return Tier::Audit;
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
        let p = Policy::new();
        for example in [
            "git status",
            "git diff HEAD",
            "git log -5",
            "ls",
            "pwd",
            "wc -l x",
        ] {
            assert_eq!(p.classify(example), Tier::Auto, "{example} should be auto");
        }
        for example in ["python3 script.py", "pytest -x tests", "cargo test", "npm test"] {
            assert_eq!(
                p.classify(example),
                Tier::Audit,
                "{example} runs repo code"
            );
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
        assert_eq!(
            p.classify("ls && cat test_calc.py && wc -l calc.py"),
            Tier::Auto
        );
        assert_eq!(p.classify("cat a | grep foo"), Tier::Auto);
        assert_eq!(p.classify("cd src && ls"), Tier::Auto);
        assert_eq!(p.classify("ls&&cat a.txt"), Tier::Auto);
        assert_eq!(p.classify("cat 'a;python3 b.py'"), Tier::Auto);
    }

    #[test]
    fn unspaced_metacharacters_and_substitutions_cannot_bypass_audit() {
        let p = Policy::new();
        assert_eq!(p.classify("cat a;python3 b.py"), Tier::Audit);
        assert_eq!(p.classify("cat a&&python3 b.py"), Tier::Audit);
        assert_eq!(p.classify("cat a|sh"), Tier::Audit);
        assert_eq!(p.classify("cat a>out.txt"), Tier::Audit);
        assert_eq!(p.classify("echo $(python3 b.py)"), Tier::Audit);
        assert_eq!(p.classify("echo \"$(id)\""), Tier::Audit);
        assert_eq!(p.classify("echo `id`"), Tier::Audit);
        assert_eq!(p.classify("cat <(python3 b.py)"), Tier::Audit);
        assert_eq!(p.classify("cat a\npython3 b.py"), Tier::Audit);
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
        assert_eq!(shape("rm -rf /"), "rm -rf *");
        assert_eq!(p.classify("rm -rf /"), Tier::Deny);
        assert_eq!(p.classify("rm -fr /"), Tier::Deny);
        assert_eq!(p.classify("rm -r -f ~"), Tier::Deny);
        assert_eq!(p.classify("rm  -rf /*"), Tier::Deny);
        assert_eq!(p.classify("git push -f"), Tier::Deny);
        assert_eq!(p.classify("git push origin main --force"), Tier::Deny);
        assert_eq!(p.classify("git push --force-with-lease origin main"), Tier::Deny);
        assert_eq!(p.classify("git clean -xdf"), Tier::Deny);
        assert_eq!(p.classify("sudo git reset --hard HEAD~1"), Tier::Deny);
        // Safe variants remain Audit (never falsely denied).
        assert_eq!(p.classify("rm -rf target/tmp"), Tier::Audit);
        assert_eq!(p.classify("git push origin main"), Tier::Audit);
        assert_eq!(p.classify("git clean -n"), Tier::Audit);
    }

    #[test]
    fn distinct_commands_of_one_shape_cost_one_miss() {
        let p = Policy::new();
        p.classify("cat one.txt");
        let (_, before, _) = p.stats();
        p.classify("cat two.txt");
        let (hits, after, _) = p.stats();
        assert_eq!(
            before, after,
            "second command of the same shape must hit cache"
        );
        assert!(hits >= 1);
    }
}
