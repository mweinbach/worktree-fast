use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    tmp: TempDir,
    repo: PathBuf,
    work: PathBuf,
}
fn git(dir: &Path, args: &[&str]) {
    let result = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("project");
        let work = root.join("feature branch");
        fs::create_dir_all(repo.join("scripts")).unwrap();
        fs::write(repo.join("t3.json"),r#"{"scripts":[{"runOnWorktreeCreate":true,"command":"node scripts/setup-worktree.ts"}]}"#).unwrap();
        fs::write(repo.join("scripts/setup-worktree.ts"), "// test action").unwrap();
        git(&repo, &["init", "-q", "--template="]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "fixture"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                work.to_str().unwrap(),
                "HEAD",
            ],
        );
        fs::create_dir(root.join("bin")).unwrap();
        let node = root.join("bin/node");
        fs::write(&node,"#!/bin/sh\nprintf '%s\\n' \"$1\" \"$T3CODE_PROJECT_ROOT\" \"$T3CODE_WORKTREE_PATH\" > ran\nexit 37\n").unwrap();
        fs::set_permissions(node, fs::Permissions::from_mode(0o755)).unwrap();
        Self { tmp, repo, work }
    }
    fn cli(&self, cwd: &Path, args: &[&str]) -> Output {
        let root = self.tmp.path().canonicalize().unwrap();
        Command::new(env!("CARGO_BIN_EXE_git-worktree-fast"))
            .args(args)
            .current_dir(cwd)
            .env_remove("T3CODE_PROJECT_ROOT")
            .env_remove("T3CODE_WORKTREE_PATH")
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .output()
            .unwrap()
    }
}
#[test]
fn miss_preserves_frozen_hook_arguments_worktree_env_and_exit_code() {
    let f = Fixture::new();
    let miss = f.cli(&f.work, &["setup"]);
    assert!(!miss.status.success());
    assert!(!f.work.join("ran").exists());
    let result = f.cli(&f.work, &["setup", "--fallback"]);
    assert_eq!(
        result.status.code(),
        Some(37),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.work.join("ran")).unwrap(),
        format!(
            "scripts/setup-worktree.ts\n{}\n{}\n",
            f.repo.display(),
            f.work.display()
        )
    );
}
#[test]
fn setup_refuses_main_checkout_running_marker_and_modified_action_selection() {
    let f = Fixture::new();
    assert!(!f.cli(&f.repo, &["setup", "--fallback"]).status.success());
    assert!(!f.repo.join("ran").exists());
    fs::create_dir(f.work.join(".t3")).unwrap();
    fs::write(f.work.join(".t3/setup-running"), "101").unwrap();
    assert!(!f.cli(&f.work, &["setup", "--fallback"]).status.success());
    assert!(!f.work.join("ran").exists());
    fs::remove_file(f.work.join(".t3/setup-running")).unwrap();
    fs::write(f.work.join("t3.json"), r#"{"scripts":[]}"#).unwrap();
    assert!(!f.cli(&f.work, &["setup", "--fallback"]).status.success());
    assert!(!f.work.join("ran").exists());
}
#[test]
fn private_store_preparation_rejects_unaudited_revisions() {
    let f = Fixture::new();
    let res = f.cli(&f.repo, &["deps", "prepare"]);
    assert!(!res.status.success());
    let list = Command::new("git")
        .current_dir(&f.repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&list.stdout)
            .lines()
            .filter(|x| x.starts_with("worktree "))
            .count(),
        2
    );
}

#[cfg(unix)]
#[test]
fn store_lease_follows_git_move_and_releases_on_native_removal() {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    let f = Fixture::new();
    let common = f.repo.join(".git");
    let key = hex::encode(Sha256::digest(common.as_os_str().as_bytes()));
    let g = f
        .tmp
        .path()
        .canonicalize()
        .unwrap()
        .join("cache/worktree-fast/v1/deps")
        .join(key)
        .join("generation");
    let store = g.join("store");
    fs::create_dir_all(&store).unwrap();
    for private in [
        &g,
        g.parent().unwrap(),
        g.parent().unwrap().parent().unwrap(),
    ] {
        fs::set_permissions(private, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let id = |p: &Path| {
        let m = p.metadata().unwrap();
        [m.dev(), m.ino()]
    };
    let record = json!({"schema":1,"common":common,"common_id":id(&common),"identity":id(&g),"store":id(&store),"seed":null,
        "ready":true,"retiring":false,"context":"","plan_digest":""});
    fs::write(g.join("generation.json"), record.to_string()).unwrap();
    let admin = fs::read_dir(common.join("worktrees"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let lease =
        json!({"generation":g,"generation_id":id(&g),"checkout":f.work,"identity":id(&f.work)});
    fs::write(admin.join("worktree-fast-pnpm-v1.json"), lease.to_string()).unwrap();
    let moved = f.work.with_file_name("moved");
    git(
        &f.repo,
        &[
            "worktree",
            "move",
            f.work.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    let blocked = f.cli(&f.repo, &["deps", "retire"]);
    assert!(!blocked.status.success());
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("still uses"),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    assert!(store.is_dir());
    git(&f.repo, &["worktree", "remove", moved.to_str().unwrap()]);
    let done = f.cli(&f.repo, &["deps", "retire"]);
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stderr)
    );
    assert!(!g.exists());
}
