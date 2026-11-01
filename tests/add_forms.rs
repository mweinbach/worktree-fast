//! Compare the accelerated add forms with Git at the public worktree boundary.
#![cfg(target_os = "macos")]

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

const HIT: &str = "created from APFS checkout cache";
const FAST: &str = env!("CARGO_BIN_EXE_git-worktree-fast");

struct Repo {
    tmp: TempDir,
    path: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("source");
        fs::create_dir(&path).unwrap();
        let repo = Self { tmp, path };
        repo.git(&["init", "--template=", "-qb", "main"]);
        repo.git(&["config", "user.name", "Worktree test"]);
        repo.git(&["config", "user.email", "test@example.invalid"]);
        fs::write(repo.path.join("file"), b"old").unwrap();
        repo.git(&["add", "file"]);
        repo.git(&["commit", "-qm", "older revision"]);
        repo.git(&["branch", "past"]);
        fs::write(repo.path.join("file"), b"new").unwrap();
        repo.git(&["commit", "-qam", "newer revision"]);
        success(repo.run(FAST, &["prepare", "past"]));
        success(repo.run(FAST, &["prepare", "main"]));
        repo
    }

    fn run(&self, bin: &str, args: &[&str]) -> Output {
        self.invoke(bin, args, false)
    }

    fn invoke(&self, bin: &str, args: &[&str], trace: bool) -> Output {
        let mut command = Command::new(bin);
        for (key, _) in env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        command.env_remove("WORKTREE_FAST_TRACE");
        if trace {
            command.env("WORKTREE_FAST_TRACE", "1");
        }
        command
            .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
            .current_dir(&self.path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", self.tmp.path())
            .env("XDG_CACHE_HOME", self.tmp.path().join("cache"))
            .env("XDG_CONFIG_HOME", self.tmp.path().join("config"))
            .args(args)
            .output()
            .unwrap()
    }

    fn git(&self, args: &[&str]) -> String {
        String::from_utf8(success(self.run("git", args)).stdout)
            .unwrap()
            .trim_end()
            .into()
    }

    fn add(&self, args: &[&str], hit: bool) -> Output {
        let out = success(self.invoke(FAST, args, true));
        assert_eq!(String::from_utf8_lossy(&out.stderr).contains(HIT), hit);
        out
    }

    fn dest(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}
fn success(out: Output) -> Output {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn implicit_and_explicit_branches_follow_git_then_use_native_removal() {
    let r = Repo::new();
    let new = r.dest("my-feature");
    let result = r.add(&["add", s(&new)], true);
    assert!(String::from_utf8_lossy(&result.stdout).starts_with("HEAD is now at "));
    assert_eq!(
        r.git(&["-C", s(&new), "branch", "--show-current"]),
        "my-feature"
    );
    assert_eq!(fs::read(new.join("file")).unwrap(), b"new");
    r.git(&["worktree", "remove", s(&new)]);

    let past = r.dest("past");
    r.add(&["add", "--", s(&past)], true);
    assert_eq!(r.git(&["-C", s(&past), "branch", "--show-current"]), "past");
    assert_eq!(fs::read(past.join("file")).unwrap(), b"old");
    let elsewhere = r.dest("new path");
    let fast = r.run(FAST, &["add", s(&elsewhere), "past"]);
    let native = r.run("git", &["worktree", "add", s(&elsewhere), "past"]);
    assert!(!native.status.success());
    assert_eq!(fast.status, native.status);
    assert_eq!(fast.stderr, native.stderr);
    assert!(!elsewhere.exists());
    r.git(&["worktree", "remove", s(&past)]);

    r.add(&["add", "--", s(&elsewhere), "past"], true);
    assert_eq!(
        r.git(&["-C", s(&elsewhere), "branch", "--show-current"]),
        "past"
    );
    r.git(&["worktree", "remove", s(&elsewhere)]);
    let detached = r.dest("another path");
    r.add(&["add", "-d", "--", s(&detached), "past"], true);
    assert_eq!(r.git(&["-C", s(&detached), "branch", "--show-current"]), "");
    assert_eq!(
        r.git(&["-C", s(&detached), "rev-parse", "HEAD"]),
        r.git(&["rev-parse", "past"])
    );
    r.git(&["worktree", "remove", s(&detached)]);
}

#[test]
fn quiet_upstreams_and_unsupported_native_forms() {
    let r = Repo::new();
    r.git(&["config", "branch.autoSetupMerge", "always"]);
    let dest = r.dest("feature work");
    let out = r.add(&["add", "-q", "-b", "topic", "--", s(&dest), "past"], false);
    assert!(
        out.stdout.is_empty() && out.stderr.is_empty(),
        "quiet output {out:?}"
    );
    assert_eq!(
        r.git(&["-C", s(&dest), "rev-parse", "--abbrev-ref", "@{upstream}"]),
        "past"
    );
    r.git(&["worktree", "remove", s(&dest)]);

    r.git(&["config", "remote.origin.url", "."]);
    r.git(&[
        "config",
        "remote.origin.fetch",
        "+refs/heads/*:refs/remotes/origin/*",
    ]);
    r.git(&["update-ref", "refs/remotes/origin/remoteonly", "past"]);
    r.git(&["config", "worktree.guessRemote", "true"]);
    let remote = r.dest("remoteonly");
    r.add(&["add", s(&remote)], false);
    assert_eq!(
        r.git(&["-C", s(&remote), "rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/remoteonly"
    );
    assert_eq!(fs::read(remote.join("file")).unwrap(), b"old");
    r.git(&["worktree", "remove", s(&remote)]);

    r.add(&["add", "-B", "topic", s(&dest), "main"], false);
    assert_eq!(
        r.git(&["-C", s(&dest), "rev-parse", "HEAD"]),
        r.git(&["rev-parse", "main"])
    );
    r.git(&["worktree", "remove", s(&dest)]);
    let fast = r.run(FAST, &["add", s(&dest), "does-not-exist"]);
    let native = r.run("git", &["worktree", "add", s(&dest), "does-not-exist"]);
    assert!(!native.status.success());
    assert_eq!(fast.status, native.status);
    assert_eq!(fast.stderr, native.stderr);
}

#[test]
fn revision_names_are_not_interpreted_as_filesystem_paths() {
    let r = Repo::new();
    std::os::unix::fs::symlink("absent", r.path.join("past")).unwrap();
    let work = r.dest("work");
    r.add(&["add", s(&work), "past"], true);
    assert_eq!(r.git(&["-C", s(&work), "branch", "--show-current"]), "past");
    r.git(&["worktree", "remove", s(&work)]);

    let ns = r.tmp.path().join("cache/worktree-fast/v1");
    let alias = r.tmp.path().join("cache/WORKTREE-FAST/v1");
    if alias.is_dir() {
        let target = alias.join("checkouts/cannot-write");
        let reject = r.run(FAST, &["add", "--detach", s(&target)]);
        assert!(!reject.status.success() && !ns.join("checkouts/cannot-write").exists());
    }
}

#[test]
fn default_success_streams_and_reset_state_match_stock_git() {
    let native = Repo::new();
    let cached = Repo::new();
    assert_eq!(
        native.git(&["rev-parse", "HEAD"]),
        cached.git(&["rev-parse", "HEAD"])
    );
    for repo in [&native, &cached] {
        repo.git(&["config", "core.abbrev", "12"]);
        repo.git(&["config", "remote.origin.url", "."]);
        repo.git(&[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ]);
        repo.git(&["update-ref", "refs/remotes/origin/upstream", "past"]);
    }
    for (dir, flags, rev) in [
        ("implicit", vec![], None),
        ("existing", vec![], Some("past")),
        ("full-ref", vec![], Some("refs/heads/past")),
        (
            "remote",
            vec!["-b", "local", "--track"],
            Some("origin/upstream"),
        ),
        ("quiet", vec!["-q", "-d"], Some("main")),
    ] {
        let mut results = Vec::new();
        for (repo, bin) in [(&native, "git"), (&cached, FAST)] {
            let dest = repo.dest(dir);
            let mut args = if bin == "git" {
                vec!["worktree", "add"]
            } else {
                vec!["add"]
            };
            args.extend_from_slice(&flags);
            args.extend(["--", s(&dest)]);
            args.extend(rev);
            let result = success(repo.run(bin, &args));
            let orig = repo.git(&["-C", s(&dest), "rev-parse", "ORIG_HEAD"]);
            let reflog = repo.git(&["-C", s(&dest), "reflog", "show", "--format=%gs", "HEAD"]);
            results.push((result.stdout, result.stderr, orig, reflog));
            repo.git(&["worktree", "remove", s(&dest)]);
        }
        assert_eq!(results[0], results[1], "output or state differed for {dir}");
    }
}

#[test]
fn changing_worker_count_does_not_invalidate_prepared_bytes() {
    let r = Repo::new();
    // T3 supplies checkout.workers when adding a previously created branch.
    r.git(&["config", "checkout.workers", "4"]);
    let dest = r.dest("checkout");
    r.add(&["add", s(&dest), "past"], true);
    assert_eq!(r.git(&["-C", s(&dest), "branch", "--show-current"]), "past");
    r.git(&["worktree", "remove", s(&dest)]);
}

#[test]
fn only_checkout_settings_invalidate_prepared_bytes() {
    let r = Repo::new();
    for (key, value) in [
        ("core.editor", "vi"),
        ("core.pager", "cat"),
        ("core.abbrev", "12"),
        ("core.quotePath", "false"),
        ("core.fsyncMethod", "batch"),
    ] {
        r.git(&["config", key, value]);
    }
    let dest = r.dest("unrelated");
    r.add(&["add", "--detach", s(&dest), "past"], true);
    r.git(&["worktree", "remove", s(&dest)]);

    r.git(&["config", "core.autocrlf", "true"]);
    r.add(&["add", "--detach", s(&dest), "past"], false);
    assert_eq!(fs::read(dest.join("file")).unwrap(), b"old");
    r.git(&["worktree", "remove", s(&dest)]);
}

#[test]
fn missing_parent_directories_are_created_like_git() {
    let r = Repo::new();
    let dest = r.dest("worktrees/source/topic");
    r.add(&["add", "-b", "topic", "--", s(&dest), "past"], true);
    assert_eq!(fs::read(dest.join("file")).unwrap(), b"old");
    assert_eq!(r.git(&["-C", s(&dest), "status", "--porcelain"]), "");
    r.git(&["worktree", "remove", s(&dest)]);

    let blocked = r.dest("blocker");
    fs::write(&blocked, b"").unwrap();
    let target = blocked.join("child");
    let fast = r.run(FAST, &["add", "--detach", s(&target), "past"]);
    let native = r.run("git", &["worktree", "add", "--detach", s(&target), "past"]);
    assert!(!native.status.success());
    assert_eq!(fast.status, native.status);
    assert_eq!(fast.stderr, native.stderr);
}
