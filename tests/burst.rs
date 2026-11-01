//! Independent callers may create and hand off multiple ordinary Git worktrees at once.
#![cfg(target_os = "macos")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Barrier;
use std::{env, fs, thread};

const COUNT: usize = 8;

fn command(bin: &str, cwd: &Path, base: &Path) -> Command {
    let mut cmd = Command::new(bin);
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.current_dir(cwd)
        .env("HOME", base)
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("XDG_CACHE_HOME", base.join("cache"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("WORKTREE_FAST_TRACE", "1");
    cmd
}

fn checked(mut cmd: Command, args: &[&str]) -> Output {
    let out = cmd.args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn eight_independent_adds_have_native_branches_and_private_indexes() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = base.join("source");
    fs::create_dir(&repo).unwrap();
    let git = |cwd: &Path, args: &[&str]| {
        let out = checked(command("git", cwd, &base), args);
        String::from_utf8(out.stdout).unwrap().trim_end().to_owned()
    };
    git(&repo, &["init", "-qb", "main", "--template="]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    fs::write(repo.join("tracked"), b"original").unwrap();
    git(&repo, &["add", "tracked"]);
    git(&repo, &["commit", "-qm", "initial"]);
    let initial = git(&repo, &["worktree", "list", "--porcelain"]);
    let fast = || command(env!("CARGO_BIN_EXE_git-worktree-fast"), &repo, &base);
    checked(fast(), &["prepare"]);
    let worktrees: Vec<_> = (0..COUNT)
        .map(|i| base.join(format!("topic-{i}")))
        .collect();
    let barrier = Barrier::new(COUNT);
    let hits = thread::scope(|scope| {
        let jobs: Vec<_> = worktrees
            .iter()
            .map(|path| {
                scope.spawn(|| {
                    barrier.wait();
                    let out = checked(fast(), &["add", path.to_str().unwrap()]);
                    String::from_utf8(out.stderr)
                        .unwrap()
                        .contains("created from APFS checkout cache")
                })
            })
            .collect();
        jobs.into_iter()
            .map(|job| usize::from(job.join().unwrap()))
            .sum::<usize>()
    });
    assert!(hits > 0, "the burst must exercise the cache");
    let mut admins = HashSet::new();
    for path in &worktrees {
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(git(path, &["branch", "--show-current"]), name);
        assert_eq!(git(path, &["status", "--porcelain"]), "");
        let admin = PathBuf::from(git(path, &["rev-parse", "--absolute-git-dir"]));
        assert!(admin.join("index").is_file() && admins.insert(admin));
        fs::write(path.join("tracked"), name).unwrap();
        git(path, &["add", "tracked"]);
    }
    for path in &worktrees {
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(git(path, &["show", ":tracked"]), name);
        git(path, &["commit", "-qm", "per-worktree content"]);
        git(&repo, &["worktree", "remove", "--", path.to_str().unwrap()]);
        assert_eq!(git(&repo, &["show", &format!("{name}:tracked")]), name);
    }
    assert_eq!(git(&repo, &["worktree", "list", "--porcelain"]), initial);
    assert_eq!(git(&repo, &["show", "HEAD:tracked"]), "original");
    checked(fast(), &["forget"]);
}
