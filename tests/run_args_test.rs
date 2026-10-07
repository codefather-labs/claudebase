//! `claudebase run` — the flags it adds to `claude` by default.

use clap::Parser;
use claudebase::cli::{Cli, Command};

fn claude_args(argv: &[&str]) -> Vec<String> {
    let cli = Cli::try_parse_from(std::iter::once("claudebase").chain(argv.iter().copied())).unwrap();
    match cli.command {
        Command::Run(a) => a.claude_args(),
        other => panic!("not a run command: {other:?}"),
    }
}

#[test]
fn defaults_skip_permissions_and_chrome() {
    assert_eq!(claude_args(&["run"]), ["--dangerously-skip-permissions", "--chrome"]);
}

#[test]
fn forwarded_args_follow_the_defaults() {
    assert_eq!(
        claude_args(&["run", "--", "--resume", "abc"]),
        ["--dangerously-skip-permissions", "--chrome", "--resume", "abc"]
    );
}

#[test]
fn no_chrome_opts_out() {
    assert_eq!(claude_args(&["run", "--no-chrome"]), ["--dangerously-skip-permissions"]);
}

#[test]
fn forwarded_chrome_choice_is_not_doubled_or_contradicted() {
    assert_eq!(
        claude_args(&["run", "--", "--no-chrome"]),
        ["--dangerously-skip-permissions", "--no-chrome"]
    );
    assert_eq!(
        claude_args(&["run", "--no-skip-permissions", "--", "--chrome"]),
        ["--chrome"]
    );
}
