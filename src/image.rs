//! Disposable native Git worktrees backed by cloned APFS disk images.
use anyhow::Result;
use std::{ffi::OsString, process::ExitCode};

pub const USAGE: &str = "\
git worktree-fast image prepare NEW_SEED [--revision REV] [--count 1..8] [--size 8g] [--t3]
git worktree-fast image create SEED NEW_INSTANCE BRANCH [BRANCH ...]
git worktree-fast image remove INSTANCE [--discard]
git worktree-fast image retire SEED [--discard]

Prepare from the current Git repository. Each create prints checkout paths,
one per line. Remove a complete instance together; named branches are kept.
--t3 installs the audited T3 revision once and makes dependency-ready copies.
Seed and instance directories must be new and outside the source worktree.
Interrupted builds may be recovered with remove/retire --discard.";

#[cfg(target_os = "macos")]
#[path = "image_darwin.rs"]
mod darwin;

pub fn run(args: &[OsString]) -> Result<ExitCode> {
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    #[cfg(target_os = "macos")]
    {
        darwin::run(args)?;
        Ok(ExitCode::SUCCESS)
    }
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("APFS image worktrees require macOS")
}
