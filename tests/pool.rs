//! The pool uses ordinary Git worktrees on both macOS and Linux.
#![cfg(unix)]
use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    repo: PathBuf,
    pool: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let f = Self {
            repo: root.join("repo"),
            pool: root.join("pool"),
            _tmp: tmp,
        };
        fs::create_dir(&f.repo).unwrap();
        f.git(&f.repo, &["init", "-qb", "main", "--template="]);
        f.git(&f.repo, &["config", "user.name", "Pool"]);
        f.git(&f.repo, &["config", "user.email", "pool@example.invalid"]);
        fs::write(f.repo.join("code"), "base").unwrap();
        fs::write(f.repo.join(".gitignore"), ".env\n").unwrap();
        f.git(&f.repo, &["add", "."]);
        f.git(&f.repo, &["commit", "-qm", "init"]);
        f
    }
    fn cmd(&self, name: &str, dir: &Path) -> Command {
        let mut c = Command::new(name);
        c.current_dir(dir)
            .env("HOME", self._tmp.path())
            .env("XDG_CACHE_HOME", self._tmp.path().join("cache"))
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for (k, _) in std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("GIT_")) {
            c.env_remove(k);
        }
        c.env("GIT_CONFIG_NOSYSTEM", "1");
        c
    }
    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        ok(self.cmd("git", cwd).args(args).output().unwrap())
    }
    fn pool(&self, action: &str, args: &[&str]) -> Output {
        self.cmd(env!("CARGO_BIN_EXE_git-worktree-fast"), &self.repo)
            .args(["pool", action])
            .arg(&self.pool)
            .args(args)
            .output()
            .unwrap()
    }
}
fn ok(o: Output) -> String {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap().trim_end().into()
}
fn error(o: Output) -> String {
    assert!(!o.status.success());
    String::from_utf8(o.stderr).unwrap()
}

#[test]
fn concurrent_claims_are_distinct_and_survive_drain() {
    let f = Fixture::new();
    ok(f.pool("prepare", &["5"]));
    let claims = thread::scope(|s| {
        (0..4)
            .map(|_| s.spawn(|| PathBuf::from(ok(f.pool("claim", &[])))))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|a| a.join().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
    });
    assert_eq!(claims.len(), 4);
    ok(f.pool("drain", &[]));
    assert!(!f.pool.join("slot-0005").exists());
    assert_eq!(
        f.git(
            &f.repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/worktree-fast/"
            ]
        )
        .lines()
        .count(),
        4
    );
    error(f.pool("claim", &[]));
    error(f.pool("retire", &[]));
    for path in &claims {
        let branch = f.git(path, &["branch", "--show-current"]);
        assert!(branch.starts_with("worktree-fast/pool-"));
        f.git(&f.repo, &["worktree", "remove", path.to_str().unwrap()]);
        f.git(&f.repo, &["rev-parse", "--verify", &branch]);
    }
    thread::scope(|s| {
        let first = s.spawn(|| f.pool("retire", &[]));
        let second = s.spawn(|| f.pool("retire", &[]));
        assert_ne!(
            first.join().unwrap().status.success(),
            second.join().unwrap().status.success()
        );
    });
    assert!(!f.pool.exists());
}

#[test]
fn drain_preserves_ignored_dirty_locked_and_advanced_slots() {
    let f = Fixture::new();
    ok(f.pool("prepare", &["1"]));
    let path = f.pool.join("slot-0001");
    fs::write(path.join(".env"), "keep").unwrap();
    assert!(error(f.pool("drain", &[])).contains("ignored files changed"));
    fs::remove_file(path.join(".env")).unwrap();
    f.git(&path, &["update-index", "--assume-unchanged", "code"]);
    fs::write(path.join("code"), "new").unwrap();
    assert!(error(f.pool("drain", &[])).contains("unexamined"));
    f.git(&path, &["update-index", "--no-assume-unchanged", "code"]);
    error(f.pool("drain", &[]));
    assert_eq!(fs::read_to_string(path.join("code")).unwrap(), "new");
    f.git(&path, &["restore", "code"]);
    f.git(&f.repo, &["worktree", "lock", path.to_str().unwrap()]);
    error(f.pool("drain", &[]));
    f.git(&f.repo, &["worktree", "unlock", path.to_str().unwrap()]);
    f.git(&path, &["commit", "--allow-empty", "-qm", "advanced"]);
    let tip = f.git(&path, &["rev-parse", "HEAD"]);
    let branch = f.git(&path, &["branch", "--show-current"]);
    error(f.pool("drain", &[]));
    f.git(&f.repo, &["worktree", "remove", path.to_str().unwrap()]);
    ok(f.pool("drain", &[]));
    ok(f.pool("retire", &[]));
    assert_eq!(f.git(&f.repo, &["rev-parse", &branch]), tip);
}

#[test]
fn refuse_replaced_slots_and_foreign_pool_entries() {
    let f = Fixture::new();
    ok(f.pool("prepare", &["1"]));
    let path = f.pool.join("slot-0001");
    let moved = f.pool.with_file_name("moved");
    f.git(
        &f.repo,
        &[
            "worktree",
            "move",
            path.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    symlink(&moved, &path).unwrap();
    error(f.pool("claim", &[]));
    error(f.pool("drain", &[]));
    assert!(moved.join("code").is_file());
    fs::remove_file(&path).unwrap();
    f.git(
        &f.repo,
        &[
            "worktree",
            "move",
            moved.to_str().unwrap(),
            path.to_str().unwrap(),
        ],
    );
    ok(f.pool("drain", &[]));
    fs::write(f.pool.join("unexpected"), "keep").unwrap();
    error(f.pool("retire", &[]));
    assert!(f.pool.join(".worktree-fast-pool.json").is_file());
    let alias = f.pool.with_file_name("foreign");
    symlink(&f.pool, &alias).unwrap();
    let o = f
        .cmd(env!("CARGO_BIN_EXE_git-worktree-fast"), &f.repo)
        .args(["pool", "retire"])
        .arg(alias)
        .output()
        .unwrap();
    error(o);
    assert_eq!(
        fs::read_to_string(f.pool.join("unexpected")).unwrap(),
        "keep"
    );
}

#[test]
fn failed_preparation_is_unpublished_and_can_be_drained() {
    let f = Fixture::new();
    error(f.pool("prepare", &["2", "--t3"]));
    error(f.pool("claim", &[]));
    ok(f.pool("drain", &[]));
    ok(f.pool("retire", &[]));
    assert_eq!(
        f.git(&f.repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
    assert!(
        f.git(&f.repo, &["branch", "--list", "worktree-fast/*"])
            .is_empty()
    );
}
