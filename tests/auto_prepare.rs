//! Exercise the installed Git hook and its detached worker through real local commits.
#![cfg(target_os = "macos")]

use fs2::FileExt;
use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, File};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Repo {
    tmp: TempDir,
    src: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("repo");
        fs::create_dir(&src).unwrap();
        let r = Self { tmp, src };
        r.git(&r.src, &["init", "-qb", "main", "--template="]);
        r.git(&r.src, &["config", "user.name", "Tester"]);
        r.git(&r.src, &["config", "user.email", "test@example.invalid"]);
        r.commit(&r.src);
        r
    }
    fn run(&self, cwd: &Path, exe: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new(exe);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(name);
            }
        }
        cmd.current_dir(cwd)
            .env("HOME", self.tmp.path())
            .env("XDG_CACHE_HOME", self.tmp.path().join("cache"))
            .env("XDG_CONFIG_HOME", self.tmp.path().join("config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("WORKTREE_FAST_TRACE", "1")
            .args(args)
            .output()
            .unwrap()
    }
    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        ok(self.run(cwd, "git", args))
    }
    fn fast(&self, args: &[&str]) -> String {
        ok(self.run(&self.src, env!("CARGO_BIN_EXE_git-worktree-fast"), args))
    }
    fn commit(&self, cwd: &Path) -> String {
        self.git(cwd, &["commit", "--allow-empty", "-qm", "local"]);
        self.git(cwd, &["rev-parse", "HEAD"])
    }
    fn cache(&self) -> PathBuf {
        let checkouts = self.tmp.path().join("cache/worktree-fast/v1/checkouts");
        fs::read_dir(checkouts)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }
    fn cached(&self, oid: &str) -> bool {
        self.cache().join(oid).join("seed.json").is_file()
    }
    fn expect_cache(&self, commits: &[&str]) {
        let expected: BTreeSet<_> = commits.iter().map(|&s| s.to_owned()).collect();
        wait(|| {
            let actual: BTreeSet<_> = fs::read_dir(self.cache())
                .unwrap()
                .map(Result::unwrap)
                .filter(|e| e.path().join("seed.json").is_file())
                .map(|e| e.file_name().into_string().unwrap())
                .collect();
            actual == expected
        });
    }
    fn ready(&self, branch: &str, oid: &str) {
        let ready = format!("refs/heads/{branch} {oid}");
        wait(|| {
            self.fast(&["auto", "status"])
                .lines()
                .any(|line| line == ready)
                && self.cached(oid)
        });
    }
    fn hook(&self) -> PathBuf {
        self.src.join(".git/hooks/post-commit")
    }
}
fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim_end().to_owned()
}
#[track_caller]
fn wait(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out waiting for auto worker"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn rapid_commits_keep_three_per_branch_and_manual_seeds_survive_eviction() {
    let repo = Repo::new();
    repo.fast(&["prepare"]);
    let manual = repo.git(&repo.src, &["rev-parse", "HEAD"]);
    repo.fast(&["auto", "enable"]);
    assert!(
        repo.fast(&["auto", "status"])
            .contains("Retention limit: 3 per branch")
    );
    repo.git(
        &repo.src,
        &["worktree", "add", "-qb", "team/other", "../other"],
    );
    let linked = repo.tmp.path().join("other");
    // Distinct trees ensure the two branches' empty commits cannot have the same SHA.
    fs::write(linked.join("other"), "other branch\n").unwrap();
    repo.git(&linked, &["add", "other"]);
    let lock = File::open(repo.cache()).unwrap();
    FileExt::lock_exclusive(&lock).unwrap();
    // Hold preparation's own lock while the hooks enqueue several tips. A synchronous
    // post-commit would leave the first Git command blocked here.
    let (shas, other_shas) = std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        let repo = &repo;
        scope.spawn(move || {
            let main: Vec<_> = (0..4).map(|_| repo.commit(&repo.src)).collect();
            let other: Vec<_> = (0..4).map(|_| repo.commit(&linked)).collect();
            tx.send((main, other)).unwrap();
        });
        let completed = rx.recv_timeout(Duration::from_secs(10));
        let status = repo.fast(&["auto", "status"]);
        drop(lock);
        assert!(status.contains("pending refs/heads/main"));
        assert!(status.contains("pending refs/heads/team/other"));
        completed.expect("commits should return while preparation is blocked")
    });
    let last = shas.last().unwrap();
    repo.ready("main", last);
    repo.ready("team/other", &other_shas[3]);
    repo.expect_cache(&[
        &manual,
        &shas[1],
        &shas[2],
        last,
        &other_shas[1],
        &other_shas[2],
        &other_shas[3],
    ]);
    let work = repo.run(
        &repo.src,
        env!("CARGO_BIN_EXE_git-worktree-fast"),
        &["add", "--detach", "../from-auto", last],
    );
    assert!(String::from_utf8_lossy(&work.stderr).contains("created from APFS checkout cache"));
    ok(work);
    assert!(
        repo.git(
            &repo.tmp.path().join("from-auto"),
            &["status", "--porcelain"]
        )
        .is_empty()
    );
    repo.git(&repo.src, &["worktree", "remove", "../from-auto"]);
    assert!(repo.cached(&manual));
    repo.fast(&["prepare", last]); // Explicit prepare promotes an existing automatic seed.
    let next = repo.commit(&repo.src);
    repo.ready("main", &next);
    repo.fast(&["forget", &next]);
    assert!(!repo.fast(&["auto", "status"]).contains(&next));
    repo.fast(&["auto", "notify"]);
    repo.ready("main", &next);
    repo.fast(&["auto", "keep", "1"]);
    repo.expect_cache(&[&manual, last, &next, &other_shas[3]]);
    repo.fast(&["auto", "disable"]);
    assert!(repo.cached(last));
    assert!(!repo.hook().exists());
    let disabled = repo.commit(&repo.src);
    assert!(!repo.cached(&disabled));
}

#[test]
fn two_commits_on_one_branch_capture_their_shas_before_waiting_for_the_queue() {
    let repo = Repo::new();
    repo.fast(&["prepare"]);
    let base = repo.git(&repo.src, &["rev-parse", "HEAD"]);
    repo.fast(&["auto", "enable"]);
    let dir = repo.cache().join("automatic");
    DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let queue = File::open(dir).unwrap();
    FileExt::lock_exclusive(&queue).unwrap();
    let control = File::open(repo.src.join(".git/worktree-fast-auto")).unwrap();

    let (first, second) = std::thread::scope(|s| {
        let a = s.spawn(|| repo.git(&repo.src, &["commit", "--allow-empty", "-qm", "first"]));
        // The hook has reached the queue while holding a shared registration lock.
        wait(|| match FileExt::try_lock_exclusive(&control) {
            Ok(()) => {
                FileExt::unlock(&control).unwrap();
                false
            }
            Err(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
                true
            }
        });
        let first = repo.git(&repo.src, &["rev-parse", "HEAD"]);
        let b = s.spawn(|| repo.git(&repo.src, &["commit", "--allow-empty", "-qm", "second"]));
        wait(|| repo.git(&repo.src, &["rev-parse", "HEAD"]) != first);
        let second = repo.git(&repo.src, &["rev-parse", "HEAD"]);
        drop(queue);
        a.join().unwrap();
        b.join().unwrap();
        (first, second)
    });
    repo.ready("main", &second);
    repo.expect_cache(&[&base, &first, &second]);
    repo.fast(&["auto", "disable"]);
}

#[test]
fn failures_preserve_the_last_successes_and_other_branches_keep_their_caches() {
    let repo = Repo::new();
    repo.fast(&["auto", "enable", "--keep", "2"]);
    let base = repo.commit(&repo.src);
    repo.ready("main", &base);
    let newer = repo.commit(&repo.src);
    repo.ready("main", &newer);
    repo.git(
        &repo.src,
        &["worktree", "add", "-qb", "team/other", "../other"],
    );
    let linked = repo.tmp.path().join("other");
    let other = repo.commit(&linked);
    repo.git(&repo.src, &["worktree", "remove", "../other"]);
    repo.ready("team/other", &other);
    fs::write(
        repo.src.join(".gitattributes"),
        b"data filter=unsupported\n",
    )
    .unwrap();
    fs::write(repo.src.join("data"), b"hello").unwrap();
    repo.git(&repo.src, &["add", "."]);
    let unsupported = repo.commit(&repo.src);
    wait(|| {
        repo.fast(&["auto", "status"])
            .contains("checkout filters must run in their native worktree")
    });
    repo.expect_cache(&[&base, &newer, &other]);
    assert!(!repo.cached(&unsupported));
    repo.git(&repo.src, &["rm", "-q", ".gitattributes"]);
    let tip = repo.commit(&repo.src);
    repo.ready("main", &tip);
    repo.expect_cache(&[&newer, &other, &tip]);
    let final_tip = repo.commit(&repo.src);
    repo.ready("main", &final_tip);
    // Removing a linked worktree did not delete its branch or its retained cache.
    assert_eq!(repo.git(&repo.src, &["rev-parse", "team/other"]), other);
    repo.expect_cache(&[&other, &tip, &final_tip]);
    repo.fast(&["auto", "disable"]);
}

#[test]
fn branches_sharing_a_sha_release_its_single_seed_only_after_the_last_owner() {
    let repo = Repo::new();
    repo.fast(&["auto", "enable", "--keep", "1"]);
    let shared = repo.commit(&repo.src);
    repo.ready("main", &shared);
    repo.git(
        &repo.src,
        &["worktree", "add", "-qb", "feature", "../feature"],
    );
    let feature = repo.tmp.path().join("feature");
    ok(repo.run(
        &feature,
        env!("CARGO_BIN_EXE_git-worktree-fast"),
        &["auto", "notify"],
    ));
    repo.ready("feature", &shared);
    repo.ready("main", &shared);
    repo.expect_cache(&[&shared]);

    let main = repo.commit(&repo.src);
    repo.ready("main", &main);
    repo.ready("feature", &shared);
    repo.expect_cache(&[&main, &shared]);
    repo.git(&repo.src, &["worktree", "remove", "../feature"]);
    repo.git(&repo.src, &["branch", "-D", "feature"]);
    let latest = repo.commit(&repo.src);
    repo.ready("main", &latest);
    repo.expect_cache(&[&latest]);
    repo.fast(&["auto", "disable"]);
}

#[test]
fn retention_can_be_changed_in_place_and_invalid_limits_preserve_settings() {
    let repo = Repo::new();
    let cli = env!("CARGO_BIN_EXE_git-worktree-fast");
    let invalid_enable = repo.run(&repo.src, cli, &["auto", "enable", "--keep", "0"]);
    assert!(!invalid_enable.status.success());
    assert!(!repo.hook().exists());
    repo.fast(&["auto", "keep", "5"]);
    let status = repo.fast(&["auto", "status"]);
    assert!(status.contains("disabled") && status.contains("Retention limit: 5 per branch"));
    assert!(!repo.hook().exists());
    repo.fast(&["auto", "enable", "--keep", "4"]);
    assert!(
        repo.fast(&["auto", "status"])
            .contains("Retention limit: 4 per branch")
    );
    let mut commits = Vec::new();
    for _ in 0..4 {
        let oid = repo.commit(&repo.src);
        repo.ready("main", &oid);
        commits.push(oid);
    }
    for limit in ["0", "-1", "no-limit"] {
        assert!(
            !repo
                .run(&repo.src, cli, &["auto", "keep", limit])
                .status
                .success()
        );
    }
    assert!(
        repo.fast(&["auto", "status"])
            .contains("Retention limit: 4 per branch")
    );
    repo.expect_cache(&[&commits[0], &commits[1], &commits[2], &commits[3]]);
    repo.fast(&["auto", "keep", "2"]);
    // No new commit or notify is needed to apply a lower limit.
    repo.expect_cache(&[&commits[2], &commits[3]]);
    repo.fast(&["auto", "enable"]);
    assert!(
        repo.fast(&["auto", "status"])
            .contains("Retention limit: 2 per branch")
    );
    let newest = repo.commit(&repo.src);
    repo.ready("main", &newest);
    repo.expect_cache(&[&commits[3], &newest]);
    repo.fast(&["auto", "keep", "4"]);
    let another = repo.commit(&repo.src);
    repo.ready("main", &another);
    repo.expect_cache(&[&commits[3], &newest, &another]);
    repo.fast(&["auto", "disable"]);
    repo.fast(&["auto", "enable"]);
    assert!(
        repo.fast(&["auto", "status"])
            .contains("Retention limit: 4 per branch")
    );
    repo.fast(&["auto", "disable"]);
}

#[test]
fn legacy_branch_pins_and_pending_work_survive_the_upgrade() {
    let repo = Repo::new();
    repo.fast(&["prepare"]);
    let manual = repo.git(&repo.src, &["rev-parse", "HEAD"]);
    repo.fast(&["auto", "enable"]);
    let shared = repo.commit(&repo.src);
    repo.ready("main", &shared);
    repo.git(&repo.src, &["branch", "idle", &shared]);
    let older = repo.commit(&repo.src);
    repo.ready("main", &older);
    repo.git(&repo.src, &["switch", "-qc", "feature"]);
    let newer = repo.commit(&repo.src);
    repo.ready("feature", &newer);
    repo.fast(&["auto", "disable"]);
    let pending = repo.commit(&repo.src);
    repo.fast(&["auto", "enable"]);

    // Older registrations had no limit. Their queues held branch pins and
    // pending ref names, but no ordered list of commits.
    let config = repo.src.join(".git/worktree-fast-auto/enabled");
    let mut reg: serde_json::Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    reg.as_object_mut().unwrap().remove("keep");
    fs::write(config, serde_json::to_vec(&reg).unwrap()).unwrap();
    let branch = hex::encode("refs/heads/feature");
    let legacy = serde_json::json!({
        "generation": reg["generation"],
        "pending": [branch],
        "pins": { (hex::encode("refs/heads/main")): older, (branch): newer },
        "retiring": [manual, shared],
        "last_error": null
    });
    fs::write(
        repo.cache().join("automatic/queue.json"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    let status = repo.fast(&["auto", "status"]);
    assert!(status.contains("Retention limit: 3 per branch"));
    assert!(status.contains(&format!("pending refs/heads/feature {pending}")));
    assert!(status.contains(&format!("refs/heads/main {older}")));
    assert!(status.contains(&format!("refs/heads/idle {shared}")));
    repo.fast(&["auto", "keep", "1"]);
    repo.expect_cache(&[&manual, &shared, &older, &newer]);
    repo.fast(&["auto", "notify"]);
    repo.ready("feature", &pending);
    repo.expect_cache(&[&manual, &shared, &older, &pending]);
    repo.fast(&["auto", "disable"]);
}

#[test]
fn activation_and_deactivation_preserve_user_hooks_and_custom_paths() {
    let repo = Repo::new();
    let hook = repo.hook();
    fs::create_dir(hook.parent().unwrap()).unwrap();
    symlink("missing", &hook).unwrap();
    let denied = repo.run(
        &repo.src,
        env!("CARGO_BIN_EXE_git-worktree-fast"),
        &["auto", "enable"],
    );
    assert!(!denied.status.success());
    assert_eq!(fs::read_link(&hook).unwrap(), Path::new("missing"));
    fs::remove_file(&hook).unwrap();
    repo.git(&repo.src, &["config", "hook.example.event", "post-commit"]);
    let denied = repo.run(
        &repo.src,
        env!("CARGO_BIN_EXE_git-worktree-fast"),
        &["auto", "enable", "--existing-hook"],
    );
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("hook.*"));
    repo.git(&repo.src, &["config", "--unset", "hook.example.event"]);
    repo.fast(&["auto", "enable"]);
    // Retrying activation repairs a hook lost between registration and installation.
    fs::remove_file(&hook).unwrap();
    repo.fast(&["auto", "enable"]);
    assert!(hook.is_file());
    fs::write(&hook, b"#!/bin/sh\n# user edits\n").unwrap();
    repo.fast(&["auto", "disable"]);
    assert_eq!(fs::read(&hook).unwrap(), b"#!/bin/sh\n# user edits\n");

    repo.git(&repo.src, &["config", "core.hooksPath", ".custom-hooks"]);
    let custom = repo.src.join(".custom-hooks/post-commit");
    fs::create_dir(custom.parent().unwrap()).unwrap();
    let content = format!(
        "#!/bin/sh\necho hook-ran >>hook-log\n'{}' auto notify\n",
        env!("CARGO_BIN_EXE_git-worktree-fast").replace('\'', "'\\''")
    );
    fs::write(&custom, &content).unwrap();
    fs::set_permissions(&custom, fs::Permissions::from_mode(0o755)).unwrap();
    let denied = repo.run(
        &repo.src,
        env!("CARGO_BIN_EXE_git-worktree-fast"),
        &["auto", "enable"],
    );
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--existing-hook"));
    repo.fast(&["auto", "enable", "--existing-hook", "--keep", "2"]);
    let oid = repo.commit(&repo.src);
    repo.ready("main", &oid);
    repo.fast(&["auto", "disable"]);
    let next = repo.commit(&repo.src);
    assert!(!repo.cached(&next));
    assert_eq!(fs::read_to_string(&custom).unwrap(), content);
    assert_eq!(
        fs::read_to_string(repo.src.join("hook-log")).unwrap(),
        "hook-ran\nhook-ran\n"
    );
}
