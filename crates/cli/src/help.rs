//! The command line as `arena` reads it: clap's tree from the derive, with grouped `--help`
//! for its two long command lists, and whether `--provider` was given.
//!
//! clap prints a command list in code order under one `Commands:` heading: `arena pods --help`
//! was 29 lines to scan for the one you want, `arena --help` 16. Here each is split into a few
//! groups by what the commands do. clap 4 has no heading per subcommand, so the help template is
//! clap's default with its command list swapped for these groups, in clap's styles and
//! alignment; the usage line and the options read as before. Parsing is untouched: prefixes
//! (`infer_subcommands`), aliases and hidden commands work as they did.

use clap::{Command, FromArgMatches};

/// A heading and the commands under it, in the order shown.
type Groups = &'static [(&'static str, &'static [&'static str])];

/// `arena --help`. Every visible top-level command is in exactly one group (a test checks).
pub(crate) const TOP: Groups = &[
    ("Fleet", &["pods", "tui", "snapshot"]),
    ("Money", &["offers", "balance", "teardown"]),
    ("Access", &["proxy", "ssh-config", "keys"]),
    ("Automation", &["cron", "plan", "archive"]),
    ("Setup", &["config", "update", "api"]),
];

/// `arena pods --help`, by what a command does to pods.
pub(crate) const PODS: Groups = &[
    ("Look", &["list", "test", "idle", "ask", "jobs", "logs"]),
    ("Make", &["up", "create", "setup", "start", "copy-keys"]),
    ("Change", &["rename", "set-branch", "run", "cp", "lock", "unlock", "reimage"]),
    ("Move", &["replace", "migrate"]),
    ("Save", &["backup", "pull", "init-branches", "baseline", "restore"]),
    ("Stop", &["stop", "restart", "terminate", "reap"]),
];

/// The CLI's command tree, its two long command lists grouped.
pub(crate) fn command() -> Command {
    use clap::CommandFactory;
    grouped(super::Cli::command(), TOP).mut_subcommand("pods", |pods| grouped(pods, PODS))
}

/// `cmd` with its command list printed in `groups`: clap's default template (`{all-args}` is
/// the command list, then `Options:`), the list replaced.
fn grouped(cmd: Command, groups: Groups) -> Command {
    let list = command_list(&cmd, groups);
    let header = *cmd.get_styles().get_header();
    cmd.help_template(format!(
        "{{before-help}}{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{list}\n{header}Options:{header:#}\n{{options}}{{after-help}}"
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

/// Parse `args` as `Cli::parse` would, over [`command`]; and whether `--provider` was given on
/// the command line — its default (`runpod`) reads the same as `--provider runpod`, and
/// `offers` shows every configured provider unless one is named.
pub(crate) fn parse_from<I, T>(args: I) -> Result<(super::Cli, bool), clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let mut cmd = command();
    let matches = cmd.try_get_matches_from_mut(args)?;
    let named = matches.value_source("provider") == Some(clap::parser::ValueSource::CommandLine);
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
        // The options read as clap's own template prints them.
        let plain = <crate::Cli as clap::CommandFactory>::command().try_get_matches_from(["arena", "pods", "--help"]).unwrap_err().to_string();
        let options = |t: &str| t.split_once("\nOptions:\n").map(|(_, o)| o.trim_end().to_string());
        assert!(options(&plain).is_some());
        assert_eq!(options(&text), options(&plain));
        // A command's own help is clap's as ever (no group list there).
        assert!(help(&["arena", "pods", "list", "--help"]).contains("\nOptions:\n"));
    }

    /// `arena --help` is grouped the same way; `gpus` (hidden, `offers --all`'s old name) isn't
    /// listed; every about is printed as written (a brace in one would garble the template).
    #[test]
    fn top_help_lists_its_commands_in_groups() {
        let text = help(&["arena", "--help"]);
        let headings: Vec<&str> = text.lines().filter(|l| l.ends_with(':') && !l.starts_with(' ')).collect();
        assert_eq!(headings, ["Fleet:", "Money:", "Access:", "Automation:", "Setup:", "Options:"], "{text}");
        assert!(text.contains("\n  offers      ") && !text.contains("gpus"), "{text}");
        let cli = command();
        for sub in cli.get_subcommands().filter(|s| !s.is_hide_set()) {
            let about = sub.get_about().map(|s| s.to_string()).unwrap();
            assert!(text.contains(&about) && !about.contains(['{', '}']), "{}: {about}", sub.get_name());
        }
        let pods = cli.find_subcommand("pods").unwrap();
        for sub in pods.get_subcommands() {
            assert!(!sub.get_about().map(|s| s.to_string()).unwrap_or_default().contains(['{', '}']), "{}", sub.get_name());
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
