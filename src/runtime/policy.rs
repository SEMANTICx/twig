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
    ("readelf", &[]),
    ("nm", &[]),
    ("strings", &[]),
    ("ldd", &[]),
    ("objdump", &[]),
    ("hexdump", &[]),
    ("od", &[]),
    ("sha256sum", &[]),
    ("sha1sum", &[]),
    ("md5sum", &[]),
    ("b3sum", &[]),
    ("ffprobe", &[]),
    ("jq", &[]),
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

/// Returns `true` when `raw` points to a sensitive secret/credential file (e.g. `.env`,
/// `.env.production`, SSH private keys, `.pem`/`.key`, `.npmrc`, `.netrc`), while allowing
/// safe committed templates like `.env.example`, `.env.sample`, `.env.template`, or `.env.test`.
pub fn is_sensitive_secret_path(raw: &str) -> bool {
    let trimmed = raw
        .trim()
        .trim_matches(['"', '\''])
        .trim_start_matches('@');
    let candidate = match trimmed.split_once('=') {
        Some((flag, val)) if flag.starts_with('-') => val.trim_matches(['"', '\'']),
        _ => trimmed,
    };
    if candidate.is_empty() || candidate.starts_with('-') {
        return false;
    }
    let norm = candidate.replace('\\', "/");
    let lower = norm.to_ascii_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    if base.is_empty() {
        return false;
    }
    if base == ".env" {
        return true;
    }
    if let Some(suffix) = base.strip_prefix(".env.") {
        return !matches!(
            suffix,
            "example" | "sample" | "template" | "dist" | "schema" | "defaults" | "test" | "ci"
        );
    }
    if matches!(
        base,
        ".npmrc"
            | ".pypirc"
            | ".netrc"
            | ".git-credentials"
            | "id_rsa"
            | "id_ed25519"
            | "id_ecdsa"
            | "id_dsa"
            | "credentials.json"
            | "service-account.json"
    ) {
        return true;
    }
    if lower.ends_with("/.aws/credentials") || lower.ends_with("/.docker/config.json") {
        return true;
    }
    matches!(
        base.rsplit_once('.').map(|(_, ext)| ext),
        Some("pem" | "key" | "p12" | "pfx" | "kdbx")
    )
}

fn touches_sensitive_secret(cmd: &str) -> bool {
    let Some(tokens) = lex_shell(cmd) else {
        return false;
    };
    let mut first_in_seg = true;
    let mut skip_seg = false;
    for tok in tokens {
        match tok {
            Token::Op(_) => {
                first_in_seg = true;
                skip_seg = false;
            }
            Token::Word(w) => {
                if first_in_seg {
                    first_in_seg = false;
                    let base = w.rsplit('/').next().unwrap_or(&w);
                    if base == "echo" || base == "printf" {
                        skip_seg = true;
                    }
                    continue;
                }
                if !skip_seg && is_sensitive_secret_path(&w) {
                    return true;
                }
            }
            Token::Unsafe => {}
        }
    }
    false
}

/// Returns `true` when `raw` targets harness control-plane configuration or session transcripts
/// (`.twig/project.json`, `.twig/mcp.json`, `.twig/harness.json`, `~/.config/twig/*`,
/// `~/.local/state/twig/*`, or `sessions/*.jsonl`), preventing prompt-injected self-tampering
/// without explicit human approval (`Tier::Deny`).
pub fn is_control_plane_path(raw: &str) -> bool {
    let trimmed = raw.trim().trim_matches(['"', '\'']);
    let candidate = match trimmed.split_once('=') {
        Some((flag, val)) if flag.starts_with('-') => val.trim_matches(['"', '\'']),
        _ => trimmed,
    };
    if candidate.is_empty() || candidate.starts_with('-') {
        return false;
    }
    let norm = candidate.replace('\\', "/");
    let clean = norm.trim_start_matches("./");
    let lower = clean.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        ".twig/project.json" | ".twig/mcp.json" | ".twig/harness.json" | ".git/config"
    ) || lower.ends_with("/.twig/project.json")
        || lower.ends_with("/.twig/mcp.json")
        || lower.ends_with("/.twig/harness.json")
        || lower.ends_with("/.git/config")
        || lower.starts_with(".git/hooks/")
        || lower.contains("/.git/hooks/")
        || lower.starts_with(".git/info/")
        || lower.contains("/.git/info/")
        || lower.starts_with(".githooks/")
        || lower.contains("/.githooks/")
        || lower.starts_with(".husky/")
        || lower.contains("/.husky/")
    {
        return true;
    }
    if lower.contains("/.config/twig/")
        || lower.ends_with("/.config/twig")
        || lower.contains("/.local/state/twig/")
        || lower.ends_with("/.local/state/twig")
    {
        return true;
    }
    (lower.contains("sessions/") || lower.contains("subagents/"))
        && (lower.ends_with(".jsonl") || lower.ends_with(".lock"))
}

fn matches_meta_escape(cmd: &str) -> bool {
    let Some(tokens) = lex_shell(cmd) else {
        return false;
    };
    let check_seg = |raw: &[&str]| -> bool {
        let mut s = raw;
        while let Some(first) = s.first() {
            let base = first.rsplit('/').next().unwrap_or(first);
            if matches!(base, "sudo" | "env" | "command" | "builtin" | "noglob")
                || (first.contains('=') && !first.starts_with('-'))
            {
                s = &s[1..];
            } else {
                break;
            }
        }
        let Some(&raw_prog) = s.first() else {
            return false;
        };
        let prog = raw_prog.rsplit('/').next().unwrap_or(raw_prog);
        let rest = &s[1..];

        // 1. Tmux / screen / keystroke self-drive injection into terminal panes.
        if prog == "tmux"
            && rest.iter().any(|t| {
                matches!(
                    *t,
                    "send-keys" | "send" | "paste-buffer" | "load-buffer" | "set-buffer"
                )
            })
        {
            return true;
        }
        if prog == "screen"
            && rest.contains(&"-X")
            && rest.iter().any(|t| matches!(*t, "stuff" | "eval" | "readbuf" | "paste"))
        {
            return true;
        }
        if matches!(prog, "xdotool" | "ydotool" | "wtype" | "dotool") {
            return true;
        }
        if prog == "osascript"
            && rest
                .iter()
                .any(|t| t.contains("keystroke") || t.contains("key code"))
        {
            return true;
        }

        // 2. Spawning nested external agents with permission/sandbox bypass flags.
        if rest.iter().any(|t| {
            matches!(
                *t,
                "--dangerously-skip-permissions" | "--dangerously-bypass-approvals-and-sandbox"
            )
        }) || (matches!(prog, "claude" | "codex" | "gemini" | "agy" | "antigravity")
            && rest.contains(&"--yolo"))
        {
            return true;
        }

        // 3. Shell commands touching harness control-plane configs or session `.jsonl` records.
        if !matches!(prog, "echo" | "printf")
            && rest.iter().any(|t| is_control_plane_path(t))
        {
            return true;
        }
        false
    };

    let mut seg: Vec<&str> = Vec::new();
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

fn escapes_worktree_arg(tok: &str) -> bool {
    if tok.starts_with('-') {
        if let Some((_, val)) = tok.split_once('=') {
            return escapes_worktree_arg(val);
        }
        return false;
    }
    tok == "~"
        || tok.starts_with("~/")
        || tok.starts_with("$HOME")
        || tok.starts_with("${HOME}")
        || (tok.starts_with('/') && tok != "/dev/null")
        || tok == ".."
        || tok.starts_with("../")
        || tok.ends_with("/..")
        || tok.contains("/../")
}

fn has_outside_worktree_reference(cmd: &str) -> bool {
    let Some(tokens) = lex_shell(cmd) else {
        return true;
    };
    let mut i = 0;
    while i < tokens.len() {
        if let Token::Word(w) = &tokens[i] {
            if w == "-C" {
                return true;
            }
            if escapes_worktree_arg(w) {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn read_only(seg: &[&str]) -> bool {
    let Some(raw_program) = seg.first() else {
        return false;
    };
    let program = raw_program.rsplit('/').next().unwrap_or(raw_program);
    if program == "git" {
        return seg.get(1).is_some_and(|sub| GIT_READ.contains(sub))
            && !seg
                .iter()
                .any(|t| *t == "-C" || t.starts_with("--output") || t.starts_with("--git-dir") || t.starts_with("--work-tree"));
    }
    READ_ONLY.iter().any(|(name, writes)| {
        *name == program
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

/// Find the byte index of the matching `)` for `$(` starting at `open_paren_idx`
/// (`cmd.as_bytes()[open_paren_idx] == b'('`), respecting nested parens and quotes.
fn find_matching_paren(cmd: &str, open_paren_idx: usize) -> Option<usize> {
    let bytes = cmd.as_bytes();
    let mut i = open_paren_idx + 1;
    let mut depth = 1usize;
    let mut in_s = false;
    let mut in_d = false;
    while i < bytes.len() {
        let ch = cmd[i..].chars().next()?;
        let len = ch.len_utf8();
        if in_s {
            if ch == '\'' {
                in_s = false;
            }
            i += len;
            continue;
        }
        if in_d {
            if ch == '\\' && i + 1 < bytes.len() {
                let next = cmd[i + 1..].chars().next()?;
                i += 1 + next.len_utf8();
                continue;
            }
            if ch == '"' {
                in_d = false;
            } else if ch == '$' && bytes.get(i + 1) == Some(&b'(') {
                let close = find_matching_paren(cmd, i + 1)?;
                i = close + 1;
                continue;
            }
            i += len;
            continue;
        }
        match ch {
            '\\' if i + 1 < bytes.len() => {
                let next = cmd[i + 1..].chars().next()?;
                i += 1 + next.len_utf8();
                continue;
            }
            '\'' => in_s = true,
            '"' => in_d = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += len;
    }
    None
}

/// Find the byte index of the closing unescaped `` ` `` starting after `open_tick_idx`.
fn find_closing_backtick(cmd: &str, open_tick_idx: usize) -> Option<usize> {
    let bytes = cmd.as_bytes();
    let mut i = open_tick_idx + 1;
    while i < bytes.len() {
        let ch = cmd[i..].chars().next()?;
        if ch == '\\' && i + 1 < bytes.len() {
            let next = cmd[i + 1..].chars().next()?;
            i += 1 + next.len_utf8();
            continue;
        }
        if ch == '`' {
            return Some(i);
        }
        i += ch.len_utf8();
    }
    None
}

/// Quote-aware shell lexer that separates unquoted metacharacters even without
/// surrounding spaces, flags command/process substitutions with `Token::Unsafe`
/// (so `shape()` never marks them `Auto`), and recursively unpacks nested `$(...)`
/// and `` `...` `` bodies as separate segments so `Deny` and secret checks inspect them.
fn lex_shell(cmd: &str) -> Option<Vec<Token>> {
    lex_shell_depth(cmd, 0)
}

fn lex_shell_depth(cmd: &str, depth: usize) -> Option<Vec<Token>> {
    if depth > 8 {
        return None;
    }
    let bytes = cmd.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;
    let mut paren_depth = 0usize;
    let mut sub_tokens: Vec<Token> = Vec::new();

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
                '`' => {
                    let close = find_closing_backtick(cmd, i)?;
                    let inner = &cmd[i + 1..close];
                    sub_tokens.push(Token::Unsafe);
                    sub_tokens.push(Token::Op(";"));
                    sub_tokens.extend(lex_shell_depth(inner, depth + 1)?);
                    sub_tokens.push(Token::Op(";"));
                    i = close + 1;
                }
                '$' if bytes.get(i + 1) == Some(&b'(') => {
                    let close = find_matching_paren(cmd, i + 1)?;
                    let inner = &cmd[i + 2..close];
                    sub_tokens.push(Token::Unsafe);
                    sub_tokens.push(Token::Op(";"));
                    sub_tokens.extend(lex_shell_depth(inner, depth + 1)?);
                    sub_tokens.push(Token::Op(";"));
                    i = close + 1;
                }
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
            '`' => {
                flush(&mut cur, &mut has_token, &mut out);
                let close = find_closing_backtick(cmd, i)?;
                let inner = &cmd[i + 1..close];
                out.push(Token::Unsafe);
                out.push(Token::Op(";"));
                out.extend(lex_shell_depth(inner, depth + 1)?);
                out.push(Token::Op(";"));
                i = close + 1;
            }
            '$' | '<' | '>' if bytes.get(i + 1) == Some(&b'(') => {
                flush(&mut cur, &mut has_token, &mut out);
                let close = find_matching_paren(cmd, i + 1)?;
                let inner = &cmd[i + 2..close];
                out.push(Token::Unsafe);
                out.push(Token::Op(";"));
                out.extend(lex_shell_depth(inner, depth + 1)?);
                out.push(Token::Op(";"));
                i = close + 1;
            }
            '(' => {
                flush(&mut cur, &mut has_token, &mut out);
                paren_depth += 1;
                out.push(Token::Unsafe);
                out.push(Token::Op(";"));
                i += 1;
            }
            ')' => {
                if paren_depth == 0 {
                    return None;
                }
                flush(&mut cur, &mut has_token, &mut out);
                paren_depth -= 1;
                out.push(Token::Unsafe);
                out.push(Token::Op(";"));
                i += 1;
            }
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

    if in_single || in_double || paren_depth != 0 {
        return None; // unbalanced quotes or parens: fail closed
    }
    flush(&mut cur, &mut has_token, &mut out);
    if !sub_tokens.is_empty() {
        out.push(Token::Op(";"));
        out.extend(sub_tokens);
    }
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
    /// Catastrophic host/filesystem commands never fold target paths to `*`.
    pub fn allow_for_session(&self, cmd: &str) -> String {
        let key = if Self::is_catastrophic_deny(cmd)
            || touches_sensitive_secret(cmd)
            || matches_meta_escape(cmd)
        {
            cmd.trim().to_owned()
        } else {
            let sh = shape(cmd);
            if sh.is_empty() || sh == "<unsafe>" {
                cmd.trim().to_owned()
            } else {
                sh
            }
        };
        if !key.is_empty() {
            self.session_allows
                .lock()
                .expect("policy session allows")
                .insert(key.clone());
        }
        key
    }

    fn is_catastrophic_target(t: &str) -> bool {
        if matches!(
            t,
            "/" | "/*" | "~" | "~/" | "~/*" | "$HOME" | "${HOME}" | "*" | ".." | "/root" | "/etc" | "/usr" | "/var" | "/home" | "/Users"
        ) || t.starts_with("$HOME/")
            || t.starts_with("${HOME}/")
        {
            return true;
        }
        for root in ["/home/", "/Users/"] {
            if let Some(rest) = t.strip_prefix(root) {
                let trimmed = rest.trim_end_matches(['/', '*']);
                if !trimmed.is_empty() && !trimmed.contains('/') {
                    return true;
                }
            }
        }
        false
    }

    fn is_catastrophic_deny(cmd: &str) -> bool {
        if [
            "rm -rf /",
            "rm -rf ~",
            "rm -rf *",
            "dd of=/dev/",
            "mkfs",
            ":(){",
            "chmod -R 777 /",
            "shutdown",
            "reboot",
        ]
        .iter()
        .any(|d| cmd.contains(d))
        {
            return true;
        }
        Self::check_tokens(cmd, true)
    }

    /// Token-level destructive command check so flag reordering, extra whitespace,
    /// absolute binary paths (`/bin/rm`), subshells (`sh -c`), or short flags cannot slip past.
    fn matches_semantic_deny(cmd: &str) -> bool {
        Self::check_tokens(cmd, false)
    }

    fn check_tokens(cmd: &str, catastrophic_only: bool) -> bool {
        let Some(tokens) = lex_shell(cmd) else {
            return false;
        };
        let mut seg: Vec<&str> = Vec::new();
        let check_seg = |raw: &[&str]| -> bool {
            let mut s = raw;
            while let Some(first) = s.first() {
                let base = first.rsplit('/').next().unwrap_or(first);
                if matches!(base, "sudo" | "env" | "command" | "builtin" | "noglob")
                    || (first.contains('=') && !first.starts_with('-'))
                {
                    s = &s[1..];
                } else {
                    break;
                }
            }
            let Some(&raw_prog) = s.first() else {
                return false;
            };
            let prog = raw_prog.rsplit('/').next().unwrap_or(raw_prog);
            let rest = &s[1..];
            let has_short = |ch: char| {
                rest.iter().any(|t| {
                    t.starts_with('-') && !t.starts_with("--") && t[1..].contains(ch)
                })
            };
            match prog {
                "sh" | "bash" | "zsh" | "dash" => {
                    if let Some(pos) = rest.iter().position(|t| *t == "-c" || (*t != "--" && t.starts_with('-') && !t.starts_with("--") && t[1..].contains('c'))) {
                        if let Some(script) = rest.get(pos + 1) {
                            if HARD_DENY.iter().any(|d| script.contains(d))
                                || Self::check_tokens(script, catastrophic_only)
                            {
                                return true;
                            }
                        }
                    }
                    false
                }
                "shutdown" | "reboot" | "poweroff" | "halt" => true,
                p if p == "mkfs" || p.starts_with("mkfs.") => true,
                "dd" => rest.iter().any(|t| t.starts_with("of=/dev/")),
                "chmod" => {
                    let recursive = has_short('R') || rest.contains(&"--recursive");
                    recursive && rest.contains(&"777") && rest.iter().any(|t| Self::is_catastrophic_target(t))
                }
                "find" => {
                    rest.contains(&"-delete")
                        && rest.iter().any(|t| !t.starts_with('-') && Self::is_catastrophic_target(t))
                }
                "rm" => {
                    let recursive =
                        has_short('r') || has_short('R') || rest.contains(&"--recursive");
                    let force = has_short('f') || rest.contains(&"--force");
                    let dangerous_target = rest
                        .iter()
                        .any(|t| !t.starts_with('-') && Self::is_catastrophic_target(t));
                    recursive && (force || dangerous_target) && dangerous_target
                }
                "git" if !catastrophic_only => match rest.first().copied() {
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
        if !cmd.trim().is_empty() && lex_shell(cmd).is_none() {
            let allows = self.session_allows.lock().expect("policy session allows");
            return if allows.contains(cmd.trim()) {
                Tier::Audit
            } else {
                Tier::Deny
            };
        }
        let sh = shape(cmd);
        let exact_gate = touches_sensitive_secret(cmd) || matches_meta_escape(cmd);
        if HARD_DENY.iter().any(|d| cmd.contains(d))
            || Self::matches_semantic_deny(cmd)
            || exact_gate
        {
            let allows = self.session_allows.lock().expect("policy session allows");
            let allowed = if Self::is_catastrophic_deny(cmd) || exact_gate {
                allows.contains(cmd.trim())
            } else {
                allows.contains(&sh) || allows.contains(cmd.trim())
            };
            if allowed {
                return Tier::Audit;
            }
            return Tier::Deny;
        }
        let escapes = has_outside_worktree_reference(cmd);
        if !escapes {
            if let Some(tier) = self.cache.lock().expect("policy cache").get(&sh) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return *tier;
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let tier = if escapes {
            Tier::Audit
        } else {
            self.compound(&sh)
        };
        if !escapes {
            self.cache
                .lock()
                .expect("policy cache")
                .insert(sh, tier);
        }
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

    #[test]
    fn outside_worktree_reads_downgrade_to_audit_and_catastrophic_denies_never_wildcard() {
        let p = Policy::new();
        // S1: outside-worktree reads must require Audit even though `cat *` is Auto inside worktree.
        assert_eq!(p.classify("cat src/lib.rs"), Tier::Auto);
        assert_eq!(p.classify("cat .env.example"), Tier::Auto);
        assert_eq!(p.classify("cat ~/.bashrc"), Tier::Audit);
        assert_eq!(p.classify("cat ../notes.txt"), Tier::Audit);
        assert_eq!(p.classify("cat /etc/passwd"), Tier::Audit);
        assert_eq!(p.classify("git -C /tmp/other status"), Tier::Audit);
        assert_eq!(p.classify("rg token $HOME/.config"), Tier::Audit);

        // Sensitive credentials (.env, private keys) pause for explicit human approval (`Deny`)
        // and never wildcard across distinct secret files.
        assert_eq!(p.classify("cat .env"), Tier::Deny);
        assert_eq!(p.classify("head -n 20 .env.production"), Tier::Deny);
        assert_eq!(p.classify("cat ~/.ssh/id_rsa"), Tier::Deny);
        assert_eq!(p.classify("cat certs/server.key"), Tier::Deny);
        assert_eq!(p.allow_for_session("cat .env.dev"), "cat .env.dev");
        assert_eq!(p.classify("cat .env.dev"), Tier::Audit);
        assert_eq!(p.classify("cat .env.production"), Tier::Deny);

        // S2: path-prefixed binaries, subshells, find -delete, and /home/<user> are caught.
        assert_eq!(p.classify("/bin/rm -rf /"), Tier::Deny);
        assert_eq!(p.classify("/usr/bin/git push --force origin main"), Tier::Deny);
        assert_eq!(p.classify("sh -c 'rm -rf ~'"), Tier::Deny);
        assert_eq!(p.classify("rm -rf /home/alice"), Tier::Deny);
        assert_eq!(p.classify("find / -delete"), Tier::Deny);

        // Allowing one catastrophic target must never wildcard `rm -rf *` to allow `rm -rf /`.
        assert_eq!(p.allow_for_session("rm -rf /home/alice"), "rm -rf /home/alice");
        assert_eq!(p.classify("rm -rf /"), Tier::Deny);
        // Repo-level denies like `git reset --hard` still fold arguments cleanly for session memory.
        assert_eq!(p.allow_for_session("git reset --hard HEAD"), "git reset --hard *");
        assert_eq!(p.classify("git reset --hard origin/main"), Tier::Audit);

        // Meta-escape vectors (tmux keystroke injection, unsafe nested agent flags, control-plane tampering)
        // pause at `Tier::Deny` for human choice and never wildcard across distinct commands.
        assert_eq!(p.classify("tmux send-keys -t 0 '1' Enter"), Tier::Deny);
        assert_eq!(p.classify("xdotool key Return"), Tier::Deny);
        assert_eq!(p.classify("claude -p 'do x' --dangerously-skip-permissions"), Tier::Deny);
        assert_eq!(p.classify("gemini -p 'do x' --yolo"), Tier::Deny);
        assert_eq!(p.classify("cat ~/.local/state/twig/sessions/s1.jsonl"), Tier::Deny);
        assert_eq!(p.classify("cp evil.json .twig/mcp.json"), Tier::Deny);
        assert_eq!(p.classify("cp hook.sh .git/hooks/pre-commit"), Tier::Deny);
        assert_eq!(p.classify("cp hook.sh .husky/pre-commit"), Tier::Deny);
        assert_eq!(p.classify("tmux list-sessions"), Tier::Audit);
    }

    #[test]
    fn command_substitutions_and_subshells_cannot_bypass_deny_or_secret_gates() {
        let p = Policy::new();
        assert_eq!(p.classify("echo $(rm -r -f /)"), Tier::Deny);
        assert_eq!(p.classify("echo $(/bin/rm -fr /)"), Tier::Deny);
        assert_eq!(p.classify("echo \"$(/bin/rm -fr ~)\""), Tier::Deny);
        assert_eq!(p.classify("echo `rm -r -f /`"), Tier::Deny);
        assert_eq!(p.classify("(rm -r -f /)"), Tier::Deny);
        assert_eq!(p.classify("echo $(cat \"$HOME/.ssh/id_rsa\")"), Tier::Deny);
        assert_eq!(p.classify("echo \"$(cat \"$HOME/.ssh/id_rsa\")\""), Tier::Deny);
        assert_eq!(p.classify("echo `cat .env`"), Tier::Deny);
        assert_eq!(p.classify("cat <(cat .env.production)"), Tier::Deny);
        assert_eq!(p.classify("echo $(cp evil.json .twig/project.json)"), Tier::Deny);
        // Unbalanced quotes or substitutions fail closed to Deny.
        assert_eq!(p.classify("echo $(rm -rf /"), Tier::Deny);
        assert_eq!(p.classify("echo 'unterminated"), Tier::Deny);
        // Benign subshells remain Audit (not falsely blocked, not Auto).
        assert_eq!(p.classify("(cd src && cargo test)"), Tier::Audit);
    }
}
