//! Masking for what `pods ask` sends off this machine: every text field of a pod's bundle goes
//! through here before it reaches a prompt.
//!
//! Three layers, from the most specific to the bluntest:
//! - [`secrets`]: the backup's own key patterns ([`crate::backup::SECRET_PATTERNS`], one list for
//!   git and for this), applied exactly as the pods' `sed -E` applies them — the same leads, the
//!   same replacements (`REDACTED-OPENROUTER-KEY`, …), held to GNU sed by a differential test.
//!   There's no regex crate in this workspace, so [`Ere`] reads the small subset of POSIX ERE
//!   those patterns use (literals, bracket expressions, `( | )`, `{n}`/`{n,}`/`{n,m}`/`*`/`+`/`?`
//!   on one character, `^`, `$`, `\b`); a pattern using anything else fails to compile, which a
//!   test catches and [`ready`] refuses at run time — never a silently unredacted bundle.
//! - [`named_values`]: the value of anything *named* like a secret — `HF_TOKEN=…`,
//!   `--api-key …`, `password=…`, `Bearer …`, `https://user:pass@…` — whatever its shape (a
//!   short password matches no pattern).
//! - [`long_tokens`]: any run of 32 or more `[A-Za-z0-9_-]` — the shape of almost every API key,
//!   session id and bearer token — becomes [`MASK`]. Applied to free text (process arguments,
//!   log lines, commands, commit subjects), not to names (a branch, a file name), which can
//!   legitimately run that long and carry no secret of their own.
//!
//! Everything here is pure.

use std::sync::OnceLock;

use crate::backup::SECRET_PATTERNS;

/// What a masked value becomes (as `arena api get` shows one).
pub const MASK: &str = crate::apiget::REDACTED;

/// The shortest run of `[A-Za-z0-9_-]` that [`long_tokens`] masks.
pub const LONG_TOKEN: usize = 32;

/// What a private-key block in a log becomes ([`log_lines`]).
pub const PEM_MASK: &str = "<redacted: private key block>";

type Set = Box<[bool; 256]>;

/// One element of a parsed pattern.
#[derive(Debug, Clone)]
enum Node {
    /// One byte out of a set: a literal, a bracket expression, `.`.
    Set(Set),
    /// A set repeated `min..=max` times (greedy).
    Repeat(Set, usize, Option<usize>),
    Start,
    End,
    WordBoundary,
    /// `( a | b | … )`.
    Group(Vec<Vec<Node>>),
}

/// A compiled pattern: the subset of POSIX ERE [`SECRET_PATTERNS`] is written in (module doc),
/// matched on bytes as `LC_ALL=C` GNU sed does. Backtracking, greedy; the patterns' shape (a
/// fixed prefix, then classes) keeps that linear in practice.
#[derive(Debug, Clone)]
pub struct Ere(Vec<Vec<Node>>);

impl Ere {
    /// Compile `re`, or say what in it isn't supported.
    pub fn parse(re: &str) -> Result<Ere, String> {
        let mut p = Parser { s: re.as_bytes(), i: 0 };
        let alts = p.alternation().map_err(|e| format!("{e} in `{re}`"))?;
        if p.i != p.s.len() {
            return Err(format!("an unmatched `)` in `{re}`"));
        }
        Ok(Ere(alts))
    }

    /// Whether the pattern matches `t` starting exactly at `i`, calling `k` with each end it can
    /// reach (longest first) until `k` accepts one.
    fn at(&self, t: &[u8], i: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
        for alt in &self.0 {
            if seq(alt, t, i, k) {
                return true;
            }
        }
        false
    }

    /// Whether the pattern matches anywhere in `t`.
    pub fn is_match(&self, t: &str) -> bool {
        let b = t.as_bytes();
        (0..=b.len()).any(|i| self.at(b, i, &mut |_| true))
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

fn single(b: u8) -> Set {
    let mut s = Box::new([false; 256]);
    s[b as usize] = true;
    s
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn alternation(&mut self) -> Result<Vec<Vec<Node>>, String> {
        let mut alts = vec![self.sequence()?];
        while self.peek() == Some(b'|') {
            self.i += 1;
            alts.push(self.sequence()?);
        }
        Ok(alts)
    }

    fn sequence(&mut self) -> Result<Vec<Node>, String> {
        let mut out = Vec::new();
        while let Some(c) = self.peek() {
            if c == b'|' || c == b')' {
                break;
            }
            self.i += 1;
            let node = match c {
                b'(' => {
                    let g = self.alternation()?;
                    if self.peek() != Some(b')') {
                        return Err("an unclosed `(`".into());
                    }
                    self.i += 1;
                    Node::Group(g)
                }
                b'[' => Node::Set(self.bracket()?),
                b'\\' => {
                    let e = self.peek().ok_or("a trailing `\\`")?;
                    self.i += 1;
                    match e {
                        b'b' => Node::WordBoundary,
                        e if e.is_ascii_alphanumeric() => return Err(format!("the unsupported escape `\\{}`", e as char)),
                        e => Node::Set(single(e)),
                    }
                }
                b'^' => Node::Start,
                b'$' => Node::End,
                b'.' => Node::Set(Box::new([true; 256])),
                b'*' | b'+' | b'?' | b'{' => return Err("a quantifier with nothing to repeat".into()),
                b => Node::Set(single(b)),
            };
            let node = match (node, self.quantifier()?) {
                (n, None) => n,
                (Node::Set(set), Some((min, max))) => Node::Repeat(set, min, max),
                (_, Some(_)) => return Err("a quantifier on a group or an anchor (unsupported)".into()),
            };
            out.push(node);
        }
        Ok(out)
    }

    fn quantifier(&mut self) -> Result<Option<(usize, Option<usize>)>, String> {
        let q = match self.peek() {
            Some(b'*') => (0, None),
            Some(b'+') => (1, None),
            Some(b'?') => (0, Some(1)),
            Some(b'{') => {
                let close = self.s[self.i..].iter().position(|&b| b == b'}').ok_or("an unclosed `{`")? + self.i;
                let body = std::str::from_utf8(&self.s[self.i + 1..close]).map_err(|_| "a bad `{…}`")?;
                let num = |s: &str| s.parse::<usize>().map_err(|_| format!("the bad repeat `{{{body}}}`"));
                let q = match body.split_once(',') {
                    None => {
                        let n = num(body)?;
                        (n, Some(n))
                    }
                    Some((a, "")) => (num(a)?, None),
                    Some((a, b)) => (num(a)?, Some(num(b)?)),
                };
                if q.1.is_some_and(|max| max < q.0) {
                    return Err(format!("the empty repeat `{{{body}}}`"));
                }
                self.i = close;
                q
            }
            _ => return Ok(None),
        };
        self.i += 1;
        if matches!(self.peek(), Some(b'*' | b'+' | b'?' | b'{')) {
            return Err("stacked quantifiers (unsupported)".into());
        }
        Ok(Some(q))
    }

    /// After `[`: up to the closing `]` (a `]` first is literal, as is a `-` first or last).
    fn bracket(&mut self) -> Result<Set, String> {
        let negate = self.peek() == Some(b'^');
        if negate {
            self.i += 1;
        }
        let mut set = Box::new([false; 256]);
        let mut first = true;
        loop {
            let c = self.peek().ok_or("an unclosed `[`")?;
            if c == b']' && !first {
                self.i += 1;
                break;
            }
            if c == b'[' && matches!(self.s.get(self.i + 1), Some(b':' | b'=' | b'.')) {
                return Err("a `[:class:]` (unsupported)".into());
            }
            self.i += 1;
            first = false;
            match (self.peek(), self.s.get(self.i + 1)) {
                (Some(b'-'), Some(&hi)) if hi != b']' => {
                    if hi < c {
                        return Err("a reversed range".into());
                    }
                    self.i += 2;
                    (c..=hi).for_each(|b| set[b as usize] = true);
                }
                _ => set[c as usize] = true,
            }
        }
        if negate {
            set.iter_mut().for_each(|b| *b = !*b);
        }
        Ok(set)
    }
}

/// A word byte, as `\b` reads one in the C locale.
fn is_word(t: &[u8], i: Option<usize>) -> bool {
    i.and_then(|i| t.get(i)).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn seq(nodes: &[Node], t: &[u8], i: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    let Some((first, rest)) = nodes.split_first() else { return k(i) };
    match first {
        Node::Set(set) => t.get(i).is_some_and(|b| set[*b as usize]) && seq(rest, t, i + 1, k),
        Node::Repeat(set, min, max) => {
            let mut n = 0;
            while max.is_none_or(|m| n < m) && t.get(i + n).is_some_and(|b| set[*b as usize]) {
                n += 1;
            }
            if n < *min {
                return false;
            }
            for c in (*min..=n).rev() {
                if seq(rest, t, i + c, k) {
                    return true;
                }
            }
            false
        }
        Node::Start => i == 0 && seq(rest, t, i, k),
        Node::End => i == t.len() && seq(rest, t, i, k),
        Node::WordBoundary => is_word(t, i.checked_sub(1)) != is_word(t, Some(i)) && seq(rest, t, i, k),
        Node::Group(alts) => {
            for alt in alts {
                if seq(alt, t, i, &mut |j| seq(rest, t, j, k)) {
                    return true;
                }
            }
            false
        }
    }
}

/// One of [`SECRET_PATTERNS`], compiled: its lead (kept, as sed's `\1`), the key, what the key
/// becomes.
#[derive(Debug, Clone)]
pub struct Compiled {
    lead: Ere,
    re: Ere,
    replacement: &'static str,
}

/// Every [`SECRET_PATTERNS`] entry compiled, once — or why one can't be.
pub fn compiled() -> Result<&'static [Compiled], String> {
    static ALL: OnceLock<Result<Vec<Compiled>, String>> = OnceLock::new();
    ALL.get_or_init(|| {
        SECRET_PATTERNS
            .iter()
            .map(|p| {
                let what = |e: String| format!("secret pattern `{}`: {e}", p.name);
                Ok(Compiled { lead: Ere::parse(p.lead).map_err(what)?, re: Ere::parse(p.re).map_err(what)?, replacement: p.replacement })
            })
            .collect()
    })
    .as_ref()
    .map(Vec::as_slice)
    .map_err(Clone::clone)
}

/// `Ok` when every secret pattern applies here — what `pods ask` checks before it probes
/// anything: a bundle it couldn't redact is never built.
pub fn ready() -> Result<(), String> {
    compiled().map(|_| ())
}

/// One line through one pattern, as `sed -E 's#<lead><re>#\1<replacement>#g'`: the leftmost
/// match, its lead kept, the search going on after it.
fn redact_line(line: &[u8], c: &Compiled, out: &mut Vec<u8>) {
    let (mut i, mut copied) = (0, 0);
    while i <= line.len() {
        let mut found = None;
        c.lead.at(line, i, &mut |le| {
            c.re.at(line, le, &mut |e| {
                found = Some((le, e));
                true
            })
        });
        match found {
            Some((le, e)) if e > i => {
                out.extend_from_slice(&line[copied..le]);
                out.extend_from_slice(c.replacement.as_bytes());
                copied = e;
                i = e;
            }
            _ => i += 1,
        }
    }
    out.extend_from_slice(&line[copied..]);
}

/// `text` with every key [`SECRET_PATTERNS`] knows replaced by its `REDACTED-…` text, line by
/// line, pattern by pattern — byte for byte what the backup's `sed -E` program makes of it.
/// If a pattern couldn't be compiled, the whole text is [`MASK`] (fail closed; [`ready`] says
/// why up front).
pub fn secrets(text: &str) -> String {
    let Ok(all) = compiled() else { return MASK.to_string() };
    let mut cur: Vec<u8> = text.as_bytes().to_vec();
    for c in all {
        let mut next = Vec::with_capacity(cur.len());
        for (n, line) in cur.split(|&b| b == b'\n').enumerate() {
            if n > 0 {
                next.push(b'\n');
            }
            redact_line(line, c, &mut next);
        }
        cur = next;
    }
    // Splices happen only at ASCII bytes (each key starts with one), so this never replaces
    // anything; lossy only to keep the type honest.
    String::from_utf8_lossy(&cur).into_owned()
}

fn token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// `text` with every run of at least [`LONG_TOKEN`] `[A-Za-z0-9_-]` replaced by [`MASK`].
pub fn long_tokens(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        if !token_byte(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && token_byte(b[i]) {
            i += 1;
        }
        if i - start >= LONG_TOKEN {
            out.push_str(&text[copied..start]);
            out.push_str(MASK);
            copied = i;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Whether a variable, flag or field called `name` holds a secret by its name. Broad on
/// purpose — over-masking a harmless value costs a word in the prompt.
fn secret_name(name: &str) -> bool {
    let n = name.trim_start_matches('-').to_ascii_lowercase();
    ["token", "secret", "passw", "passwd", "pwd", "credential", "auth", "cookie", "apikey", "api_key", "api-key", "private"]
        .iter()
        .any(|w| n.contains(w))
        || n == "key"
        || n.ends_with("_key")
        || n.ends_with("-key")
        || n.ends_with(".key")
}

/// The `name` of a `name=value` word (after any leading `-`/`--` and an `export`-free shape):
/// letters, digits, `_`, `-`, `.`.
fn assignment(word: &str) -> Option<(&str, &str)> {
    let (name, value) = word.split_once('=')?;
    let bare = name.trim_start_matches('-');
    (!bare.is_empty() && bare.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))).then_some((name, value))
}

/// `scheme://user:password@host` → `scheme://<redacted>@host`, in one word.
fn url_credentials(word: &str) -> Option<String> {
    let at = word.find("://")? + 3;
    let rest = &word[at..];
    let end = rest.find('@')?;
    let cred = &rest[..end];
    (!cred.is_empty() && !cred.contains('/') && cred.contains(':')).then(|| format!("{}{MASK}{}", &word[..at], &rest[end..]))
}

/// `text` with the values of secret-named things masked: `NAME=value` and `--name=value`
/// words, a `--name value` / `-name value` pair, the word after `Bearer` (and after
/// `Authorization: Basic`), and the credentials in a URL. Whitespace is kept as it was.
pub fn named_values(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut mask_next, mut after_authorization) = (false, false);
    let mut rest = text;
    while !rest.is_empty() {
        let ws = rest.len() - rest.trim_start().len();
        out.push_str(&rest[..ws]);
        rest = &rest[ws..];
        if rest.is_empty() {
            break;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        rest = &rest[end..];
        if mask_next && !word.starts_with('-') {
            out.push_str(MASK);
            mask_next = false;
            continue;
        }
        let bare = word.trim_matches(|c| c == '"' || c == '\'').trim_end_matches(':');
        let basic = after_authorization && bare.eq_ignore_ascii_case("basic");
        after_authorization = bare.eq_ignore_ascii_case("authorization");
        mask_next = false;
        if let Some((name, _)) = assignment(word.trim_matches(|c| c == '"' || c == '\'')).filter(|(n, v)| secret_name(n) && !v.is_empty()) {
            out.push_str(&format!("{name}={MASK}"));
        } else if let Some(masked) = url_credentials(word) {
            out.push_str(&masked);
        } else {
            out.push_str(word);
            let flag = word.starts_with('-') && !word.contains('=') && secret_name(word);
            mask_next = flag || basic || bare.eq_ignore_ascii_case("bearer");
        }
    }
    out
}

/// One line of pod text made safe to show and send: line breaks and tabs are spaces, other
/// control characters and escape sequences gone ([`crate::jobs::sanitize`]), whitespace runs one
/// space.
fn one_line(s: &str) -> String {
    let spaced: String = s.chars().map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c }).collect();
    crate::jobs::sanitize(&spaced).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A name from a pod (a branch, a file, a tmux session, a GPU process) for the prompt: on one
/// line, keys out ([`secrets`], before and after the escapes go — a key glued to a colour code
/// is found either way), clipped to `max` characters.
pub fn name(s: &str, max: usize) -> String {
    crate::fleet::clip(&secrets(&one_line(&secrets(s))), max)
}

/// Free text from a pod (a process's arguments, a log line, a command, a commit subject) for
/// the prompt: [`name`]'s treatment, then the secret-named values ([`named_values`]) and any
/// long token ([`long_tokens`]) masked, then clipped to `max` characters.
pub fn text(s: &str, max: usize) -> String {
    crate::fleet::clip(&long_tokens(&named_values(&secrets(&one_line(&secrets(s))))), max)
}

/// A log's lines for the prompt, each through [`text`] — and a private-key block (`-----BEGIN
/// … PRIVATE KEY-----` to its `-----END`, or to the end) as one [`PEM_MASK`] line: its base64
/// lines are broken up by `+` and `/`, so no single run need reach [`LONG_TOKEN`].
pub fn log_lines(lines: &[String], max: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut in_pem = false;
    for l in lines {
        if in_pem {
            in_pem = !l.contains("-----END");
            continue;
        }
        if l.contains("-----BEGIN") && l.contains("PRIVATE KEY") {
            out.push(PEM_MASK.to_string());
            in_pem = !l.contains("-----END");
            continue;
        }
        out.push(text(l, max));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::backup::{redact_sed, SECRET_PATTERNS};

    /// `n` characters of `alphabet`, deterministic per `seed`. Keys are built at run time: a
    /// key-shaped literal here would be refused by GitHub's push protection (as in backup.rs).
    pub(crate) fn body(alphabet: &str, n: usize, seed: usize) -> String {
        let a: Vec<char> = alphabet.chars().collect();
        (0..n).map(|i| a[(i * 7 + seed * 13 + i / 3) % a.len()]).collect()
    }
    pub(crate) const HEX: &str = "0123456789abcdef";
    pub(crate) const ALNUM: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    pub(crate) const LETTERS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    pub(crate) const URLSAFE: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";

    /// One key of every [`SECRET_PATTERNS`] family (and both OpenAI shapes, every GitHub prefix),
    /// with the family's replacement: what the bundle tests plant in process args and logs.
    pub(crate) fn one_key_per_family() -> Vec<(String, &'static str)> {
        let ant = |kind: &str| format!("{}{kind}-{}AA", "sk-ant-", body(URLSAFE, 93, 3));
        let gh = |p: &str| format!("{p}_{}", body(ALNUM, 36, 4));
        vec![
            (format!("{}{}", "sk-or-v1-", body(HEX, 64, 1)), "REDACTED-OPENROUTER-KEY"),
            (ant("api03"), "REDACTED-ANTHROPIC-KEY"),
            (ant("oat01"), "REDACTED-ANTHROPIC-KEY"),
            (format!("{}{}", "sk-proj-", body(URLSAFE, 120, 7)), "REDACTED-OPENAI-KEY"),
            (format!("sk-{}T3BlbkFJ{}", body(ALNUM, 20, 10), body(ALNUM, 20, 11)), "REDACTED-OPENAI-KEY"),
            (format!("{}{}", "hf_", body(LETTERS, 34, 5)), "REDACTED-HUGGINGFACE-TOKEN"),
            (gh("ghp"), "REDACTED-GITHUB-TOKEN"),
            (gh("ghs"), "REDACTED-GITHUB-TOKEN"),
            (format!("{}{}_{}", "github_pat_", body(ALNUM, 22, 12), body(ALNUM, 59, 13)), "REDACTED-GITHUB-TOKEN"),
            (format!("{}{}", "AIza", body(URLSAFE, 35, 14)), "REDACTED-GOOGLE-API-KEY"),
            (format!("{}{}", "gsk_", body(ALNUM, 52, 15)), "REDACTED-GROQ-KEY"),
            (format!("{}{}", "AKIA", body("ABCDEFGHIJKLMNOPQRSTUVWXYZ0234567", 16, 6)), "REDACTED-AWS-ACCESS-KEY-ID"),
        ]
    }

    /// Every pattern the backup redacts with compiles here — a pattern added there in syntax
    /// this reader doesn't know fails this test, not a live `pods ask` (which refuses then).
    #[test]
    fn every_secret_pattern_compiles() {
        assert_eq!(compiled().unwrap().len(), SECRET_PATTERNS.len());
        assert!(ready().is_ok());
        // And what isn't supported is refused, never misread.
        for bad in [r"\w+", r"(ab)+", r"[[:alpha:]]", r"a{3", r"x**", r"[z-a]", "(a", "a)", r"\d"] {
            assert!(Ere::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_matcher_reads_ere() {
        let cases: &[(&str, &str, bool)] = &[
            ("ab{2,3}c", "xabbcx", true),
            ("ab{2,3}c", "abc", false),
            ("ab{2,3}c", "abbbbc", false),
            ("^a", "ba", false),
            ("a$", "ba", true),
            (r"x\b", "x y", true),
            (r"x\b", "xy", false),
            ("(cat|dog)s?", "hotdog", true),
            (r"\\[nrt]k", r"a\nk", true),
            (r"\[[0-9;]*m", "\u{1b}[1;32m", true),
            ("[^A-Za-z0-9_]k", "_k", false),
            ("[^A-Za-z0-9_]k", "-k", true),
            ("[A-Za-z0-9_-]{4}", "a-_b", true),
        ];
        for (re, t, want) in cases {
            assert_eq!(Ere::parse(re).unwrap().is_match(t), *want, "{re} on {t:?}");
        }
    }

    /// The patterns as GNU sed applies them — the program every pod's backup runs — and as
    /// [`secrets`] does: byte-identical over every family in the places they're pasted, the
    /// lookalikes, and lines with several keys or none.
    #[cfg(unix)]
    #[test]
    fn secrets_matches_gnu_sed_byte_for_byte() {
        use std::io::Write;
        let keys = one_key_per_family();
        let mut lines: Vec<String> = Vec::new();
        for (k, _) in &keys {
            lines.push(k.clone());
            lines.push(format!("export X={k}"));
            lines.push(format!("python train.py --key {k} --lr 3e-4"));
            lines.push(format!("    \"key = '{k}'\\n\","));
            lines.push(format!("\u{1b}[32m{k}\u{1b}[0m"));
            lines.push(format!("GET /v1?key=%22{k}%22"));
            lines.push(format!("x{k}"));
            lines.push(format!("{k}{k}"));
            lines.push(format!("café {k} ü"));
        }
        lines.push(format!("a={} b={};", keys[0].0, keys[5].0));
        lines.extend(
            [
                "from sklearn.linear_model import LogisticRegression",
                "task-or-v1-0123 sk-learn hf_hub_download",
                "plain text 64.119.209.250:23924",
                "",
            ]
            .map(String::from),
        );
        lines.push(format!("{}{}", "sk-or-v1-", body(HEX, 63, 1)));
        lines.push(format!("iVBORw0KGgo/{}{}", "AIza", body(ALNUM, 35, 14)));
        let input = lines.join("\n") + "\n";
        let mut c = std::process::Command::new("sed")
            .args(["-E", &redact_sed()])
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("sed");
        c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let sed = String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap();
        let ours = secrets(&input);
        for (n, (a, b)) in sed.lines().zip(ours.lines()).enumerate() {
            assert_eq!(b, a, "line {n}: {}", &lines[n][..lines[n].len().min(60)]);
        }
        assert_eq!(ours, sed);
        // Every family was actually replaced (the differential test alone would pass a no-op
        // on both sides only if sed did nothing — it doesn't, but say it).
        for (k, repl) in &keys {
            let out = secrets(&format!("k={k}"));
            assert_eq!(out, format!("k={repl}"), "{repl}");
        }
    }

    #[test]
    fn long_tokens_masks_runs_from_32() {
        let t40 = body(URLSAFE, 40, 2);
        let t32 = body(ALNUM, 32, 3);
        let t31 = body(ALNUM, 31, 3);
        assert_eq!(long_tokens(&format!("--session {t40} x")), format!("--session {MASK} x"));
        assert_eq!(long_tokens(&t32), MASK);
        assert_eq!(long_tokens(&format!("a/{t31}/b")), format!("a/{t31}/b"));
        assert_eq!(long_tokens("/opt/conda/envs/arena-env/bin/python -m pytest -x"), "/opt/conda/envs/arena-env/bin/python -m pytest -x");
        assert_eq!(long_tokens(&format!("é{t40}é")), format!("é{MASK}é"));
    }

    #[test]
    fn named_values_masks_what_is_named_secret() {
        let cases = [
            ("HF_TOKEN=abc123 python x.py", "HF_TOKEN=<redacted> python x.py"),
            ("prog --api-key short1 --lr 1", "prog --api-key <redacted> --lr 1"),
            ("prog --password=hunter2", "prog --password=<redacted>"),
            ("curl -H 'Authorization: Bearer abc' u", "curl -H 'Authorization: Bearer <redacted> u"),
            ("-H 'Authorization: Basic dXNlcjpw'", "-H 'Authorization: Basic <redacted>"),
            ("Basic usage: run it", "Basic usage: run it"),
            ("git clone https://user:pw@github.com/x/y", "git clone https://<redacted>@github.com/x/y"),
            ("MY_SECRET='x y'", "MY_SECRET=<redacted> y'"),
            ("python train.py --lr 3e-4 --epochs=2", "python train.py --lr 3e-4 --epochs=2"),
            ("ssh git@github.com", "ssh git@github.com"),
            ("--token", "--token"),
        ];
        for (input, want) in cases {
            assert_eq!(named_values(input), want, "{input}");
        }
    }

    #[test]
    fn text_and_name_clean_and_clip() {
        let k = format!("{}{}", "hf_", body(LETTERS, 34, 5));
        assert_eq!(text(&format!("a\tb\n\u{1b}[31m{k}\u{1b}[0m"), 200), "a b REDACTED-HUGGINGFACE-TOKEN");
        // A branch or a file name keeps its length — only keys go.
        let branch = "autocommit-devtest-w1d2-alpha-bravo-charlie";
        assert_eq!(name(branch, 100), branch);
        assert_eq!(text(branch, 100), MASK);
        assert_eq!(name(&format!("notes-{k}.txt"), 100), "notes-REDACTED-HUGGINGFACE-TOKEN.txt");
        assert_eq!(text(&"x ".repeat(100), 11).chars().count(), 11);
    }

    #[test]
    fn a_private_key_block_in_a_log_is_one_line() {
        let lines: Vec<String> = [
            "loading",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW",
            "QyNTUxOQAAACD+/+/+/+abc",
            "-----END OPENSSH PRIVATE KEY-----",
            "done",
            "-----BEGIN RSA PRIVATE KEY----- cut",
            "MIIEow+x/y",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(log_lines(&lines, 200), vec!["loading", PEM_MASK, "done", PEM_MASK]);
    }
}
