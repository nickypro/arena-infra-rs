//! The command line as `arena` reads it: clap's tree from the derive, with grouped `--help`
//! for its two long command lists, and whether `--provider` was given.
//!
//! clap prints a command list in code order under one `Commands:` heading: `arena pods --help`
//! was 29 lines to scan for the one you want, `arena --help` 16. Here each is split into a few
//! groups by what the commands do. clap 4 has no heading per subcommand, so the help template is
//! clap's default with its command list swapped for these groups, in clap's styles and
//! alignment; the usage line and the options read as before. Parsing is untouched: prefixes
//! (`infer_subcommands`), aliases and hidden commands work as they did. clap's own `help`
//! subcommand isn't listed (`-h, --help` is, under Options); `arena help pods` still works.
//!
//! Every command's help is also **compact** ([`compact`]): options one per line, no blank line
//! between them. clap prints `--help` in its "long" layout as soon as any text has a second
//! paragraph (a command's description, one option's detail) — and the long layout puts every
//! option's help on its own line with a blank line after each, so `arena offers --help` was 50
//! lines for 12 options. clap's own switches don't fix that: `next_line_help` only moves the
//! text, and `-h` (the short layout) drops the second paragraphs. So the long texts are moved
//! where the short layout prints them — a command's description above its usage
//! (`before_help`), an option's detail folded into its one help entry — and nothing is left for
//! the long layout: `-h`, `--help` and `arena help <cmd>` all print the same compact page, with
//! every word they had. clap here wraps nothing (that's its `wrap_help` feature, and another
//! dependency), so an option's text is wrapped to end by column [`PAGE_WIDTH`] ([`help_width`]).

use clap::{Command, FromArgMatches};

/// A heading and the commands under it, in the order shown.
type Groups = &'static [(&'static str, &'static [&'static str])];

/// `arena --help`. Every visible top-level command is in exactly one group (a test checks).
pub(crate) const TOP: Groups = &[
    ("Fleet", &["pods", "tui", "snapshot"]),
    ("Access", &["proxy", "ssh-config", "keys", "claude"]),
    ("Money", &["offers", "balance", "spend", "teardown"]),
    ("Automation", &["cron", "plan", "archive", "backups"]),
    ("Setup", &["config", "image", "update", "api"]),
];

/// `arena pods --help`, by what a command does to pods.
pub(crate) const PODS: Groups = &[
    ("Look", &["list", "test", "idle", "ask", "jobs", "logs"]),
    ("Make", &["up", "create", "setup", "start", "copy-keys"]),
    ("Change", &["rename", "set-branch", "run", "cp", "lock", "unlock", "reimage"]),
    ("Move", &["replace", "migrate", "copy-home", "spare"]),
    ("Save", &["backup", "pull", "init-branches", "baseline", "restore"]),
    ("Stop", &["stop", "restart", "terminate", "reap"]),
];

/// The CLI's command tree, every command's help [`compact`], its two long command lists
/// grouped.
pub(crate) fn command() -> Command {
    use clap::CommandFactory;
    let cmd = compact_tree(super::Cli::command());
    grouped(cmd, TOP).mut_subcommand("pods", |pods| grouped(pods, PODS))
}

/// The column an option's help text ends by: clap's own default page width.
const PAGE_WIDTH: usize = 100;

/// How wide an option's spec reads in clap's short layout — `-y, --yes`, `    --gpu <GPU>` —
/// for [`help_width`]. Positionals are listed apart (`Arguments:`): 0.
fn spec_width(arg: &clap::Arg) -> usize {
    if arg.is_positional() {
        return 0;
    }
    let long = arg.get_long().map_or(0, |l| 2 + l.chars().count());
    let value = if arg.get_action().takes_values() {
        let names: Vec<String> = match arg.get_value_names() {
            Some(names) => names.iter().map(|n| n.to_string()).collect(),
            None => vec![arg.get_id().as_str().to_ascii_uppercase()],
        };
        names.iter().map(|n| 3 + n.chars().count()).sum()
    } else {
        0
    };
    4 + long + value
}

/// The whole tree [`compact`]: the global options' widest spec counts in every command, and
/// their own text — printed under every command — fits the widest command's column.
fn compact_tree(cli: Command) -> Command {
    let globals = cli.get_arguments().filter(|a| a.is_global_set()).map(spec_width).max().unwrap_or(0);
    let widest = widest_spec(&cli).max(globals);
    compact(cli, globals, help_width(widest))
}

/// The widest option spec anywhere under `cmd` ([`spec_width`]).
fn widest_spec(cmd: &Command) -> usize {
    let own = cmd.get_arguments().map(spec_width).max().unwrap_or(0);
    cmd.get_subcommands().map(widest_spec).fold(own, usize::max)
}

/// The width a command's option help is wrapped at: what's left of [`PAGE_WIDTH`] after the
/// help column (two spaces, the widest spec — the global options' included, `globals` — and
/// two more), never under 40. Pure.
fn help_width(widest_spec: usize) -> usize {
    PAGE_WIDTH.saturating_sub(2 + widest_spec + 2).max(40)
}

/// The width a command's description is wrapped at.
const ABOUT_WIDTH: usize = 100;

/// clap's default template, minus `{about}`: the description is `before_help` ([`compact`]).
const TEMPLATE: &str = "{before-help}{usage-heading} {usage}\n\n{all-args}{after-help}";

/// `cmd` and every command under it with nothing left for clap's long layout (see the module
/// doc): the description — the long one when there is one — printed above the usage, and each
/// option's paragraphs folded into its one help entry, one line each, wrapped to fit the page
/// ([`help_width`]; `globals`: the global options' widest spec, which clap adds to every
/// command; `global_width`: what a global option's own text is wrapped at, as it's printed in
/// every command). `about` stays as it is: it's the one-line summary a command list shows.
fn compact(cmd: Command, globals: usize, global_width: usize) -> Command {
    let description = cmd.get_long_about().or_else(|| cmd.get_about()).map(|s| s.to_string());
    let width = help_width(cmd.get_arguments().map(spec_width).max().unwrap_or(0).max(globals));
    let mut cmd = cmd.long_about(None).help_template(TEMPLATE).mut_args(|arg| {
        let text = arg.get_long_help().or_else(|| arg.get_help()).map(|s| s.to_string());
        let w = if arg.is_global_set() { global_width } else { width };
        match text {
            Some(t) => arg.long_help(None).help(fold(&t, w)),
            None => arg,
        }
    });
    if let Some(d) = description {
        cmd = cmd.before_help(wrap(&d, ABOUT_WIDTH));
    }
    let names: Vec<String> = cmd.get_subcommands().map(|s| s.get_name().to_string()).collect();
    for name in names {
        cmd = cmd.mut_subcommand(name, |sub| compact(sub, globals, global_width));
    }
    cmd
}

/// An option's help text as one entry: its paragraphs on consecutive lines (a blank line would
/// read as the end of the entry), each word-wrapped at `width` columns. Pure.
pub(crate) fn fold(text: &str, width: usize) -> String {
    let paragraphs: Vec<&str> = text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()).collect();
    wrap(&paragraphs.join("\n"), width)
}

/// `text` with every line word-wrapped at `width` columns (a word longer than that keeps its
/// own line); line breaks already there, blank lines included, are kept. Pure.
pub(crate) fn wrap(text: &str, width: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for source in text.lines() {
        let indent: String = source.chars().take_while(|c| *c == ' ').collect();
        let mut line = indent.clone();
        for word in source.split_whitespace() {
            if line.len() > indent.len() && line.chars().count() + 1 + word.chars().count() > width {
                lines.push(std::mem::replace(&mut line, indent.clone()));
            }
            if line.len() > indent.len() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(if line.trim().is_empty() { String::new() } else { line });
    }
    lines.join("\n")
}

/// `cmd` with its command list printed in `groups`: [`TEMPLATE`] (`{all-args}` is the command
/// list, then `Options:`), the list replaced.
fn grouped(cmd: Command, groups: Groups) -> Command {
    let list = command_list(&cmd, groups);
    let header = *cmd.get_styles().get_header();
    cmd.help_template(format!(
        "{{before-help}}{{usage-heading}} {{usage}}\n\n{list}\n{header}Options:{header:#}\n{{options}}{{after-help}}"
    ))
}

/// The groups as clap lays out a command list: a styled heading, then `  name  about` with
/// the names padded to the longest and visible aliases after the about (`[aliases: copy]`).
/// A name `cmd` doesn't have is left out (the test keeps the groups in step with the tree).
fn command_list(cmd: &Command, groups: Groups) -> String {
    let styles = cmd.get_styles();
    let (header, literal, ctx, ctx_val) = (styles.get_header(), styles.get_literal(), styles.get_context(), styles.get_context_value());
    let longest = groups.iter().flat_map(|(_, names)| names.iter()).map(|n| n.chars().count()).max().unwrap_or(0);
    let mut out = String::new();
    for (i, (title, names)) in groups.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("{header}{title}:{header:#}\n"));
        for name in names.iter() {
            let Some(sub) = cmd.find_subcommand(name) else { continue };
            let about = sub.get_about().or_else(|| sub.get_long_about()).map(|s| s.to_string()).unwrap_or_default();
            let aliases: Vec<String> = sub.get_visible_aliases().map(|a| format!("{ctx_val}{a}{ctx_val:#}")).collect();
            let aliases = if aliases.is_empty() {
                String::new()
            } else {
                format!(" {ctx}[aliases: {ctx:#}{}{ctx}]{ctx:#}", aliases.join(&format!("{ctx}, {ctx:#}")))
            };
            let pad = " ".repeat(longest - name.chars().count());
            out.push_str(&format!("  {literal}{name}{literal:#}{pad}  {about}{aliases}\n"));
        }
    }
    out
}

/// Parse `args` as `Cli::parse` would, over [`command`]; and whether `--provider` was given —
/// its default (`runpod`) reads the same as `--provider runpod`, and `offers` shows every
/// configured provider unless one is named. Anything but the default counts (today the
/// command line is its only other source).
pub(crate) fn parse_from<I, T>(args: I) -> Result<(super::Cli, bool), clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let mut cmd = command();
    let matches = cmd.try_get_matches_from_mut(args)?;
    let named = !matches!(matches.value_source("provider"), None | Some(clap::parser::ValueSource::DefaultValue));
    let cli = super::Cli::from_arg_matches(&matches).map_err(|e| e.format(&mut cmd))?;
    Ok((cli, named))
}

/// [`parse_from`] the process's arguments; a bad command line (or `--help`) exits as clap does.
pub(crate) fn parse() -> (super::Cli, bool) {
    parse_from(std::env::args_os()).unwrap_or_else(|e| e.exit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cmd, PodCmd};

    /// The help text `args` asks for, as printed (styles stripped).
    fn help(args: &[&str]) -> String {
        match parse_from(args) {
            Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => e.to_string(),
            other => panic!("{args:?}: not help: {:?}", other.map(|(_, named)| named)),
        }
    }

    /// Every visible command is in exactly one group, and every grouped name is a visible
    /// command: a new command can't be left out of `--help`, nor a renamed one linger.
    #[test]
    fn every_command_is_in_exactly_one_group() {
        let cli = command();
        let pods = cli.find_subcommand("pods").expect("pods");
        for (cmd, groups) in [(&cli, TOP), (pods, PODS)] {
            let visible: Vec<&str> = cmd.get_subcommands().filter(|s| !s.is_hide_set()).map(|s| s.get_name()).collect();
            let grouped: Vec<&str> = groups.iter().flat_map(|(_, names)| names.iter().copied()).collect();
            for name in &visible {
                let n = grouped.iter().filter(|g| *g == name).count();
                assert_eq!(n, 1, "`{} {name}` is in {n} groups of its --help (want exactly one)", cmd.get_name());
            }
            for name in &grouped {
                assert!(visible.contains(name), "`{}`'s --help groups `{name}`, which isn't a visible command", cmd.get_name());
            }
        }
    }

    /// `arena pods --help`: the groups in order, each command once with its one-line about,
    /// aligned as clap aligns them, the alias kept; then the options exactly as clap printed them.
    #[test]
    fn pods_help_lists_its_commands_in_groups() {
        let text = help(&["arena", "pods", "--help"]);
        let headings: Vec<&str> = text.lines().filter(|l| l.ends_with(':') && !l.starts_with(' ')).collect();
        assert_eq!(headings, ["Look:", "Make:", "Change:", "Move:", "Save:", "Stop:", "Options:"], "{text}");
        assert!(text.starts_with("Pods: create, set up, run, back up, stop, terminate\n\nUsage: arena pods [OPTIONS] <COMMAND>\n\nLook:\n"), "{text}");
        assert!(text.contains("\n  list           List current pods (read-only)\n"), "{text}");
        assert!(text.contains("\n  cp             Copy a local file or dir (-r) to every pod (scp) [aliases: copy]\n"), "{text}");
        assert!(text.contains("\n  init-branches  Create + push each pod's wNdM autocommit branch\n"), "{text}");
        // The options read as clap's own template prints them, here and at the top.
        let options = |t: &str| t.split_once("\nOptions:\n").map(|(_, o)| o.trim_end().to_string());
        for args in [&["arena", "pods", "--help"][..], &["arena", "--help"], &["arena", "pods", "-h"]] {
            let plain = plain_help(args);
            assert!(options(&plain).is_some(), "{args:?}");
            assert_eq!(options(&help(args)), options(&plain), "{args:?}");
        }
    }

    /// The help clap prints without the groups (compact, as every command's is).
    fn plain_help(args: &[&str]) -> String {
        compact_tree(<crate::Cli as clap::CommandFactory>::command()).try_get_matches_from(args).unwrap_err().to_string()
    }

    /// Every command path in the tree (`["pods", "list"]`), hidden ones too.
    fn paths(cmd: &Command, prefix: Vec<String>, out: &mut Vec<Vec<String>>) {
        for sub in cmd.get_subcommands() {
            let mut path = prefix.clone();
            path.push(sub.get_name().to_string());
            out.push(path.clone());
            paths(sub, path, out);
        }
    }

    /// Every command's help — `--help`, `-h` and `arena help <cmd>` alike — lists its options and
    /// arguments one after another, never a blank line between two (clap's long layout put one
    /// after every option), and says everything it used to: the long description at the top,
    /// each option's every paragraph.
    #[test]
    fn every_commands_help_is_compact() {
        let tree = command();
        let mut all = vec![Vec::new()];
        paths(&tree, Vec::new(), &mut all);
        assert!(all.len() > 60, "{}", all.len());
        let raw = <crate::Cli as clap::CommandFactory>::command();
        for path in &all {
            let words: Vec<&str> = path.iter().map(String::as_str).collect();
            let long = help(&[&["arena"][..], &words, &["--help"]].concat());
            assert_eq!(long, help(&[&["arena"][..], &words, &["-h"]].concat()), "{path:?}: -h and --help differ");
            if !path.is_empty() && path[0] != "help" {
                assert_eq!(long, help(&[&["arena", "help"][..], &words].concat()), "{path:?}: `help` differs");
            }
            // Blocks are separated by one blank line; a block holding options or arguments
            // holds all of them: the next one starts with a heading.
            let blocks: Vec<&str> = long.split("\n\n").collect();
            for (i, b) in blocks.iter().enumerate() {
                let first = b.lines().next().unwrap_or("");
                if !matches!(first, "Options:" | "Arguments:") {
                    continue;
                }
                if let Some(next) = blocks.get(i + 1).filter(|n| !n.trim().is_empty()) {
                    let head = next.lines().next().unwrap_or("");
                    assert!(head.ends_with(':') && !head.starts_with(' '), "{path:?}: a blank line inside {first}\n{long}");
                }
            }
            // Every line fits the page (a word too long to break may not).
            for line in long.lines().filter(|l| l.chars().count() > PAGE_WIDTH) {
                assert!(!line.trim().contains(' ') || line.contains("[default:") || line.contains("[aliases:"), "{path:?}: {line:?} is over {PAGE_WIDTH}\n{long}");
            }
            // Nothing the long layout said is lost: the description, every option's text.
            let mut cmd = &raw;
            for w in &words {
                cmd = cmd.find_subcommand(w).unwrap_or(cmd);
            }
            let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
            let page = squash(&long);
            if let Some(about) = cmd.get_long_about().or_else(|| cmd.get_about()) {
                assert!(page.contains(&squash(&about.to_string())), "{path:?}: description lost\n{long}");
            }
            for arg in cmd.get_arguments().filter(|a| !a.is_hide_set()) {
                if let Some(text) = arg.get_long_help().or_else(|| arg.get_help()) {
                    assert!(page.contains(&squash(&text.to_string())), "{path:?} {}: help lost\n{long}", arg.get_id());
                }
            }
        }
    }

    /// Wrapping keeps words whole and line breaks as written; folding an option's paragraphs
    /// leaves no blank line.
    #[test]
    fn wrap_and_fold() {
        assert_eq!(wrap("aa bb cc dd", 5), "aa bb\ncc dd");
        assert_eq!(wrap("  aa bb cc\n\nx", 7), "  aa bb\n  cc\n\nx", "indent kept, blank line kept");
        assert_eq!(wrap("averyveryverylongword b", 5), "averyveryverylongword\nb");
        assert_eq!(fold("One line.\n\nMore detail\nhere.\n\n", 64), "One line.\nMore detail\nhere.");
        // The help column: `--max-price <MAX_PRICE>` is 4 + 11 + 12 wide, its text starts at 31.
        let max_price = clap::Arg::new("max_price").long("max-price").action(clap::ArgAction::Set);
        assert_eq!(spec_width(&max_price), 27);
        assert_eq!((help_width(27), help_width(70)), (69, 40));
    }

    /// `arena --help` is grouped the same way; `gpus` (hidden, RunPod's GPU list's old name) isn't
    /// listed; every about is printed as written (a brace in one would garble the template).
    #[test]
    fn top_help_lists_its_commands_in_groups() {
        let text = help(&["arena", "--help"]);
        let headings: Vec<&str> = text.lines().filter(|l| l.ends_with(':') && !l.starts_with(' ')).collect();
        assert_eq!(headings, ["Fleet:", "Access:", "Money:", "Automation:", "Setup:", "Options:"], "{text}");
        assert!(text.contains("\n  offers      ") && !text.contains("gpus"), "{text}");
        // Each listed line, as `command_list` writes it, is in the help as written: no brace in
        // an about or alias turned into a template tag.
        let cli = command();
        let pods_text = help(&["arena", "pods", "--help"]);
        for (cmd, text, groups) in [(&cli, &text, TOP), (cli.find_subcommand("pods").unwrap(), &pods_text, PODS)] {
            let w = groups.iter().flat_map(|(_, names)| names.iter()).map(|n| n.len()).max().unwrap();
            for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
                let about = sub.get_about().or_else(|| sub.get_long_about()).map(|s| s.to_string()).unwrap_or_default();
                let aliases: Vec<&str> = sub.get_visible_aliases().collect();
                let line = format!("  {:<w$}  {about}", sub.get_name());
                let line = if aliases.is_empty() { line } else { format!("{line} [aliases: {}]", aliases.join(", ")) };
                assert!(text.contains(&format!("\n{line}\n")), "{}: {line:?}\n{text}", sub.get_name());
            }
        }
    }

    /// Parsing is untouched: prefixes, aliases and hidden commands resolve as before, and
    /// `--provider` counts as named only when it's on the command line, before or after the
    /// command.
    #[test]
    fn prefixes_aliases_and_a_named_provider_parse() {
        let parse = |args: &[&str]| parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        assert!(matches!(parse(&["arena", "po", "li"]).0.cmd, Cmd::Pods(PodCmd::List { .. })));
        assert!(parse_from(["arena", "po", "l"]).is_err(), "list, lock and logs: ambiguous, as before");
        assert!(matches!(parse(&["arena", "pods", "copy", "f"]).0.cmd, Cmd::Pods(PodCmd::Cp { .. })));
        assert!(matches!(parse(&["arena", "gp", "--json"]).0.cmd, Cmd::Gpus { json: true }));
        assert!(matches!(parse(&["arena", "of", "--all"]).0.cmd, Cmd::Offers { all: true, .. }));
        let (cli, named) = parse(&["arena", "offers"]);
        assert_eq!((cli.provider.as_str(), named), ("runpod", false));
        for args in [&["arena", "--provider", "runpod", "offers"][..], &["arena", "offers", "--provider", "vast"]] {
            let (cli, named) = parse(args);
            assert!(named, "{args:?}");
            assert_eq!(cli.provider, args[if args[1] == "offers" { 3 } else { 2 }]);
        }
        assert!(parse_from(["arena", "pods", "frobnicate"]).is_err());
    }
}
