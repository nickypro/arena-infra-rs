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
//! - [`named_values`]: the whole value of anything *named* like a secret, whatever its shape (a
//!   short password matches no pattern): `HF_TOKEN=…` and `MY_SECRET='x y'`, `--api-key …`,
//!   JSON/YAML/Python fields (`"password": "a b"`, `'token': '…'`, `api_key: …`, `password:x`),
//!   `Bearer …`, basic auth (`curl -u user:pass`, `--user user:pass`, `https://user:pass@…`), a
//!   command's password option (`redis-cli -a …`, `sshpass -p …`, `mysql -p…`). A value it can't
//!   find the end of (an unclosed quote or bracket), or whose quoting it can't tell, makes the
//!   whole line [`LINE_MASK`]; in a log, the lines such a value runs on into go too
//!   ([`CONT_MASK`]).
//! - [`long_tokens`]: any run of 32 or more `[A-Za-z0-9_-]` — the shape of almost every API key,
//!   session id and bearer token — becomes [`MASK`], and so does base64 key material
//!   ([`base64_runs`]) and a private-key block ([`PEM_MASK`]). Applied to free text (process
//!   arguments, log lines, commands, commit subjects), not to names (a branch, a file name), which
//!   can legitimately run that long and carry no secret of their own.
//!
//! Everything here is pure.

use std::sync::OnceLock;

use crate::backup::SECRET_PATTERNS;

/// What a masked value becomes (as `arena api get` shows one).
pub const MASK: &str = crate::apiget::REDACTED;

/// The shortest run of `[A-Za-z0-9_-]` that [`long_tokens`] masks.
pub const LONG_TOKEN: usize = 32;

/// What a private-key block in a log becomes ([`log_tail`]).
pub const PEM_MASK: &str = "<redacted: private key block>";

/// What a line becomes when [`named_values`] finds a secret-named value it can't delimit (an
/// unclosed quote or bracket: the value may run on past this line, so nothing of it is kept) or
/// whose quoting it can't tell (an apostrophe before a quoted value: `Couldn't … PASSWORD='a b'`).
pub const LINE_MASK: &str = "<redacted: a line holding a secret that couldn't be masked>";

/// What a log's lines become where a secret's value runs on from a line above ([`log_tail`]): a
/// YAML block scalar or a value on the lines below its name, an unclosed quote or bracket.
pub const CONT_MASK: &str = "<redacted: a secret's value continued on these lines>";

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
    ["token", "secret", "passw", "passphrase", "pwd", "credential", "auth", "cookie", "apikey", "api_key", "api-key", "private"]
        .iter()
        .any(|w| n.contains(w))
        || n == "key"
        || n.ends_with("_key")
        || n.ends_with("-key")
        || n.ends_with(".key")
        // `--pass`, `DB_PASS`, `redis.pass` — a word of its own, not `bypass` or `--num-passes`.
        || n.split(['_', '-', '.']).any(|w| w == "pass")
}

/// A byte of a name: a variable, a flag (its leading `-`s too), a JSON/YAML key.
fn name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

fn quote_byte(b: u8) -> bool {
    matches!(b, b'"' | b'\'')
}

/// The quote each byte of `b` sits inside, shell-style (`"` with backslash escapes, `'` with
/// none), read from the start of the line: `state[i]` is the quote open just before byte `i`.
fn quote_states(b: &[u8]) -> Vec<Option<u8>> {
    let mut states = Vec::with_capacity(b.len() + 1);
    let (mut open, mut escaped) = (None, false);
    for &c in b {
        states.push(open);
        match open {
            Some(b'"') if escaped => escaped = false,
            Some(b'"') if c == b'\\' => escaped = true,
            Some(q) if c == q => open = None,
            Some(_) => {}
            None if quote_byte(c) => open = Some(c),
            None => {}
        }
    }
    states.push(open);
    states
}

/// Where the quoted run opened by `b[at]` ends (just past its closing quote): backslash escapes
/// count inside `"`, and inside `'` too when `escapes` (JSON, Python, YAML — not the shell).
/// `None`: it never closes on this line.
fn quoted_end(b: &[u8], at: usize, escapes: bool) -> Option<usize> {
    let q = b[at];
    let mut i = at + 1;
    while i < b.len() {
        match b[i] {
            b'\\' if q == b'"' || escapes => i += 2,
            c if c == q => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Where the bracketed run opened by `b[at]` (`(`, `[`, `{`) closes, quotes inside skipped.
/// `None`: not on this line.
fn bracket_end(b: &[u8], at: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = at;
    while i < b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            q if quote_byte(q) => {
                i = quoted_end(b, i, true)?;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The end of a shell word's value starting at `at`: up to whitespace, or the quote `enclosing`
/// the whole assignment (`echo "HF_TOKEN=abc"`), through any quoted part it opens itself
/// (`X='a b'`, `X=a"b c"`). `None`: a quote it opens never closes.
fn shell_value_end(b: &[u8], at: usize, enclosing: Option<u8>) -> Option<usize> {
    let mut i = at;
    while i < b.len() {
        match b[i] {
            c if c.is_ascii_whitespace() => break,
            q if Some(q) == enclosing => break,
            q if quote_byte(q) => i = quoted_end(b, i, false)?,
            _ => i += 1,
        }
    }
    Some(i)
}

/// How a separator reads a value: `=` as the shell does, `:` as JSON/YAML/a header does.
#[derive(Clone, Copy, PartialEq)]
enum Sep {
    Equals,
    /// After a quoted key (`"k": v`, `'k': v`): a bare value ends at `,` `}` `]`.
    ColonQuoted,
    /// After a bare key (`k: v`, a header): a bare value is the rest of the line (or of the
    /// quoted run the key is in).
    Colon,
}

/// Where the value starting at `at` ends. `None`: it can't be told (an unclosed quote or
/// bracket).
fn value_end(b: &[u8], at: usize, sep: Sep, enclosing: Option<u8>) -> Option<usize> {
    match b[at] {
        b'(' | b'[' | b'{' => bracket_end(b, at),
        q if quote_byte(q) && sep != Sep::Equals => quoted_end(b, at, true),
        _ if sep == Sep::Equals => shell_value_end(b, at, enclosing),
        _ => {
            let stop = |c: u8| Some(c) == enclosing || (sep == Sep::ColonQuoted && matches!(c, b',' | b'}' | b']'));
            let end = b[at..].iter().position(|&c| stop(c)).map_or(b.len(), |n| at + n);
            Some(end)
        }
    }
}

/// How a secret-named value runs on past its line — into a log's next lines ([`continuations`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Carry {
    /// Nothing after the name on its line (`password:`, `"token":`), or a YAML block scalar
    /// (`password: |-`): the next non-blank line is the value, and so is each line after it
    /// indented deeper than the name's line (`indent`).
    Below { indent: usize },
    /// A quote the value opened that its line doesn't close: up to the line that does.
    Quote(u8),
    /// Brackets the value opened that its line doesn't close (`depth` still open): until they
    /// balance.
    Bracket(usize),
    /// An end that can't be told: the rest of the window.
    Rest,
}

/// A YAML block scalar's header as a whole value (`|`, `|-`, `>+`, `|2`, a comment after it).
fn block_scalar(v: &str) -> bool {
    let v = v.split(" #").next().unwrap_or("").trim();
    let mut c = v.chars();
    matches!(c.next(), Some('|' | '>')) && c.all(|c| matches!(c, '+' | '-' | '0'..='9'))
}

/// Brackets still open after `b` with `depth` open before it, quoted runs skipped (one that
/// doesn't close ends the count there).
fn depth_after(b: &[u8], mut depth: usize) -> usize {
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            q if quote_byte(q) => match quoted_end(b, i, true) {
                Some(e) => {
                    i = e;
                    continue;
                }
                None => return depth.max(1),
            },
            _ => {}
        }
        i += 1;
    }
    depth
}

/// How a value starting at `b[v]` that doesn't end on its line runs on: to its quote, to its
/// brackets' close, else to the end of the window.
fn open_at(b: &[u8], v: usize) -> Carry {
    match b[v] {
        q if quote_byte(q) => Carry::Quote(q),
        b'(' | b'[' | b'{' => Carry::Bracket(depth_after(&b[v..], 0).max(1)),
        _ => Carry::Rest,
    }
}

/// `text` with the value of every secret-named field masked whole: `NAME=value` (the value a
/// shell word: `X='a b'` is all of `'a b'`), `name: value` and `"name": value` (JSON, YAML,
/// a Python dict, a header — a quoted value to its closing quote, a bracketed one to its
/// closing bracket, a bare one to the end of the line or the field), and `name:value` glued (a
/// shell word, as after `=`). A `:` separates only after a quoted key, before a space or quote,
/// or glued after a name with no `.` and before no `/`, so `auth.py:42:` and `https://` aren't
/// fields. `Ok`: the text, and how a value runs on below it ([`Carry::Below`]). `Err`: a
/// secret-named value whose end can't be told (and how it runs on), or whose quoting can't — an
/// apostrophe earlier read as opening a quote (`Couldn't … PASSWORD='hunter two'`: review
/// ai-review codex #1) — the whole line is to go.
fn fields(text: &str) -> Result<(String, Option<Carry>), Option<Carry>> {
    let b = text.as_bytes();
    let states = quote_states(b);
    let indent = b.iter().take_while(|c| c.is_ascii_whitespace()).count();
    let mut carry = None;
    let mut out = String::with_capacity(text.len());
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        if !name_byte(b[i]) || (i > 0 && name_byte(b[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i;
        while j < b.len() && name_byte(b[j]) {
            j += 1;
        }
        // A quoted key: the quote before it closes right after it.
        let quote = start.checked_sub(1).map(|k| b[k]).filter(|&q| quote_byte(q));
        let quoted = quote.is_some_and(|q| b.get(j) == Some(&q));
        let enclosing = if quoted { states[start - 1] } else { states[start] };
        let mut k = if quoted { j + 1 } else { j };
        while k < b.len() && b[k] == b' ' {
            k += 1;
        }
        let sep = match (b.get(k), b.get(k + 1)) {
            (Some(b'='), next) if next != Some(&b'=') => Some((Sep::Equals, k + 1)),
            (Some(b':'), Some(b'=')) => Some((Sep::Equals, k + 2)),
            (Some(b':'), _) if quoted => Some((Sep::ColonQuoted, k + 1)),
            (Some(b':'), next) if next.is_none_or(|&c| c == b' ' || quote_byte(c)) => Some((Sep::Colon, k + 1)),
            (Some(b':'), Some(&c)) if k == j && c != b'/' && !c.is_ascii_whitespace() && !text[start..j].contains('.') => Some((Sep::Equals, k + 1)),
            _ => None,
        };
        let Some((sep, after)) = sep else {
            i = j;
            continue;
        };
        if !secret_name(&text[start..j]) {
            // Not a secret, but its value may hold one (`--env=HF_TOKEN=…`): read on inside it.
            i = after;
            continue;
        }
        let mut v = after;
        while v < b.len() && b[v] == b' ' {
            v += 1;
        }
        if v >= b.len() {
            // Nothing after the name: a YAML or JSON value is on the lines below.
            if sep != Sep::Equals {
                carry = Some(Carry::Below { indent });
            }
            i = v;
            continue;
        }
        if Some(b[v]) == enclosing {
            // The quote around the name closes right after it — an empty value (`echo "TOKEN="`)
            // — unless a value follows: then the quoting can't be told.
            if b.get(v + 1).is_none_or(|c| c.is_ascii_whitespace() || matches!(c, b';' | b',' | b')' | b'}' | b']')) {
                i = v;
                continue;
            }
            return Err(quoted_end(b, v, true).is_none().then_some(Carry::Quote(b[v])));
        }
        if sep != Sep::Equals && block_scalar(&text[v..]) {
            carry = Some(Carry::Below { indent });
        }
        let Some(end) = value_end(b, v, sep, enclosing) else { return Err(Some(open_at(b, v))) };
        if end > v {
            out.push_str(&text[copied..v]);
            out.push_str(MASK);
            copied = end;
        }
        i = end.max(v + 1);
    }
    out.push_str(&text[copied..]);
    Ok((out, carry))
}

/// What the previous word asks of the next one.
#[derive(Clone, Copy, PartialEq)]
enum Next {
    Keep,
    /// A secret-named flag's value, `Bearer`'s, `Authorization: Basic`'s: masked.
    Secret,
    /// `-u`/`--user`/`--proxy-user`'s: masked when it is `user:password`.
    UserPass,
}

/// `scheme://user:password@host` → `scheme://<redacted>@host`, in one word — up to its last
/// `@`, so a password holding a `/` or an `@` goes whole (a URL that merely has an `@` in its
/// path loses a little more than it must).
fn url_credentials(word: &str) -> Option<String> {
    let at = word.find("://")? + 3;
    let rest = &word[at..];
    let end = rest.rfind('@')?;
    let cred = &rest[..end];
    (!cred.is_empty() && cred.contains(':')).then(|| format!("{}{MASK}{}", &word[..at], &rest[end..]))
}

/// Basic auth glued to its flag: `--user=u:p`, `--proxy-user=u:p`, `-uu:p` → the flag and
/// [`MASK`].
fn attached_user(word: &str) -> Option<String> {
    let (flag, value) = ["--user=", "--proxy-user=", "-u"].iter().find_map(|f| word.strip_prefix(f).map(|v| (*f, v)))?;
    value.contains(':').then(|| format!("{flag}{MASK}"))
}

/// The short option a command takes its password with (review ai-review codex #2, claude #7):
/// `redis-cli -a PASS`, `sshpass -p PASS`, mysql's `-pPASS` — and `-p NAME`, there the database,
/// masked all the same. `word`: the program, as typed (a path too).
fn password_flag(word: &str) -> Option<&'static str> {
    match word.rsplit('/').next().unwrap_or(word) {
        "redis-cli" => Some("-a"),
        "sshpass" => Some("-p"),
        "mysql" | "mysqldump" | "mysqladmin" | "mysqlimport" | "mysqlshow" | "mysqlcheck" | "mariadb" | "mariadb-dump" | "mariadb-admin" => Some("-p"),
        _ => None,
    }
}

/// `text` with the word after a secret-named flag (`--api-key x`, `--password "a b"`: the
/// value a shell word, quotes and all), after `Bearer` or `Authorization: Basic`, after a
/// command's password option ([`password_flag`], glued too), and basic auth (`-u user:pass`,
/// `--user user:pass`, either glued, the credentials in a URL) masked. Whitespace is kept as it
/// was. `Err`: a value opens a quote that never closes (and how it runs on).
fn flag_values(text: &str) -> Result<String, Option<Carry>> {
    let b = text.as_bytes();
    let states = quote_states(b);
    let mut out = String::with_capacity(text.len());
    let (mut next, mut after_authorization) = (Next::Keep, false);
    let mut tool: Option<&'static str> = None;
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            let ws = b[i..].iter().position(|c| !c.is_ascii_whitespace()).map_or(b.len(), |n| i + n);
            out.push_str(&text[i..ws]);
            i = ws;
            continue;
        }
        let end = b[i..].iter().position(u8::is_ascii_whitespace).map_or(b.len(), |n| i + n);
        let word = &text[i..end];
        if next != Next::Keep && !word.starts_with('-') {
            let Some(value_end) = shell_value_end(b, i, states[i]) else { return Err(Some(open_at(b, i))) };
            let value = &text[i..value_end];
            out.push_str(if next == Next::Secret || value.contains(':') { MASK } else { value });
            next = Next::Keep;
            i = value_end;
            continue;
        }
        let bare = word.trim_matches(|c| c == '"' || c == '\'').trim_end_matches(':');
        let basic = after_authorization && bare.eq_ignore_ascii_case("basic");
        after_authorization = bare.eq_ignore_ascii_case("authorization");
        next = Next::Keep;
        let glued = tool.filter(|f| word.strip_prefix(f).is_some_and(|v| !v.is_empty()));
        if let Some(masked) = url_credentials(word).or_else(|| attached_user(word)) {
            out.push_str(&masked);
        } else if let Some(flag) = glued {
            out.push_str(flag);
            out.push_str(MASK);
        } else {
            out.push_str(word);
            let secret_flag = word.starts_with('-') && !word.contains('=') && secret_name(word);
            if secret_flag || basic || bare.eq_ignore_ascii_case("bearer") || tool == Some(word) {
                next = Next::Secret;
            } else if matches!(word, "-u" | "--user" | "--proxy-user") {
                next = Next::UserPass;
            }
        }
        // The command each word belongs to, for its password option: the last program named,
        // until a command ends.
        if let Some(flag) = password_flag(word) {
            tool = Some(flag);
        } else if word.ends_with([';', '&', '|']) {
            tool = None;
        }
        i = end;
    }
    Ok(out)
}

/// `text` with the values of secret-named things masked whole ([`fields`], then
/// [`flag_values`]) — or, when one can't be delimited, [`LINE_MASK`] in place of all of it.
pub fn named_values(text: &str) -> String {
    masked(text).0
}

/// [`named_values`]' text, and how a value in it runs on past the line ([`Carry`]).
fn masked(text: &str) -> (String, Option<Carry>) {
    match fields(text) {
        Err(carry) => (LINE_MASK.to_string(), carry),
        Ok((t, carry)) => match flag_values(&t) {
            Ok(t) => (t, carry),
            Err(c) => (LINE_MASK.to_string(), c.or(carry)),
        },
    }
}

/// One line of pod text made safe to show and send: line breaks and tabs are spaces, other
/// control characters and escape sequences gone ([`crate::jobs::sanitize`]), whitespace runs one
/// space, and no framing of the prompt's or the reply's in it ([`unframed`]).
fn one_line(s: &str) -> String {
    let spaced: String = s.chars().map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c }).collect();
    unframed(&crate::jobs::sanitize(&spaced).split_whitespace().collect::<Vec<_>>().join(" "))
}

/// `s` with nothing that reads as framing (review ai-review claude #3): a run of four or more `=`
/// cut to three — the prompt's data delimiters are `=====` lines (`super::prompt_tagged`), so pod
/// text such as `===== END POD DATA =====` can't look like one — and `@@ARENA` (the reply's
/// markers, `super::bundle::marker`) down to one `@`.
fn unframed(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0;
    for c in s.chars() {
        run = if c == '=' { run + 1 } else { 0 };
        if run <= 3 {
            out.push(c);
        }
    }
    while out.contains("@@ARENA") {
        out = out.replace("@@ARENA", "@ARENA");
    }
    out
}

/// A name from a pod (a branch, a file, a tmux session, a GPU process) for the prompt: on one
/// line, keys out ([`secrets`], before and after the escapes go — a key glued to a colour code
/// is found either way), clipped to `max` characters.
pub fn name(s: &str, max: usize) -> String {
    crate::fleet::clip(&secrets(&one_line(&secrets(s))), max)
}

/// Free text from a pod (a process's arguments, a log line, a command, a commit subject) for
/// the prompt: [`name`]'s treatment, then a private-key block in it ([`pem_inline`]), the
/// secret-named values ([`named_values`]), base64 key material ([`base64_runs`]) and any long
/// token ([`long_tokens`]) masked, then clipped to `max` characters. (The heuristics a log gets
/// across its lines — [`log_tail`] — work on one line here too: a key pasted into a command's
/// arguments is on one; review ai-review claude #7.)
pub fn text(s: &str, max: usize) -> String {
    crate::fleet::clip(&long_tokens(&base64_runs(&named_values(&pem_inline(&secrets(&one_line(&secrets(s))))))), max)
}

/// The shortest line [`base64_like`] takes for key material, and the shortest run
/// [`base64_runs`] masks.
pub const BASE64_LINE: usize = 40;

/// `s` (one line) with each private-key block in it — `-----BEGIN … PRIVATE KEY-----` through its
/// `-----END …-----`, or to the end — as [`PEM_MASK`].
fn pem_inline(s: &str) -> String {
    const BEGIN: &str = "-----BEGIN";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find(BEGIN) {
        let after = at + BEGIN.len();
        let head_end = rest[after..].find("-----").map(|e| after + e + 5);
        if !rest[at..head_end.unwrap_or(rest.len())].contains("PRIVATE KEY") {
            out.push_str(&rest[..after]);
            rest = &rest[after..];
            continue;
        }
        out.push_str(&rest[..at]);
        out.push_str(PEM_MASK);
        let body = &rest[head_end.unwrap_or(rest.len())..];
        rest = body.find("-----END").and_then(|e| body[e + 8..].find("-----").map(|x| &body[e + 8 + x + 5..])).unwrap_or("");
    }
    out.push_str(rest);
    out
}

fn base64_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')
}

/// `text` with every run of [`BASE64_LINE`] or more of `[A-Za-z0-9+/=]` holding an upper- and a
/// lower-case letter and a digit — base64 key material, which `+` and `/` keep under
/// [`LONG_TOKEN`]'s runs — replaced by [`MASK`]. A path is rarely all three (and its `.`, `-`
/// and `_` end a run); a hex hash has no upper case.
pub fn base64_runs(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        if !base64_byte(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && base64_byte(b[i]) {
            i += 1;
        }
        let run = &b[start..i];
        let mixed = run.iter().any(u8::is_ascii_uppercase) && run.iter().any(u8::is_ascii_lowercase) && run.iter().any(u8::is_ascii_digit);
        if run.len() >= BASE64_LINE && mixed {
            out.push_str(&text[copied..start]);
            out.push_str(MASK);
            copied = i;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Whether a log line looks like a line of base64 key material: [`BASE64_LINE`] or more of
/// `[A-Za-z0-9+/=]` and nothing else (surrounding whitespace aside).
fn base64_like(line: &str) -> bool {
    let t = line.trim();
    t.len() >= BASE64_LINE && t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

fn pem_begin(l: &str) -> bool {
    l.contains("-----BEGIN") && l.contains("PRIVATE KEY")
}

fn pem_end(l: &str) -> bool {
    l.contains("-----END") && l.contains("PRIVATE KEY")
}

/// `lines` with every private-key block — `-----BEGIN … PRIVATE KEY-----` to its `-----END`,
/// or to the last line — as one [`PEM_MASK`] line: its base64 lines are broken up by `+` and
/// `/`, so no single run need reach [`LONG_TOKEN`]. When `starts_cut` (the lines begin part-way
/// into the log) and an `-----END … PRIVATE KEY-----` comes before any `BEGIN`, the window
/// begins inside a key: everything up to that END is one [`PEM_MASK`] too.
fn pem_blocks(lines: &[String], starts_cut: bool) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut rest = lines;
    if starts_cut {
        let first_begin = lines.iter().position(|l| pem_begin(l)).unwrap_or(lines.len());
        if let Some(end) = lines[..first_begin].iter().position(|l| pem_end(l)) {
            out.push(PEM_MASK.to_string());
            rest = &lines[end + 1..];
        }
    }
    let mut in_pem = false;
    for l in rest {
        if in_pem {
            in_pem = !l.contains("-----END");
        } else if pem_begin(l) {
            out.push(PEM_MASK.to_string());
            in_pem = !l.contains("-----END");
        } else {
            out.push(l.clone());
        }
    }
    out
}

/// Leading lines that look like key material ([`base64_like`]) taken off `lines`: what a cut
/// that landed inside a key (or any base64 blob) leaves at its start. Conservative — a harmless
/// hash or a line of `=` there goes too. How many went.
fn drop_leading_base64(lines: &mut Vec<String>) -> usize {
    let n = lines.iter().take_while(|l| base64_like(l)).count();
    lines.drain(..n);
    n
}

/// The leading spaces and tabs of `l`.
fn indent_of(l: &str) -> usize {
    l.bytes().take_while(|c| *c == b' ' || *c == b'\t').count()
}

/// `lines` with every line a secret's value runs on into ([`Carry`], from the line above) as
/// one [`CONT_MASK`] line a run (review ai-review codex #3): a YAML block scalar
/// (`password: |-`) or a value on the lines below its name (`"password":`) — the next non-blank
/// line, and each after it indented deeper than the name; an unclosed quote — through the line
/// that closes it; unbalanced brackets — until they balance; an end that can't be told — the
/// rest of the window. Lines masked whole already ([`PEM_MASK`]) are left as they are.
fn continuations(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let (mut carry, mut first) = (None, false);
    for l in lines {
        if let Some(c) = carry {
            let hidden = match c {
                Carry::Below { .. } if l.trim().is_empty() => true,
                Carry::Below { indent } => {
                    let below = first || indent_of(&l) > indent;
                    first = false;
                    below
                }
                Carry::Quote(q) => {
                    if l.as_bytes().contains(&q) {
                        carry = None;
                    }
                    true
                }
                Carry::Bracket(depth) => {
                    let d = depth_after(l.as_bytes(), depth);
                    carry = (d > 0).then_some(Carry::Bracket(d));
                    true
                }
                Carry::Rest => true,
            };
            if hidden {
                if out.last().map(String::as_str) != Some(CONT_MASK) {
                    out.push(CONT_MASK.to_string());
                }
                continue;
            }
        }
        carry = if l == PEM_MASK { None } else { masked(&crate::jobs::sanitize(&l)).1 };
        first = true;
        out.push(l);
    }
    out
}

/// A log's last `keep` lines for the prompt, from `lines` — the whole window read (the remote
/// read's last bytes; `starts_cut`: it begins part-way into the log, its partial first line
/// already gone). Private-key blocks are masked over the whole window **first** ([`pem_blocks`]),
/// so neither cut can keep a key's body without the header that marks it, and so are a secret's
/// values that run on over lines ([`continuations`]); then at each cut — the window's start when
/// `starts_cut`, and the `keep`-line cut — leading base64-looking lines are dropped
/// ([`drop_leading_base64`]): a key printed without its armour, or one whose END is past the
/// window. Each line left goes through [`text`]. The lines, and how many older lines of the
/// window aren't shown.
pub fn log_tail(lines: &[String], starts_cut: bool, keep: usize, max: usize) -> (Vec<String>, usize) {
    let mut v = continuations(pem_blocks(lines, starts_cut));
    let mut hidden = if starts_cut { drop_leading_base64(&mut v) } else { 0 };
    let over = v.len().saturating_sub(keep);
    if over > 0 {
        v.drain(..over);
        hidden += over + drop_leading_base64(&mut v);
    }
    let shown = v.into_iter().map(|l| if l == PEM_MASK || l == CONT_MASK { l } else { text(&l, max) }).collect();
    (shown, hidden)
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

    /// The whole value of what is named secret — quoted values with their spaces, JSON, YAML
    /// and Python-dict fields, headers, basic auth — and nothing of the look-alikes around it
    /// (`auth.py:42:`, `python -u`, `token ==`, a URL without credentials).
    #[test]
    fn named_values_masks_what_is_named_secret() {
        let cases = [
            ("HF_TOKEN=abc123 python x.py", "HF_TOKEN=<redacted> python x.py"),
            ("prog --api-key short1 --lr 1", "prog --api-key <redacted> --lr 1"),
            ("prog --password=hunter2", "prog --password=<redacted>"),
            ("curl -H 'Authorization: Bearer abc' u", "curl -H 'Authorization: <redacted>' u"),
            ("-H 'Authorization: Basic dXNlcjpw'", "-H 'Authorization: <redacted>'"),
            ("Basic usage: run it", "Basic usage: run it"),
            ("git clone https://user:pw@github.com/x/y", "git clone https://<redacted>@github.com/x/y"),
            ("MY_SECRET='x y'", "MY_SECRET=<redacted>"),
            ("MY_SECRET='x y' next", "MY_SECRET=<redacted> next"),
            ("export DB_PASSWORD=\"hunter two\" && run", "export DB_PASSWORD=<redacted> && run"),
            ("TOKEN=a'b c'd tail", "TOKEN=<redacted> tail"),
            ("echo \"loading HF_TOKEN=abc\"; echo \"next\"", "echo \"loading HF_TOKEN=<redacted>\"; echo \"next\""),
            ("--password \"a b\" --lr 1", "--password <redacted> --lr 1"),
            ("--token='a b' x", "--token=<redacted> x"),
            ("--env=HF_TOKEN=abc x", "--env=HF_TOKEN=<redacted> x"),
            // JSON, a Python dict, YAML, source code.
            ("{\"password\": \"cohort db pass\", \"user\": \"bob\"}", "{\"password\": <redacted>, \"user\": \"bob\"}"),
            ("{\"password\":\"cohort-db-pass\",\"port\":5432}", "{\"password\":<redacted>,\"port\":5432}"),
            ("{\"token\": \"a\\\"b c\", \"n\": 1}", "{\"token\": <redacted>, \"n\": 1}"),
            ("{'api_key': 'x y z', 'lr': 0.1}", "{'api_key': <redacted>, 'lr': 0.1}"),
            ("{\"apiKey\": 12345, \"n\": 1}", "{\"apiKey\": <redacted>, \"n\": 1}"),
            ("\"Token\" : \"x y\"", "\"Token\" : <redacted>"),
            ("api_key: abc def", "api_key: <redacted>"),
            ("password: \"a b\" # set", "password: <redacted> # set"),
            ("client_secret = \"a b\"", "client_secret = <redacted>"),
            ("token := os.Getenv(x)", "token := <redacted>"),
            ("auth=(\"user\", \"pass word\") rest", "auth=<redacted> rest"),
            ("\"credentials\": {\"u\": \"a\", \"p\": \"b c\"} rest", "\"credentials\": <redacted> rest"),
            ("secrets: [\"a\", \"b c\"]", "secrets: <redacted>"),
            // Basic auth.
            ("curl -u admin:pw https://x", "curl -u <redacted> https://x"),
            ("curl --user admin:pw https://x", "curl --user <redacted> https://x"),
            ("curl -u \"admin:p w\" https://x", "curl -u <redacted> https://x"),
            ("curl --user=admin:pw x", "curl --user=<redacted> x"),
            ("curl --proxy-user px:pw x", "curl --proxy-user <redacted> x"),
            ("curl -uadmin:pw x", "curl -u<redacted> x"),
            // Look-alikes, kept.
            ("python -u train.py", "python -u train.py"),
            ("pip install --user torch", "pip install --user torch"),
            ("auth.py:42: DeprecationWarning: use x", "auth.py:42: DeprecationWarning: use x"),
            ("site-packages/transformers/tokenization_utils_base.py:1601: FutureWarning: x", "site-packages/transformers/tokenization_utils_base.py:1601: FutureWarning: x"),
            ("if token == \"abc\":", "if token == \"abc\":"),
            ("python train.py --lr 3e-4 --epochs=2", "python train.py --lr 3e-4 --epochs=2"),
            ("ssh git@github.com", "ssh git@github.com"),
            ("see https://github.com/x/y", "see https://github.com/x/y"),
            ("don't stop TOKEN=abc now", "don't stop TOKEN=<redacted> now"),
            ("--token", "--token"),
            ("TOKEN=", "TOKEN="),
            ("password:", "password:"),
            ("echo \"TOKEN=\"", "echo \"TOKEN=\""),
        ];
        for (input, want) in cases {
            assert_eq!(named_values(input), want, "{input}");
        }
    }

    /// Review ai-review codex #1-#2, claude #7, the exact examples and their kin: ambiguous
    /// quoting takes the line; a command's password option is masked (glued too) — only after
    /// that command, until a `;`, `&` or `|` ends it; `pass`, `passphrase` and `pwd` are secret names; `name:value` glued is a field
    /// unless the name has a `.` or a `/` follows (a file's line, a URL).
    #[test]
    fn short_password_options_glued_values_and_ambiguous_quotes() {
        let cases = [
            ("Couldn't connect with PASSWORD='hunter two'", LINE_MASK),
            ("Couldn't connect: password: 'hunter two'", LINE_MASK),
            ("it's set: echo 'TOKEN='abc", LINE_MASK),
            ("Couldn't connect with PASSWORD=\"hunter two\" (retrying)", "Couldn't connect with PASSWORD=<redacted> (retrying)"),
            ("redis-cli -a cohort-db-pass ping", "redis-cli -a <redacted> ping"),
            ("/usr/bin/redis-cli -h db -a 'two words' ping", "/usr/bin/redis-cli -h db -a <redacted> ping"),
            ("sshpass -p cohort-login-pass ssh root@10.0.0.1", "sshpass -p <redacted> ssh root@10.0.0.1"),
            ("sshpass -pPASS ssh x", "sshpass -p<redacted> ssh x"),
            ("mysql -h db -u root -pPASS app", "mysql -h db -u root -p<redacted> app"),
            ("mysql -u root -p PASS", "mysql -u root -p <redacted>"),
            ("cd /x && mysqldump -psecret db > d.sql", "cd /x && mysqldump -p<redacted> db > d.sql"),
            ("gpg --batch --passphrase X --decrypt f", "gpg --batch --passphrase <redacted> --decrypt f"),
            ("tool --pass hunter2 --v", "tool --pass <redacted> --v"),
            ("DB_PASS=hunter2 MYSQL_PWD=x2 run", "DB_PASS=<redacted> MYSQL_PWD=<redacted> run"),
            ("password:hunter2", "password:<redacted>"),
            ("INFO token:abc def", "INFO token:<redacted> def"),
            // Look-alikes, kept: the option without its command, another command's, words.
            ("ls -a -p dir", "ls -a -p dir"),
            ("ps -p 123; redis-cli ping", "ps -p 123; redis-cli ping"),
            ("mysql -e 'show tables'; ps -p 4", "mysql -e 'show tables'; ps -p 4"),
            ("bypass=1 --num-passes 3 compass: north", "bypass=1 --num-passes 3 compass: north"),
            ("auth.py:42: warning", "auth.py:42: warning"),
            ("see https://github.com/x and C:\\data", "see https://github.com/x and C:\\data"),
            ("at 12:30:45 INFO:root:started", "at 12:30:45 INFO:root:started"),
        ];
        for (input, want) in cases {
            assert_eq!(named_values(input), want, "{input}");
        }
        // Through `text`, a short password is gone from each.
        for line in ["Couldn't connect with PASSWORD='hunter two'", "redis-cli -a hunter2 ping", "sshpass -p hunter2 ssh h", "mysql -phunter2", "password:hunter2"] {
            assert!(!text(line, 200).contains("hunter"), "{line}");
        }
    }

    /// Review ai-review codex #3: a value that runs on over a log's lines — a YAML block scalar
    /// after a secret's name, a YAML or JSON value on the line after it, an unclosed quote or
    /// bracket — is masked on every line it takes, as one [`CONT_MASK`] line; the lines after it
    /// are kept. One that never closes takes the rest of the window.
    #[test]
    fn a_secret_value_over_several_lines_is_masked_on_each() {
        let tail = |lines: &[&str]| log_tail(&strings(lines), false, 40, 200).0;
        assert_eq!(tail(&["password: |-", "  cohort-db-pass", "next"]), strings(&["password: <redacted>", CONT_MASK, "next"]));
        assert_eq!(
            tail(&["db:", "  password: >", "    cohort db", "", "    pass", "  user: bob", "done"]),
            strings(&["db:", "password: <redacted>", CONT_MASK, "user: bob", "done"])
        );
        assert_eq!(tail(&["password:", "  hunter2", "user: x"]), strings(&["password:", CONT_MASK, "user: x"]));
        assert_eq!(
            tail(&["{", "  \"password\":", "  \"hunter2\",", "  \"user\": \"bob\"", "}"]),
            strings(&["{", "\"password\":", CONT_MASK, "\"user\": \"bob\"", "}"])
        );
        assert_eq!(tail(&["MY_SECRET=\"first part", "second part\"", "after"]), strings(&[LINE_MASK, CONT_MASK, "after"]));
        assert_eq!(tail(&["--password 'a", "b", "c' --lr 1", "after"]), strings(&[LINE_MASK, CONT_MASK, "after"]));
        assert_eq!(
            tail(&["\"credentials\": {", "  \"u\": \"a\",", "  \"p\": {\"x\": \"b c\"}", "}", "done"]),
            strings(&[LINE_MASK, CONT_MASK, "done"])
        );
        assert_eq!(tail(&["TOKEN=\"never closed", "x", "y"]), strings(&[LINE_MASK, CONT_MASK]));
        // Not a secret's: nothing hidden.
        let plain = ["name: |", "some text", "message: \"open", "more\"", "done"];
        assert_eq!(tail(&plain), strings(&plain));
        // Hidden from every shape: no fragment of a value on a line below.
        let all = tail(&["api_key: |", "  Qx7alpha", "\"token\":", "\"Qx7bravo\"", "PASSWORD='x", "Qx7charlie'", "end"]);
        assert!(!all.join("\n").contains("Qx7"), "{all:?}");
    }

    /// Review ai-review claude #7: a private key or base64 key material pasted into a process's
    /// arguments (one line, where the log's line heuristics don't reach) is masked; a path, a
    /// hex hash and short words are not.
    #[test]
    fn a_key_in_one_lines_arguments_is_masked() {
        let body = key_body(3, 4);
        let pem = format!("ssh-add - <<< '-----BEGIN OPENSSH PRIVATE KEY----- {} -----END OPENSSH PRIVATE KEY-----' && run", body.join(" "));
        let out = text(&pem, 2000);
        assert!(out.starts_with("ssh-add - <<< '<redacted: private key block>' && run"), "{out}");
        no_body_in(&[out], &body);
        let cut = text(&format!("echo -----BEGIN RSA PRIVATE KEY----- {}", body[0]), 2000);
        assert_eq!(cut, "echo <redacted: private key block>");
        assert_eq!(text("-----BEGIN CERTIFICATE----- x", 200), "-----BEGIN CERTIFICATE----- x", "not a private key");
        let args = format!("python sign.py --key-material {} --out x", body[1]);
        let out = text(&args, 2000);
        assert_eq!(out, format!("python sign.py --key-material {MASK} --out x"));
        for kept in ["/usr/lib/python3/dist/site/packages/torch/lib/libtorch/x", "0123456789abcdef0123456789abcdef01234567", "a b c"] {
            assert_eq!(base64_runs(kept), kept);
        }
    }

    /// A secret-named value whose end can't be told — a quote or bracket that doesn't close on
    /// the line — takes the whole line: nothing of it is kept.
    #[test]
    fn a_line_that_cant_be_masked_is_a_placeholder() {
        for line in [
            "TOKEN=\"never closed and more",
            "--password \"a b",
            "{\"password\": \"a b",
            "\"credentials\": {\"u\": \"a\",",
            "auth=(\"user\", \"pass",
            "curl -u 'admin:p w",
        ] {
            assert_eq!(named_values(line), LINE_MASK, "{line}");
            assert_eq!(text(&format!("INFO {line}"), 200), LINE_MASK, "{line}");
        }
        // An open quote that isn't a secret's value is no reason.
        assert_eq!(named_values("it's \"fine"), "it's \"fine");
    }

    /// Fuzz-ish: every secret name × every shape it's written in (shell, flags, JSON, a Python
    /// dict, YAML, source, headers, basic auth, URLs) × noise around it — no fragment of the
    /// value survives `text`, and something says it was masked.
    #[test]
    fn no_fragment_of_a_named_value_survives_any_shape() {
        let keys = ["password", "API_KEY", "token", "client_secret", "db.passwd", "apiKey", "HF_TOKEN", "Private-Key", "Authorization"];
        // {k} the name, {v} the value: values with spaces, then values that can't have them.
        let spaced = [
            "{k}='{v}'",
            "{k}=\"{v}\"",
            "export {k}=\"{v}\"",
            "--{k} \"{v}\"",
            "--{k} '{v}' --lr 1",
            "--{k}='{v}'",
            "{\"{k}\": \"{v}\", \"user\": \"bob\"}",
            "{\"{k}\":\"{v}\"}",
            "{'{k}': '{v}', 'lr': 0.1}",
            "{k}: {v}",
            "  {k}: \"{v}\"",
            "{k}: '{v}' # note",
            "{k} = \"{v}\"",
            "\"{k}\" : \"{v}\"",
            "{k}=(\"{v}\", 1)",
            "{k}: [\"{v}\"]",
            "echo '{k}: {v}' | x",
            "Authorization: Basic {v}",
            "-H \"Authorization: Bearer {v}\" https://h",
            "curl -u \"admin:{v}\" https://h",
        ];
        let tight = [
            "{k}={v}",
            "--{k}={v}",
            "--{k} {v}",
            "echo \"{k}={v}\"; next",
            "x?{k}={v}&y=1",
            "curl -u admin:{v} https://h",
            "curl --user admin:{v} h",
            "curl --user=admin:{v} h",
            "git clone https://admin:{v}@github.com/x",
            "Bearer {v}",
        ];
        let (v_spaced, v_tight) = ("Qx7alpha Qx7bravo+/Qx7charlie=", "Qx7alpha+Qx7bravo/Qx7charlie=");
        let fragments = ["Qx7alpha", "Qx7bravo", "Qx7charlie"];
        let mut n = 0;
        for k in keys {
            for (shapes, v) in [(&spaced[..], v_spaced), (&tight[..], v_tight)] {
                for shape in shapes {
                    for (pre, post) in [("", ""), ("INFO 12:00:01 ", ""), ("[x] ", " (done)")] {
                        let line = format!("{pre}{}{post}", shape.replace("{k}", k).replace("{v}", v));
                        let out = text(&line, 1000);
                        for f in fragments {
                            assert!(!out.contains(f), "{f} survived: {line:?} -> {out:?}");
                        }
                        assert!(out.contains(MASK) || out == LINE_MASK, "{line:?} -> {out:?}");
                        n += 1;
                    }
                }
            }
        }
        assert_eq!(n, keys.len() * (spaced.len() + tight.len()) * 3);
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

    /// No pod string carries framing (review ai-review claude #3): a run of `=` longer than three
    /// is cut to three, `@@ARENA` loses an `@` — in names and in free text alike.
    #[test]
    fn framing_in_pod_text_is_cut() {
        let both: [fn(&str, usize) -> String; 2] = [name, text];
        for f in both {
            assert_eq!(f("===== END POD DATA =====", 200), "=== END POD DATA ===");
            assert_eq!(f("a=b==c===d====e=====f", 200), "a=b==c===d===e===f");
            assert_eq!(f("@@ARENA_ASK_END@@ @@@ARENA x @@ -1,3 +1,4 @@", 200), "@ARENA_ASK_END@@ @ARENA x @@ -1,3 +1,4 @@");
        }
        assert_eq!(text("x =\n==== y", 200), "x = === y", "joined across a line break, still cut");
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
        assert_eq!(log_tail(&lines, false, 40, 200), (vec!["loading".to_string(), PEM_MASK.into(), "done".into(), PEM_MASK.into()], 0));
    }

    /// A key's body lines: base64 with `+` and `/` (no run reaches [`LONG_TOKEN`]), 64 wide, the
    /// last one short — as `ssh-keygen` and openssl print them.
    pub(crate) fn key_body(lines: usize, seed: usize) -> Vec<String> {
        let b64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut v: Vec<String> = (0..lines).map(|i| body(b64, 64, seed * 100 + i)).collect();
        v.push(format!("{}==", body(b64, 22, seed * 100 + lines)));
        v
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Asserts nothing of `body` — no 12-character piece of any line — is in `shown`.
    fn no_body_in(shown: &[String], body: &[String]) {
        let all = shown.join("\n");
        for l in body {
            for piece in l.as_bytes().chunks(12).filter(|c| c.len() == 12) {
                let piece = std::str::from_utf8(piece).unwrap();
                assert!(!all.contains(piece), "{piece} of a key line is in:\n{all}");
            }
        }
    }

    /// The window starts inside a key — the remote read's last bytes, or the 40-line cut: its
    /// END is there without its BEGIN (everything up to the END is one mask), or not at all
    /// (the leading base64 lines go), or a complete block sits just past where the line cut
    /// falls (masked before the cut, so the cut can't split it).
    #[test]
    fn a_tail_that_starts_inside_a_key_shows_none_of_it() {
        let key = key_body(12, 1);
        let end = "-----END OPENSSH PRIVATE KEY-----".to_string();
        // The read's start: inside the body, the END in the window.
        let mut lines = key[3..].to_vec();
        lines.extend([end.clone(), "epoch 1 loss 0.5".into(), "epoch 2 loss 0.4".into()]);
        let (shown, hidden) = log_tail(&lines, true, 40, 200);
        assert_eq!((shown.clone(), hidden), (strings(&[PEM_MASK, "epoch 1 loss 0.5", "epoch 2 loss 0.4"]), 0));
        // Not cut: an END with no BEGIN is just a line (nothing before it is a key's).
        let (shown, _) = log_tail(&strings(&["a", "-----END OPENSSH PRIVATE KEY-----", "b"]), false, 40, 200);
        assert_eq!(shown, strings(&["a", "-----END OPENSSH PRIVATE KEY-----", "b"]));
        // The read's start: inside a key still being printed, or one without armour.
        let mut lines = key[..10].to_vec();
        lines.push("step 3 done".into());
        let (shown, hidden) = log_tail(&lines, true, 40, 200);
        assert_eq!((shown.clone(), hidden), (strings(&["step 3 done"]), 10));
        no_body_in(&shown, &key);
        // The 40-line cut: a whole block, then 20 lines — the old order (cut, then mask) kept
        // 19 body lines and the END.
        let mut lines = vec!["start".to_string(), "-----BEGIN OPENSSH PRIVATE KEY-----".into()];
        let big = key_body(30, 2);
        lines.extend(big.iter().cloned());
        lines.push(end.clone());
        lines.extend((0..20).map(|i| format!("progress {i}")));
        let (shown, hidden) = log_tail(&lines, false, 40, 200);
        no_body_in(&shown, &big);
        assert_eq!(shown.len(), 22, "{shown:?}");
        assert_eq!((shown[0].as_str(), shown[1].as_str(), hidden), ("start", PEM_MASK, 0));
        // The 40-line cut inside a base64 blob with no armour: its leading lines go too.
        let blob = key_body(30, 3);
        let mut lines: Vec<String> = (0..100).map(|i| format!("warmup {i}")).collect();
        lines.extend(blob[..30].iter().cloned());
        lines.extend((0..20).map(|i| format!("progress {i}")));
        let (shown, hidden) = log_tail(&lines, false, 40, 200);
        no_body_in(&shown, &blob);
        assert_eq!((shown.len(), shown[0].as_str(), hidden), (20, "progress 0", 130));
        // Only whole lines of base64 alphabet, 40 or more, are taken for key material.
        assert!(base64_like(&format!("  {}  ", key[0])) && !base64_like(&key[12]) && !base64_like("step 1 loss 0.25 lr 0.001 0.002 0.003 0.004"));
    }
}
