//! A command line as `pods ask` sends it, and as a job's id is named from it: what runs — never a
//! value it was given.
//!
//! Masking a command's free-form shell words one value at a time kept leaking new forms, each
//! review another (`bash -lc 'sshpass -p X …'`, `mysql -p'two words'`, `sshpass -p -hunter2`, an
//! old job id in a wrapper's path). So a process's arguments and a job's command aren't masked:
//! their values are dropped. [`command`] keeps only
//! - the program's name (`python3`, `jupyter-lab`, `sshpass`: a path's last part),
//! - the first script or notebook it runs (`train.py`, `run.sh`, `part1.ipynb`, `server.js`: the
//!   last part of its path) or, for python, `-m <module>`,
//! - each flag's name (`--lr`, `-p`, `--token=` for one glued to its value),
//! - `<job>` for a path under arena's jobs dir (a job's wrapper, `sh ~/.arena/jobs/<id>/run`: an
//!   id made before ids were named from this carries the command as typed),
//!
//! and writes every other word as `…`: a flag's value one `…`, any other run of words one more. A
//! shell's `-c` script is `bash -c …`, whatever it holds, and python's `-c` code `python -c …`.
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

/// A script or notebook a program runs, by its extension.
const SCRIPTS: [&str; 4] = [".py", ".sh", ".ipynb", ".js"];

/// Shells whose `-c` takes a script — shown as `-c …` and nothing more (and so is python's).
const SHELLS: [&str; 8] = ["sh", "bash", "dash", "zsh", "ksh", "ash", "mksh", "yash"];

/// The longest program, script, module or flag name shown; a longer one is a value.
const NAME_MAX: usize = 64;

/// `word` without a pair of matching quotes around it (a job's command is shell: `"train.py"`).
fn unquoted(word: &str) -> &str {
    let b = word.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'"' | b'\'') && b[b.len() - 1] == b[0] {
        &word[1..word.len() - 1]
    } else {
        word
    }
}

/// A path's last part.
fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `name` is 1 to [`NAME_MAX`] bytes, each alphanumeric or one of `._+-` or `more`, and holds
/// nothing like a key ([`keyish`]).
fn plain(name: &str, more: &[u8]) -> bool {
    (1..=NAME_MAX).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b) || more.contains(&b)) && !keyish(name)
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

/// A leading `NAME=value` of a shell command (an environment assignment, not the program).
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })
}

/// The program's name as shown: the last part of its path; a kernel thread's `[name]` whole
/// (`[kswapd0]`); `<job>` under the jobs dir; anything else (an expansion, an escape sequence) a
/// value.
fn program(word: &str) -> String {
    let w = unquoted(word);
    if job_path(w) {
        return JOB.to_string();
    }
    if let Some(inner) = w.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        if plain(inner, b":/") {
            return w.to_string();
        }
    }
    let b = base(w);
    if plain(b, b":") {
        b.to_string()
    } else {
        ELIDED.to_string()
    }
}

/// A script or notebook it runs: its path's last part.
fn script(word: &str) -> Option<&str> {
    let b = base(unquoted(word));
    (plain(b, b"") && SCRIPTS.iter().any(|e| b.len() > e.len() && b.ends_with(e))).then_some(b)
}

/// A python module (`-m pytest`, `-m torch.distributed.run`).
fn module(word: &str) -> Option<&str> {
    let w = unquoted(word);
    (plain(w, b"") && w.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && w.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.'))
        .then_some(w)
}

/// A flag's name as shown, and whether its value is glued to it (`--lr=0.1`: shown `--lr=…`, and
/// the next word isn't its value): `--` alone, `--name`, `--name=…`, a dash and one letter (`-p`).
/// `None`: not a flag — a dash with more than one byte after it included (`-pPASS`, `-lc`,
/// `-5`, `-hunter2`): a value may be glued to it, or be it.
fn flag(word: &str) -> Option<(String, bool)> {
    let w = unquoted(word);
    if w == "--" {
        return Some((w.to_string(), false));
    }
    if let Some(long) = w.strip_prefix("--") {
        let (name, glued) = long.split_once('=').map_or((long, false), |(n, _)| (n, true));
        let ok = plain(name, b"") && name.starts_with(|c: char| c.is_ascii_alphabetic()) && !name.contains('+');
        return ok.then(|| if glued { (format!("--{name}={ELIDED}"), true) } else { (format!("--{name}"), false) });
    }
    let b = w.as_bytes();
    (b.len() == 2 && b[0] == b'-' && b[1].is_ascii_alphabetic()).then(|| (w.to_string(), false))
}

/// Whether a shell or python (`program`) runs code given on its command line: a `-c` among the
/// flags before its first other word, alone or in a cluster (`-lc`, `-ec`, `-uc`).
fn runs_code(program: &str, rest: &[&str]) -> bool {
    (SHELLS.contains(&program.trim_start_matches('-')) || python(program))
        && rest.iter().map(|w| unquoted(w)).take_while(|w| w.starts_with('-')).any(|w| {
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
/// - `sh /root/.arena/jobs/<id>/run` → `sh <job>`.
pub fn command(cmd: &str) -> String {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    // Leading `NAME=value`s: the shell's assignments, before the program.
    let at = words.iter().position(|w| !assignment(w)).unwrap_or(words.len());
    if at > 0 {
        out.push(ELIDED.to_string());
    }
    let Some(first) = words.get(at) else { return out.join(" ") };
    let prog = program(first);
    let rest = &words[at + 1..];
    if runs_code(&prog, rest) {
        return [out, vec![prog, "-c".to_string(), ELIDED.to_string()]].concat().join(" ");
    }
    let py = python(&prog);
    out.push(prog);
    let (mut last, mut ran, mut i) = (Last::Shown, false, 0);
    while i < rest.len() {
        let w = rest[i];
        let shown = if job_path(w) {
            Some((JOB.to_string(), Last::Shown))
        } else if let Some(m) = rest.get(i + 1).filter(|_| py && !ran && unquoted(w) == "-m").and_then(|m| module(m)) {
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
            ("python -m pytest -x tests/ -k 'part1 and not Qx7slow'", "python -m pytest -x … -k … …"),
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
        assert_eq!(flag("'--lr'"), Some(("--lr".into(), false)));
        assert_eq!(flag("-p"), Some(("-p".into(), false)));
        for value in ["-pPASS", "-lc", "-5", "-hunter2", "-", "--5x", "--", "---x", "--a b", "--ünï", &format!("--{}", "x".repeat(65))] {
            assert!(flag(value).is_none_or(|(f, _)| f == "--" && value == "--"), "{value}");
        }
        assert_eq!(script("/w/a/train.py"), Some("train.py"));
        assert_eq!(script("\"nb.ipynb\""), Some("nb.ipynb"));
        assert_eq!((script(".py"), script("x.py;"), script("x.pyc")), (None, None, None));
        assert_eq!((module("torch.distributed.run"), module("x/y"), module("'a'")), (Some("torch.distributed.run"), None, Some("a")));
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
}
