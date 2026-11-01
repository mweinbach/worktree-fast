mod apfs;
mod checkout;
mod deps;
mod image;
#[cfg(target_os = "macos")]
mod image_deps;
mod pool;
mod setup;

use std::{
    env,
    ffi::{OsStr, OsString},
    process::ExitCode,
};

const HELP: &str = "git worktree-fast — accelerate ordinary Git worktrees on APFS

  git worktree-fast prepare [REV]                      Cache tracked source
  git worktree-fast add -b BRANCH PATH [REV]           Create a native worktree
  git worktree-fast add --detach PATH [REV]            Create a detached worktree
  git worktree-fast forget [REV]                       Delete the source cache
  git worktree-fast deps prepare [--repo REPO]         Prepare the T3 package store
  git worktree-fast setup [--fallback] [CHECKOUT]      Set up an existing T3/Git checkout
  git worktree-fast deps retire [--repo REPO]          Retire an unused package store
  git worktree-fast pool --help                        Manage single-use prewarmed worktrees
  git worktree-fast image --help                       Manage private image-backed worktrees

Other worktree commands are passed to Git. See the README for cache lifetimes.";

fn run(args: &[OsString]) -> anyhow::Result<ExitCode> {
    let command = args.first().map(OsString::as_os_str);
    if command.is_none()
        || ["--help", "-h", "help"]
            .iter()
            .any(|v| command == Some(OsStr::new(v)))
    {
        println!("{HELP}");
        return Ok(ExitCode::SUCCESS);
    }
    if command == Some(OsStr::new("--version")) {
        println!("git-worktree-fast {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    match command.and_then(OsStr::to_str) {
        Some("deps") => setup::run(&args[1..]),
        Some("setup") => setup::run(args),
        Some("pool") => pool::run(&args[1..]),
        Some("image") => image::run(&args[1..]),
        _ => checkout::run(args),
    }
}

fn main() -> ExitCode {
    match run(&env::args_os().skip(1).collect::<Vec<_>>()) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("git-worktree-fast: {error:#}");
            ExitCode::FAILURE
        }
    }
}
