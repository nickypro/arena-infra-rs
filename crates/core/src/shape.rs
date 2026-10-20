//! A command line as `pods ask` sends it, and as a job's id is named from it: what runs — never a
//! value it was given.
//!
//! Masking a command's free-form shell words one value at a time kept leaking new forms, each
//! review another (`bash -lc 'sshpass -p X …'`, `mysql -p'two words'`, `sshpass -p -hunter2`, an
//! old job id in a wrapper's path). So a process's arguments and a job's command aren't masked:
//! their values are dropped. [`command`] reads the words as the shell would (`words`: quotes
//! and backslashes taken out, a quoted value with blanks in it one word) and keeps only
//! - the program's name (`python3`, `jupyter-lab`, `sshpass`: a path's last part), after every
//!   leading `NAME=value` (one `…` for them all),
//! - the first script or notebook it runs (`train.py`, `run.sh`, `part1.ipynb`, `server.js`: the
//!   last part of its path) or, for python, `-m <module>`,
//! - each flag's name (`--lr`, `-p`, `--token=` for one glued to its value),
//! - `<job>` for a path under arena's jobs dir (a job's wrapper, `sh ~/.arena/jobs/<id>/run`: an
//!   id made before ids were named from this carries the command as typed),
//!
//! and writes every other word as `…`: a flag's value one `…`, any other run of words one more. A
//! shell's `-c` script is `bash -c …`, whatever it holds, and python's `-c` code `python -c …`. A
//! program's, a script's or a module's name is shown only when it is 1 to 40 of `[A-Za-z0-9._+-]`
//! (and nothing like a key), else it is `…` too; a command whose quoting doesn't close is
//! `<command>` whole — where its words begin and end, and so which is the program, can't be told
//! (review ai-review codex #1: `APP_CONFIG='{"password": "cohort-db-pass" }' python train.py`, cut
//! at blanks, showed `cohort-db-pass` as the program and put it in the job's id). So is one with a
//! construct whose words this reading doesn't follow — `$(`, a backtick, `${`, `<(`, `>(`, `<<` —
//! anywhere but in a `-c` script, which is `…` whatever it holds: a quote inside `"$(… "x" …)"` is
//! the substitution's, not the end of the outer one, and read as the end it made a value the
//! program (review ai-review codex #1, round two: `APP_CONFIG="$(printf '%s' "password:↵
//! cohort-db-pass↵host: db")" python train.py` showed `cohort-db-pass`).
//!
//! What can still show is a word spelled exactly like a flag name (`-x`, `--fast`), a script's or
//! a program's name: a value written so is the one shape that can't be told without knowing every
//! program's options. A single dash with more after it (`-pPASS`, `-lc`, `-hunter2`) is a value:
//! a short option and the value glued to it can't be told apart.
//!
//! Pure and small on purpose: job creation names its ids with it ([`crate::jobs::JobId`]).

/// What a value becomes.
pub const ELIDED: &str = "…";

/// What a path under arena's jobs dir becomes.
pub const JOB: &str = "<job>";

/// What a command whose words can't be told becomes (`words`: a quote left open, a backslash
/// at its very end; a construct this reading doesn't follow, [`FOREIGN`]).
pub const UNREAD: &str = "<command>";

/// What starts a construct whose words `words` doesn't follow — a command substitution (`$(`,
/// a backtick), a parameter expansion with braces (`${`), a process substitution (`<(`, `>(`), a
/// here-document (`<<`) — quoted or not: a command holding one outside a `-c` script is
/// [`UNREAD`].
const FOREIGN: [&str; 6] = ["$(", "`", "${", "<(", ">(", "<<"];

/// A script or notebook a program runs, by its extension.
const SCRIPTS: [&str; 4] = [".py", ".sh", ".ipynb", ".js"];

/// Shells whose `-c` takes a script — shown as `-c …` and nothing more (and so is python's).
const SHELLS: [&str; 8] = ["sh", "bash", "dash", "zsh", "ksh", "ash", "mksh", "yash"];

/// The longest program, script or module name shown; a longer one is a value.
const NAME_MAX: usize = 40;

/// The longest flag name shown; a longer one is a value.
const FLAG_MAX: usize = 64;

/// One word of a command, as the shell reads it.
#[derive(Default)]
struct Word {
    /// Its text with the quoting taken out (`'a b'` → `a b`, `\"` → `"`).
    text: String,
    /// Some of it was quoted or escaped so far (a `NAME=` must be bare to assign).
    quoted: bool,
    /// It begins with a bare `NAME=`: an environment assignment, when it comes before the program.
    assigns: bool,
    /// Where it begins and ends in the command (char offsets, quotes included).
    span: (usize, usize),
}

/// `cmd`'s words as POSIX `sh` splits them: blanks between words; `'…'` taken as it is; `"…"`, in
/// which a backslash escapes `$`, `` ` ``, `"`, `\` or a line break (and is kept before anything
/// else); a backslash outside quotes escapes the next character, and with a line break after it
/// is nothing at all. `None` when the quoting doesn't close — a quote left open, a backslash
/// ending it all: where a word ends can't be told, nor which word is the program. Operators
/// (`;`, `&&`, `|`, `>`), expansions and comments are read as a word's characters: a shape needs
/// no more than where each word begins and ends — and where that can't be told past a
/// [`FOREIGN`] construct, [`command`] reads no further than it.
fn words(cmd: &str) -> Option<Vec<Word>> {
    let mut out = Vec::new();
    let mut word: Option<Word> = None;
    let mut it = Counted { chars: cmd.chars().peekable(), read: 0 };
    while let Some(c) = it.next() {
        if c == '\\' && it.chars.peek() == Some(&'\n') {
            it.next();
            continue;
        }
        if c.is_whitespace() {
            out.extend(word.take());
            continue;
        }
        let at = it.read - 1;
        let w = word.get_or_insert_with(|| Word { span: (at, at), ..Word::default() });
        match c {
            '\'' => {
                w.quoted = true;
                loop {
                    match it.next()? {
                        '\'' => break,
                        c => w.text.push(c),
                    }
                }
            }
            '"' => {
                w.quoted = true;
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => match it.next()? {
                            '\n' => {}
                            c @ ('$' | '`' | '"' | '\\') => w.text.push(c),
                            c => {
                                w.text.push('\\');
                                w.text.push(c);
                            }
                        },
                        c => w.text.push(c),
                    }
                }
            }
            '\\' => {
                w.quoted = true;
                w.text.push(it.next()?);
            }
            '=' if !w.quoted && !w.assigns && shell_name(&w.text) => {
                w.assigns = true;
                w.text.push(c);
            }
            c => w.text.push(c),
        }
        w.span.1 = it.read;
    }
    out.extend(word);
    Some(out)
}

/// A command's chars, counting those read: where each word begins and ends.
struct Counted<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    read: usize,
}

impl Iterator for Counted<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        self.read += 1;
        Some(c)
    }
}

/// Where each [`FOREIGN`] construct in `cmd` begins (char offsets).
fn foreign_at(cmd: &str) -> Vec<usize> {
    let chars: Vec<char> = cmd.chars().collect();
    (0..chars.len())
        .filter(|&i| FOREIGN.iter().any(|f| f.chars().enumerate().all(|(k, c)| chars.get(i + k) == Some(&c))))
        .collect()
}

/// A shell variable's name: `[A-Za-z_][A-Za-z0-9_]*`.
fn shell_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A path's last part.
fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `name` is 1 to `max` bytes of `[A-Za-z0-9._+-]` and holds nothing like a key ([`keyish`]): a
/// program's, a script's or a module's name ([`NAME_MAX`]) or a flag's ([`FLAG_MAX`]).
fn plain(name: &str, max: usize) -> bool {
    (1..=max).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b)) && !keyish(name)
}

/// Whether a name holds what looks like a key rather than words — a key pasted as the command
/// (`pods run --background '<key> --go'`), a token as a script's or a flag's name: a run of 20 or
/// more letters and digits with a digit or both cases in it, or any run of 32 or more. Real names
/// are words joined by separators (`part4_interp_on_algorithmic_model.ipynb`, `--per_device_train_batch_size`,
/// `--IdentityProvider.token`), each run short.
fn keyish(name: &str) -> bool {
    name.split(|c: char| !c.is_ascii_alphanumeric()).any(|run| {
        let mixed = run.bytes().any(|b| b.is_ascii_uppercase()) && run.bytes().any(|b| b.is_ascii_lowercase());
        run.len() >= 32 || (run.len() >= 20 && (mixed || run.bytes().any(|b| b.is_ascii_digit())))
    })
}

/// A path under arena's jobs dir (`~/.arena/jobs/<id>/run`, `$HOME/.arena/jobs`).
fn job_path(word: &str) -> bool {
    word.contains(".arena/jobs")
}

/// The program's name as shown: the last part of its path, when it is [`plain`] — and so is a
/// process title's `name:` (`sshd: root@pts/0`, `postgres: …`); a kernel thread's `[name]` whole
/// (`[kswapd0]`, `[kworker/0:1-events]`); `<job>` under the jobs dir; anything else (quotes,
/// braces, an expansion, an escape sequence, a name over [`NAME_MAX`]) a value.
fn program(word: &str) -> String {
    if job_path(word) {
        return JOB.to_string();
    }
    if let Some(inner) = word.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        if plain(&inner.replace(['/', ':'], "-"), NAME_MAX) {
            return word.to_string();
        }
    }
    let b = base(word);
    if plain(b.strip_suffix(':').unwrap_or(b), NAME_MAX) {
        b.to_string()
    } else {
        ELIDED.to_string()
    }
}

/// A script or notebook it runs: its path's last part, when that is [`plain`].
fn script(word: &str) -> Option<&str> {
    let b = base(word);
    (plain(b, NAME_MAX) && SCRIPTS.iter().any(|e| b.len() > e.len() && b.ends_with(e))).then_some(b)
}

/// A python module (`-m pytest`, `-m torch.distributed.run`), when its name is [`plain`].
fn module(word: &str) -> Option<&str> {
    (plain(word, NAME_MAX) && word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.'))
        .then_some(word)
}

/// A flag's name as shown, and whether its value is glued to it (`--lr=0.1`: shown `--lr=…`, and
/// the next word isn't its value): `--` alone, `--name`, `--name=…`, a dash and one letter (`-p`).
/// `None`: not a flag — a dash with more than one byte after it included (`-pPASS`, `-lc`,
/// `-5`, `-hunter2`): a value may be glued to it, or be it.
fn flag(w: &str) -> Option<(String, bool)> {
    if w == "--" {
        return Some((w.to_string(), false));
    }
    if let Some(long) = w.strip_prefix("--") {
        let (name, glued) = long.split_once('=').map_or((long, false), |(n, _)| (n, true));
        let ok = plain(name, FLAG_MAX) && name.starts_with(|c: char| c.is_ascii_alphabetic()) && !name.contains('+');
        return ok.then(|| if glued { (format!("--{name}={ELIDED}"), true) } else { (format!("--{name}"), false) });
    }
    let b = w.as_bytes();
    (b.len() == 2 && b[0] == b'-' && b[1].is_ascii_alphabetic()).then(|| (w.to_string(), false))
}

/// Whether a shell or python (`program`) runs code given on its command line: a `-c` among the
/// flags before its first other word, alone or in a cluster (`-lc`, `-ec`, `-uc`).
fn runs_code(program: &str, rest: &[&str]) -> bool {
    (SHELLS.contains(&program.trim_start_matches('-')) || python(program))
        && rest.iter().take_while(|w| w.starts_with('-')).any(|w| {
            let short = &w[1..];
            !short.starts_with('-') && short.contains('c') && short.bytes().all(|b| b.is_ascii_alphabetic())
        })
}

/// Whether `program` is a python (`python3.11`, `pypy3`): `-m <module>` is what it runs.
fn python(program: &str) -> bool {
    program.starts_with("python") || program.starts_with("pypy")
}

/// What the last word written was, for where a value's `…` goes.
#[derive(Clone, Copy, PartialEq)]
enum Last {
    /// A flag that may take the next word as its value.
    Flag,
    /// A flag's value.
    Value,
    /// A run of other words (one `…` for all of it).
    Run,
    /// Anything shown.
    Shown,
}

/// `cmd` (a process's arguments, a job's command) as the program, the script it runs, its flag
/// names and `…` for every value (see the module doc):
/// - `python3 /w/train.py --lr 0.1 --token X` → `python3 train.py --lr … --token …`;
/// - `sshpass -p hunter2 ssh host` → `sshpass -p … …`;
/// - `bash -lc 'sshpass -p X ssh host'` → `bash -c …` (and `python -c '…'` → `python -c …`);
/// - `sh /root/.arena/jobs/<id>/run` → `sh <job>`;
/// - `APP_CONFIG='{"password": "x y" }' python train.py` → `… python train.py`;
/// - `python -c 'print(1)` (a quote left open) → `<command>`;
/// - `X="$(cat k)" python x.py`, `python x.py --k ${K}`, `cat <<EOF` → `<command>`, but
///   `bash -c 'echo $(date)'` → `bash -c …`.
pub fn command(cmd: &str) -> String {
    let Some(words) = words(cmd) else { return UNREAD.to_string() };
    // The first word a FOREIGN construct is in: the words from it on can't be told.
    let foreign = foreign_at(cmd);
    let odd = words.iter().position(|w| foreign.iter().any(|&i| (w.span.0..w.span.1).contains(&i)));
    let mut out: Vec<String> = Vec::new();
    // Every leading `NAME=value` — each a whole word, its value's quoted blanks in it: the
    // shell's assignments, before the program.
    let at = words.iter().position(|w| !w.assigns).unwrap_or(words.len());
    if at > 0 {
        out.push(ELIDED.to_string());
    }
    let texts: Vec<&str> = words[at..].iter().map(|w| w.text.as_str()).collect();
    let Some(&first) = texts.first() else {
        return if odd.is_some() { UNREAD.to_string() } else { out.join(" ") };
    };
    let prog = program(first);
    let rest = &texts[1..];
    // A `-c` script is `…` whatever it holds: a construct in it, or after it, is fine — every word
    // before it (assignments, the program, its flags) read whole.
    let code_at = at + 1 + rest.iter().take_while(|w| w.starts_with('-')).count();
    if runs_code(&prog, rest) && odd.is_none_or(|odd| odd >= code_at) {
        return [out, vec![prog, "-c".to_string(), ELIDED.to_string()]].concat().join(" ");
    }
    if odd.is_some() {
        return UNREAD.to_string();
    }
    let py = python(&prog);
    out.push(prog);
    let (mut last, mut ran, mut i) = (Last::Shown, false, 0);
    while i < rest.len() {
        let w = rest[i];
        let shown = if job_path(w) {
            Some((JOB.to_string(), Last::Shown))
        } else if let Some(m) = rest.get(i + 1).filter(|_| py && !ran && w == "-m").and_then(|m| module(m)) {
            ran = true;
            i += 1;
            Some((format!("-m {m}"), Last::Shown))
        } else if let Some(s) = script(w).filter(|_| !ran) {
            ran = true;
            Some((s.to_string(), Last::Shown))
        } else {
            flag(w).map(|(f, glued)| (f, if glued { Last::Shown } else { Last::Flag }))
        };
        match shown {
            Some((text, then)) => {
                out.push(text);
                last = then;
            }
            // A value: one `…` for a flag's, one for each run of others.
            None => {
                let next = if last == Last::Flag { Last::Value } else { Last::Run };
                if last != Last::Run {
                    out.push(ELIDED.to_string());
                }
                last = next;
            }
        }
        i += 1;
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review ai-review's leaks, each now with nothing of its value: a shell's `-c` script
    /// (codex #1), a quoted short password and one starting with `-` (codex #1), the short
    /// password options masking had missed (claude #5), and an old job id in a wrapper's path
    /// (claude #4).
    #[test]
    fn the_reviews_examples_send_no_value() {
        let cases = [
            ("bash -lc 'sshpass -p cohort-login-pass ssh host'", "bash -c …"),
            ("sh -c sshpass -p cohort-login-pass ssh host", "sh -c …"),
            ("mysql -p'cohort login pass'", "mysql …"),
            ("sshpass -p -hunter2 ssh host", "sshpass -p … …"),
            ("sshpass -p hunter2 ssh host", "sshpass -p … …"),
            ("python3 train.py --lr 0.1 --token X", "python3 train.py --lr … --token …"),
            ("docker login -u bob -p hunter2", "docker … -u … -p …"),
            ("zip -P hunter2 out.zip notes.txt", "zip -P … …"),
            ("7z a -phunter2 out.7z notes", "7z …"),
            ("mongosh -u bob -p hunter2 db", "mongosh -u … -p … …"),
            ("smbclient -U bob%hunter2 //h/share", "smbclient -U … …"),
            ("htpasswd -b .htpasswd bob hunter2", "htpasswd -b … …"),
            ("lftp -u bob,hunter2 ftp.example.com", "lftp -u … …"),
            ("sh /root/.arena/jobs/20261019-101500-curl-u-bob-hunter2pass/run", "sh <job>"),
            ("redis-cli -a cohort-db-pass ping", "redis-cli -a … …"),
        ];
        for (cmd, want) in cases {
            let got = command(cmd);
            assert_eq!(got, want, "{cmd}");
            for secret in ["hunter2", "cohort", "bob"] {
                assert!(!got.contains(secret), "{secret} survived: {cmd} -> {got}");
            }
        }
    }

    /// Thirty-odd commands as `ps` shows them on the pods and as participants start jobs, each
    /// with values planted (`Qx7…`, `hunter`, `bob`, numbers, hosts, paths): exactly the program,
    /// the script, the flag names — and no value at all.
    #[test]
    fn real_commands_keep_their_shape_and_no_value() {
        let cases = [
            ("/opt/conda/envs/arena-env/bin/python /root/ARENA_materials/chapter1/train.py --lr 3e-4 --epochs=3", "python train.py --lr … --epochs=…"),
            ("/opt/conda/envs/arena-env/bin/python -m ipykernel_launcher -f /root/.local/share/jupyter/runtime/kernel-Qx7a.json", "python -m ipykernel_launcher -f …"),
            ("python -m pytest -x tests/ -k 'part1 and not Qx7slow'", "python -m pytest -x … -k …"),
            ("python -m torch.distributed.run --nproc_per_node=4 train.py --cfg Qx7.yaml", "python -m torch.distributed.run --nproc_per_node=… … --cfg …"),
            ("torchrun --nproc_per_node 4 train.py --wandb-key Qx7key", "torchrun --nproc_per_node … train.py --wandb-key …"),
            ("jupyter-lab --ip=0.0.0.0 --port=8888 --no-browser --allow-root --NotebookApp.token=Qx7tok", "jupyter-lab --ip=… --port=… --no-browser --allow-root --NotebookApp.token=…"),
            ("/usr/bin/python3 /usr/local/bin/jupyter-lab --ServerApp.password=Qx7pw", "python3 … --ServerApp.password=…"),
            ("python -c import os; print(os.environ['Qx7'])", "python -c …"),
            ("node /srv/app/server.js --port 3000 --secret Qx7s", "node server.js --port … --secret …"),
            ("bash run.sh --api-key Qx7k", "bash run.sh --api-key …"),
            ("-bash", "-bash"),
            ("/bin/zsh -l", "zsh -l"),
            ("zsh -ic 'export HF_TOKEN=Qx7hf; python x.py'", "zsh -c …"),
            ("bash --login -c 'curl -u bob:hunter2 http://x'", "bash -c …"),
            ("sh -e /root/.arena/jobs/20261020-120000-x/run", "sh -e <job>"),
            ("tail -f /root/.arena/jobs/20261020-120000-x/log", "tail -f <job>"),
            ("sshd: root@pts/0", "sshd: …"),
            ("sshd: root [priv]", "sshd: …"),
            ("[kswapd0]", "[kswapd0]"),
            ("[kworker/0:1-events]", "[kworker/0:1-events]"),
            ("postgres: bob appdb 10.0.0.9(5432) idle", "postgres: …"),
            ("sleep 3600", "sleep …"),
            ("ssh -i /root/.ssh/Qx7key -p 2222 bob@10.0.0.9", "ssh -i … -p … …"),
            ("curl -H Authorization: Bearer Qx7tok https://api.example.com", "curl -H … …"),
            ("git -c user.name=bob commit -m Qx7msg", "git -c … … -m …"),
            ("wget --user=bob --password=hunter2 https://x/y", "wget --user=… --password=… …"),
            ("HF_TOKEN=Qx7hf WANDB_API_KEY=Qx7w python train.py --steps 10", "… python train.py --steps …"),
            ("env HF_TOKEN=Qx7hf python train.py", "env … train.py"),
            ("cd /root/ARENA_materials && python \"part2.py\" --seed 1", "cd … part2.py --seed …"),
            ("nohup python3 serve.py > /tmp/Qx7.log 2>&1 &", "nohup … serve.py …"),
            ("accelerate launch --num_processes 2 finetune.py --hub-token Qx7", "accelerate … --num_processes … finetune.py --hub-token …"),
            ("jupyter nbconvert --to notebook --execute part1.ipynb", "jupyter … --to … --execute part1.ipynb"),
            ("java -Xmx4g -Dkey=Qx7 -jar app.jar", "java …"),
            ("pip install --index-url https://bob:hunter2@pypi.example.com/simple torch", "pip … --index-url … …"),
            ("aws s3 cp s3://Qx7bucket/data . --profile bob", "aws … --profile …"),
            ("python3 -u train.py -- --lr 0.1", "python3 -u train.py -- --lr …"),
            ("python3 -u -c \"import os; os.system('sshpass -p hunter2 ssh h')\"", "python3 -c …"),
            ("\"$PY\" x.py", "… x.py"),
            ("", ""),
        ];
        assert!(cases.len() >= 30);
        for (cmd, want) in cases {
            let got = command(cmd);
            assert_eq!(got, want, "{cmd}");
            for value in ["Qx7", "hunter", "bob", "3e-4", "0.0.0.0", "8888", "3600", "10.0.0.9", "2222", "/root", "/opt", "4g", "20261020"] {
                assert!(!got.contains(value), "{value} survived: {cmd} -> {got}");
            }
        }
    }

    /// What reads as a flag, a script, a module or a program — and what doesn't.
    #[test]
    fn flags_scripts_and_programs_are_told_strictly() {
        assert_eq!(flag("--lr"), Some(("--lr".into(), false)));
        assert_eq!(flag("--token=x"), Some(("--token=…".into(), true)));
        assert_eq!(flag("-p"), Some(("-p".into(), false)));
        for value in ["-pPASS", "-lc", "-5", "-hunter2", "-", "--5x", "--", "---x", "--a b", "--ünï", &format!("--{}", "x".repeat(65))] {
            assert!(flag(value).is_none_or(|(f, _)| f == "--" && value == "--"), "{value}");
        }
        assert_eq!(script("/w/a/train.py"), Some("train.py"));
        assert_eq!((script(".py"), script("x.py;"), script("x.pyc"), script("\"nb.ipynb\"")), (None, None, None, None));
        assert_eq!((module("torch.distributed.run"), module("x/y"), module("'a'")), (Some("torch.distributed.run"), None, None));
        // Quoted, they're the shell's words: the quotes are taken out first.
        assert_eq!(command("python \"nb.ipynb\" '--lr' 1"), "python nb.ipynb --lr …");
        assert_eq!(command("python -m 'pytest' -x"), "python -m pytest -x");
        assert_eq!((program("/usr/bin/python3"), program("$(cat /k)"), program("\u{1b}[31mx")), ("python3".into(), ELIDED.into(), ELIDED.into()));
        // A key as a program's, a script's or a flag's name is a value; long names of words aren't.
        let key = "3AHPWdlsz7ELTahpw3BIPXelt07FMTbipx4BJQXf";
        for word in [key.to_string(), format!("{key}.py"), format!("--{key}"), "0123456789abcdef0123456789abcdef".into(), "supercalifragilisticexpialidocious".into()] {
            assert_eq!(command(&format!("{word} --go")), "… --go", "{word}");
            assert_eq!(command(&format!("python {word}")), "python …", "{word}");
        }
        assert_eq!(command("python part4_interp_on_algorithmic_model.ipynb --per_device_train_batch_size 8 --IdentityProvider.token=x"),
            "python part4_interp_on_algorithmic_model.ipynb --per_device_train_batch_size … --IdentityProvider.token=…");
        assert!(runs_code("bash", &["-xec", "x"]) && runs_code("python3", &["-u", "-c", "x"]) && !runs_code("bash", &["x.sh", "-c"]) && !runs_code("node", &["-c", "x"]));
    }

    /// Review ai-review codex #1, claude #1 (lane ASK8 item 1): a command's words are the shell's.
    /// A quoted assignment's value with blanks in it (or an escaped blank) is one word, and every
    /// leading assignment is taken before the program — so no piece of a value is ever taken for
    /// the program (`APP_CONFIG='{"password": "cohort-db-pass" }' python train.py`, cut at
    /// blanks, showed `cohort-db-pass` as one).
    #[test]
    fn a_quoted_assignments_value_is_never_the_program() {
        let cases = [
            ("APP_CONFIG='{\"password\": \"cohort-db-pass\" }' python train.py", "… python train.py"),
            ("DB_PASS='correct horse battery staple' python train.py", "… python train.py"),
            ("PASS=correct\\ horse python x.py", "… python x.py"),
            ("ARGS='--lr 0.1 --token sk-cohort' python run.py", "… python run.py"),
            ("A=1 B=\"two cohort words\" C='{\"k\": \"v w\"}' D= E=\"x \\\" y\" python3 run.py --lr 3", "… python3 run.py --lr …"),
            ("CFG=\"a\nb cohort\" python x.py", "… python x.py"),
            ("python train.py --name 'cohort run 1' --lr 0.1", "python train.py --name … --lr …"),
            // A `NAME=` quoted or escaped isn't an assignment: it's the program, and not a plain name.
            ("'X=cohort pass' python x.py", "… … x.py"),
            ("X\\=cohort python x.py", "… … x.py"),
            // Quotes inside a word are the shell's too: this program is python.
            ("py'th'on train.py", "python train.py"),
        ];
        for (cmd, want) in cases {
            let got = command(cmd);
            assert_eq!(got, want, "{cmd}");
            for value in ["cohort", "password", "horse", "staple", "0.1", "sk-", "two", "{"] {
                assert!(!got.contains(value), "{value} survived: {cmd} -> {got}");
            }
        }
    }

    /// Lane ASK8 item 1: quoting that doesn't close — a quote left open, a backslash at the very
    /// end — leaves the words, and so the program, unknown: the whole command is `<command>`.
    #[test]
    fn a_command_whose_quoting_doesnt_close_is_unread() {
        for cmd in [
            "APP_CONFIG='{\"password\": \"cohort-db-pass\" } python train.py",
            "APP_CONFIG=\"{'password': 'cohort-db-pass' } python train.py",
            "python -c 'print(1)",
            "python train.py --token \"cohort",
            "python train.py --token \"cohort \\\"",
            "python train.py \\",
            "echo it's cohort",
        ] {
            assert_eq!(command(cmd), UNREAD, "{cmd}");
        }
        // Closed, the same are words again.
        assert_eq!(command("python -c 'print(1)'"), "python -c …");
        assert_eq!(command("python train.py \\\n --token \"cohort\""), "python train.py --token …");
    }

    /// Review ai-review codex #1, round two (lane ASK9 item 2): a construct whose words this
    /// reading doesn't follow — `$(`, a backtick, `${`, `<(`, `>(`, `<<`, quoted or not — makes
    /// the whole command `<command>` (the review's example: the quote inside `"$(… "password:`
    /// read as the outer one's end made `cohort-db-pass` the program), unless it's in a `-c`
    /// script or after it, which is `…` whatever it holds; before that script — an assignment, the
    /// program, a flag — it's `<command>` too. A plain `$NAME` is no such construct.
    #[test]
    fn a_construct_this_reading_doesnt_follow_is_unread() {
        let example = "APP_CONFIG=\"$(printf '%s' \"password:\n  cohort-db-pass\nhost: db\")\" python train.py";
        for cmd in [
            example,
            "TOKEN=$(echo hunter2 | base64) python x.py",
            "python x.py --key `cat cohort.key`",
            "python x.py --key ${COHORT_KEY}",
            "diff <(cat cohort.txt) b.py",
            "tee >(gzip > cohort.gz) x.py",
            "cat <<EOF\ncohort-db-pass\nEOF",
            "python x.py --k '$(cohort)'",
            "X=$(cohort) bash -c 'python x.py'",
            "bash --rcfile=$(cohort) -c 'python x.py'",
            "${COHORT} x.py",
            "$(cohort)",
        ] {
            assert_eq!(command(cmd), UNREAD, "{cmd}");
        }
        for (cmd, want) in [
            ("bash -c 'echo $(date) `whoami` ${HOME} <(x) <<EOF'", "bash -c …"),
            ("sh -c \"x=\\\"$(printf '%s' \\\"cohort pass\\\")\\\"; y\"", "sh -c …"),
            ("bash -lc 'run' $(cohort) `cohort`", "bash -c …"),
            ("python3 -c \"import os; os.system('$(cohort)')\"", "python3 -c …"),
            ("X=1 bash -c 'echo ${X}'", "… bash -c …"),
            ("\"$PY\" x.py --dir $HOME/w", "… x.py --dir …"),
            ("python x.py 2>&1 > log", "python x.py …"),
        ] {
            assert_eq!(command(cmd), want, "{cmd}");
        }
    }

    /// Lane ASK8 item 1: a program's, a script's or a module's name is shown only as 1 to 40 of
    /// `[A-Za-z0-9._+-]` — quotes, braces, an expansion, or a 41st byte make it `…`.
    #[test]
    fn a_name_with_quotes_braces_or_over_40_bytes_is_a_value() {
        for (cmd, want) in [
            ("'{\"password\": \"cohort\"}' --go", "… --go"),
            ("{cohort,pass} x.py", "… x.py"),
            ("/opt/{cohort}/bin/run --go", "run --go"),
            ("/opt/bin/{cohort} --go", "… --go"),
            ("'my\"cohort' x", "… …"),
            ("\"\" x.py", "… x.py"),
            ("python '{cohort}.py'", "python …"),
            ("python 'cohort pass.py'", "python …"),
            ("python -m '{cohort}'", "python -m …"),
            ("sshd: root@pts/0", "sshd: …"),
            ("'sshd:cohort' x", "… …"),
        ] {
            assert_eq!(command(cmd), want, "{cmd}");
        }
        let (p40, p41) = ("ab-".repeat(13) + "a", "ab-".repeat(13) + "ab");
        assert_eq!(command(&format!("/bin/{p40} --go")), format!("{p40} --go"));
        assert_eq!(command(&format!("/bin/{p41} --go")), "… --go");
        let (s40, s41) = (format!("{}x.py", "ab-".repeat(12)), format!("{}xy.py", "ab-".repeat(12)));
        assert_eq!(command(&format!("python /w/{s40} -x")), format!("python {s40} -x"));
        assert_eq!(command(&format!("python /w/{s41} -x")), "python … -x");
        let (m40, m41) = ("ab.".repeat(13) + "a", "ab.".repeat(13) + "ab");
        assert_eq!(command(&format!("python -m {m40}")), format!("python -m {m40}"));
        assert_eq!(command(&format!("python -m {m41}")), "python -m …");
        assert_eq!(command(&format!("[{p40}]")), format!("[{p40}]"));
        assert_eq!(command(&format!("[{p41}]")), "…");
    }

    /// The lexer itself: quotes and escapes taken out, a line continuation nothing, an empty
    /// quoted word a word, and only a bare leading `NAME=` an assignment.
    #[test]
    fn words_split_as_the_shell_splits() {
        let read = |cmd: &str| words(cmd).map(|v| v.into_iter().map(|w| (w.text, w.assigns)).collect::<Vec<_>>());
        let w = |t: &str, a: bool| (t.to_string(), a);
        assert_eq!(
            read("a 'b c' \"d \\\" \\$e \\x\" f\\ g h\\\ni '' X=1 'Y=2' Z\\=3 _q='a b'=c"),
            Some(vec![w("a", false), w("b c", false), w("d \" $e \\x", false), w("f g", false), w("hi", false), w("", false), w("X=1", true), w("Y=2", false), w("Z=3", false), w("_q=a b=c", true)])
        );
        assert_eq!(read("  \t\n "), Some(vec![]));
        // Where each word is in the command, quotes included.
        let spans = |cmd: &str| words(cmd).unwrap().into_iter().map(|w| w.span).collect::<Vec<_>>();
        assert_eq!(spans(" ab 'c d'\\\n e\"f\" "), vec![(1, 3), (4, 9), (12, 16)]);
        assert_eq!(read("a \\\n b"), Some(vec![w("a", false), w("b", false)]));
        for open in ["'", "\"", "a\\", "\"a\\\"", "'a'\"b"] {
            assert!(read(open).is_none(), "{open}");
        }
    }
}
