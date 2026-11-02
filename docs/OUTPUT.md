# How arena prints

Fewer words: every extra word makes it less likely that anything gets read. By default a command
says little, and `-v` says everything. `pods up` and `pods create` are the worked example
(`crates/cli/src/up.rs`, `pods.rs`, `pods/create.rs`, `pods/placement.rs`), with golden tests
in `crates/cli/src/up/output_tests.rs`.

```
$ arena pods up -a 2 --gpu "RTX 6000 Ada" --retry-mins 20
1×RTX 6000 Ada · RunPod SECURE · $0.99/h (none in Europe)
Balance $491.57 → ≈22h left ‼
Create arena9-spike, arena9-spitfire (≤$1.98/h), retrying up to 20m? [y/N] y
round 1: 2 name(s) not placed (no capacity); retrying in 60s (Ctrl+C to stop)…
[created] arena9-spike · 1×RTX 6000 Ada SECURE · $0.99/h
[arena9-spike] READY after 6m12s · keys ✓ · Claude ✓
```

## The rules

1. **By default, answer three questions:** what will happen (before a confirm), what happened,
   and what's wrong, with the one command that fixes it. Everything else goes behind `-v`:
   plans, notes, explanations, ids, raw provider errors, and the chatter of each step.
2. **One line per event.** No paragraphs. Aim for 80 columns.
3. **Repeats collapse.** A failure that repeats every retry round gets one short line per round.
   On a terminal that line is updated in place (`out::progress`).
4. **A provider error becomes a reason** (`out::reason`): `no capacity`, `rate limited`,
   `key rejected`. The HTTP status and the JSON body only show with `-v`.
5. **Style:** bold for names and verdicts. A green ✓ for done, a red ✗ for failed, a yellow ‼
   for a warning that needs action, and dim for secondary detail. Colour never carries the meaning
   alone: the symbol or the word is always there. Use few icons, because they make column widths
   unreliable: ✓ ✗ ‼, and nothing new.
6. **Colour only on a terminal.** `--color auto|always|never` (default `auto`). `NO_COLOR` and
   `TERM=dumb` turn it off. Pipes and cron logs get the same words with no escape codes.
7. **Don't change** `--json` output, exit codes, anything another program reads (below), or the
   confirm prompts' safety (typed names, `ALL`). A confirm still says what it acts on and what it
   costs.
8. **Nothing is lost:** whatever leaves the default output still prints with `-v`.

## The helpers: `arena_core::out`

| | |
|---|---|
| `out!(To::Out, "…", args)` | `println!` that knows `-v`; `out!(Detail, …)` takes a bare level too |
| `ok(s)` `fail(s)` `warn(s)` | `✓ s`, `✗ s`, `‼ s` with the symbol coloured |
| `bold` `dim` `green` `red` `yellow` | styles; plain when colour is off |
| `reason(&err)` / `reason_text(&str)` | `no capacity`, `key rejected`, `provider error (HTTP 502)` |
| `verbose()` | `-v`, for code that branches on it (`balance`, `offers`, `pods test`) |
| `progress(key, Some(line))` / `progress(key, None)` | the in-place line on a terminal; nothing elsewhere |

Each line has a level, `To::…`:

| level | stream | shows |
|---|---|---|
| `Out` / `Err` | stdout / stderr | always |
| `Detail` / `ErrDetail` | stdout / stderr | only with `-v` |
| `Brief` / `ErrBrief` | stdout / stderr | only without `-v`: the short form of what `-v` says at length |

The usual move keeps the old line as `Detail` and adds a `Brief` line beside it, so `-v` prints
exactly what it printed before. A function that takes a `say: &dyn Fn(To, &str)` (like `up`'s
pipelines) passes `&arena_core::out::emit` at runtime and a recorder in tests.

Prompts: `confirm_brief(yes, &what, &question)` asks `question [y/N]` on one line, and with `-v`
asks `what` (the whole plan) and `Proceed? [y/N]` as before. Put what it acts on and what it costs
in the question: `Create devtest-apple, devtest-bloom (≤$0.34/h)?`.

Tests: `crate::output_test::rendered(verbose, &["y"], handler(…))` returns what a terminal would
show, with colour off, the prompts answered as typed, and the error last as `main` prints it.
Write a golden test for default and for `-v` (see `up/output_tests.rs`). Under `capture` nothing
reaches the real terminal and `verbose()` is that capture's own. `-v` and `--color` are set once
by `main` (`out::init`), so nothing needs a flag threaded to it.

## What other programs read (keep it)

- **The dashboard's runs panel** (`crates/tui/src/runs.rs`) runs `arena … --yes` with stdout to a
  log and reads: `[created] NAME`, `[NAME] READY…`, `[NAME] FAILED…`, `[NAME] STOPPED…`, and any
  other `[NAME] …` as the pod's current step. It also reads `round N: …; retrying in Ns (Ctrl+C
  to stop)…`, `interrupted — stopping retries`, and `retry window (`. `NOT_PODS` lists the tags
  that aren't pods. Styles are stripped in a log, so `[{bold(name)}]` still reads the same there.
- **Cron logs** get what a pipe gets. `arena backups` reads only their size and age
  (`crates/core/src/backups.rs`), and the backup run's facts come from its record file, not its
  log. People read the logs, so keep each failure's line and its fix.
- **`--json`**, exit codes, `NoTerminal`'s exit 2, and `--check` statuses (`balance`, `teardown`).
- **`bin/` scripts and docs** that quote output: grep for the line.
- **Tests** that assert on a line. The up tests' recorder keeps what `-v` shows (`Lines::say`).

## Converting a command (phase 2)

1. Run it against the sandbox or a fake, and list its lines at default. Mark each one keep
   (`Out`/`Err`), `Detail`, or replace (a `Brief` beside the old `Detail`).
2. Replace each `println!`/`eprintln!` with `out!(…)`. Keep each line on the stream it used.
3. Plans and long prompts: one line plus `confirm_brief`. Provider errors: `reason` at default,
   the whole error at `-v`.
4. Successes are one line each, failures carry their fix (`… (retry: \`arena pods setup NAME\`)`),
   and a run ends with a tally only when that adds something (`✓ 2 of 2 READY`).
5. Write golden tests with `rendered`, at default and at `-v`. `-v` should match the old output.
6. Grep `crates/tui`, `crates/core/src/backups.rs`, `bin/`, `docs/` and the tests for every line
   you changed.
