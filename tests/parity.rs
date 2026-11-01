//! Compare published APFS worktrees with stock Git using ordinary Git for every later operation.
#![cfg(target_os = "macos")]

use std::ffi::OsStr;
use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

struct Repo {
    dir: TempDir,
    source: PathBuf,
}

impl Repo {
    fn new() -> Self {
        Self::with_format("sha1")
    }

    fn with_format(format: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().canonicalize().unwrap().join("source");
        fs::create_dir(&source).unwrap();
        let f = Self { dir, source };
        f.git(
            &f.source,
            &[
                "init",
                "-qb",
                "main",
                "--template=",
                &format!("--object-format={format}"),
            ],
        );
        f.git(&f.source, &["config", "user.name", "Tester"]);
        f.git(
            &f.source,
            &["config", "user.email", "tester@example.invalid"],
        );
        fs::write(f.source.join("message"), b"original\n").unwrap();
        fs::write(f.source.join("tool"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(f.source.join("tool"), Permissions::from_mode(0o644)).unwrap();
        fs::write(f.source.join(".gitignore"), b".env\n").unwrap();
        symlink("message", f.source.join("link")).unwrap();
        f.git(&f.source, &["add", "."]);
        f.git(&f.source, &["commit", "-qm", "base"]);
        pass(f.run(&f.source, &["prepare"], true));
        f
    }

    fn run(&self, cwd: &Path, args: &[&str], fast: bool) -> Output {
        let name = if fast {
            env!("CARGO_BIN_EXE_git-worktree-fast")
        } else {
            "git"
        };
        let mut cmd = Command::new(name);
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(k);
            }
        }
        cmd.current_dir(cwd)
            .env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("WORKTREE_FAST_TRACE", "1")
            .args(args)
            .output()
            .unwrap()
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> Vec<u8> {
        pass(self.run(cwd, args, false))
    }

    fn sibling(&self, name: &str) -> PathBuf {
        self.source.parent().unwrap().join(name)
    }

    fn seed_tree(&self) -> PathBuf {
        let cache = self.dir.path().join("cache");
        walkdir::WalkDir::new(cache)
            .into_iter()
            .map(Result::unwrap)
            .find(|e| e.path().ends_with("tree/message"))
            .unwrap()
            .into_path()
            .parent()
            .unwrap()
            .to_path_buf()
    }
}

fn pass(out: Output) -> Vec<u8> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn change_preserving_size_and_mtime(path: &Path) {
    let mut f = OpenOptions::new().write(true).open(path).unwrap();
    let info = f.metadata().unwrap();
    assert_eq!(info.len(), 9);
    f.write_all(b"tampered\n").unwrap();
    f.set_modified(info.modified().unwrap()).unwrap();
}

#[test]
fn edits_before_first_status_and_subsequent_native_lifecycle_match_git() {
    let f = Repo::new();
    // Make the cached file's mtime older than the clone's index. Apple Git can
    // compare ctime at second resolution, including after an in-place rewrite.
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    std::thread::sleep(Duration::from_nanos(1_000_000_000_u64 - u64::from(elapsed)));

    let mut observed = Vec::new();
    for (name, fast) in [("native", false), ("cached", true)] {
        let dest = f.sibling(&format!("checkout {name}"));
        let spec = ["add", "-b", name, path(&dest), "HEAD"];
        if fast {
            let out = f.run(&f.source, &spec, true);
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("APFS checkout cache"),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            pass(out);
        } else {
            f.git(&f.source, &[&["worktree"], &spec[..]].concat());
        }

        // No intervening Git command may refresh the new index before this edit.
        change_preserving_size_and_mtime(&dest.join("message"));
        fs::set_permissions(dest.join("tool"), Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(dest.join("link")).unwrap();
        symlink("tool", dest.join("link")).unwrap();
        let first = f.git(
            &dest,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        );
        assert_eq!(&first, b" M link\0 M message\0 M tool\0");
        assert!(
            !f.run(&f.source, &["worktree", "remove", path(&dest)], false)
                .status
                .success()
        );
        assert_eq!(fs::read(dest.join("message")).unwrap(), b"tampered\n");
        f.git(&dest, &["add", "message"]);
        let staged = f.git(&dest, &["diff", "--cached", "--binary"]);
        fs::write(dest.join("message"), b"another change\n").unwrap();
        let status = f.git(&dest, &["status", "--porcelain=v1", "-z"]);
        assert_eq!(&status, b" M link\0MM message\0 M tool\0");
        let unstaged = f.git(&dest, &["diff", "--binary"]);
        let common_dir = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
        assert_eq!(f.git(&dest, &common_dir), f.git(&f.source, &common_dir));
        let index = ["rev-parse", "--path-format=absolute", "--git-path", "index"];
        assert_ne!(f.git(&dest, &index), f.git(&f.source, &index));
        assert!(f.git(&f.source, &["status", "--porcelain"]).is_empty());
        f.git(&dest, &["add", "--all"]);
        f.git(&dest, &["commit", "-qm", "publish changes"]);
        let tree = f.git(&dest, &["rev-parse", "HEAD^{tree}"]);
        observed.push((first, staged, status, unstaged, tree));

        f.git(
            &f.source,
            &["worktree", "lock", "--reason", "kept", path(&dest)],
        );
        assert!(
            !f.run(
                &f.source,
                &["worktree", "remove", "--force", path(&dest)],
                false
            )
            .status
            .success()
        );
        let list = f.git(&f.source, &["worktree", "list", "--porcelain"]);
        assert!(String::from_utf8(list).unwrap().contains("locked kept\n"));
        f.git(&f.source, &["worktree", "unlock", path(&dest)]);
        let moved = f.sibling(&format!("moved {name}"));
        f.git(&f.source, &["worktree", "move", path(&dest), path(&moved)]);
        let repaired = f.sibling(&format!("renamed {name}"));
        fs::rename(moved, &repaired).unwrap();
        f.git(&f.source, &["worktree", "repair", path(&repaired)]);
        assert_eq!(
            f.git(&repaired, &["branch", "--show-current"]),
            format!("{name}\n").as_bytes()
        );
        assert!(f.git(&repaired, &["status", "--porcelain"]).is_empty());
        let head = f.git(&repaired, &["rev-parse", "HEAD"]);
        // Losing an external directory is recovered by normal Git prune.
        fs::remove_dir_all(repaired).unwrap();
        f.git(&f.source, &["worktree", "prune", "--expire=now"]);
        assert_eq!(
            f.git(&f.source, &["rev-parse", &format!("refs/heads/{name}")]),
            head
        );
        let listing =
            String::from_utf8(f.git(&f.source, &["worktree", "list", "--porcelain"])).unwrap();
        assert_eq!(
            listing
                .lines()
                .filter(|line| line.starts_with("worktree "))
                .count(),
            1
        );
    }
    assert_eq!(observed[0], observed[1]);
}

fn tampered_cache_is_never_published(change: impl FnOnce(&Path)) {
    let f = Repo::new();
    change(&f.seed_tree());
    let dest = f.sibling("requested path");
    let before = f.git(&f.source, &["worktree", "list", "--porcelain", "-z"]);
    let add = f.run(&f.source, &["add", "-b", "feature", path(&dest)], true);
    if !add.status.success() {
        assert!(!dest.exists());
        assert_eq!(
            f.git(&f.source, &["worktree", "list", "--porcelain", "-z"]),
            before
        );
        assert!(
            f.git(&f.source, &["branch", "--list", "feature"])
                .is_empty()
        );
        return;
    }
    assert_eq!(fs::read(dest.join("message")).unwrap(), b"original\n");
    assert_eq!(
        fs::read_link(dest.join("link")).unwrap(),
        Path::new("message")
    );
    assert!(fs::symlink_metadata(dest.join(".env")).is_err());
    assert!(
        f.git(&dest, &["status", "--porcelain", "--untracked-files=all"])
            .is_empty()
    );
    f.git(&f.source, &["worktree", "remove", path(&dest)]);
}

#[test]
fn cached_file_changed_without_size_or_mtime_change() {
    tampered_cache_is_never_published(|tree| {
        change_preserving_size_and_mtime(&tree.join("message"))
    });
}

#[test]
fn ignored_file_in_prepared_cache_is_never_copied() {
    tampered_cache_is_never_published(|tree| fs::write(tree.join(".env"), "cache-only\n").unwrap());
}

#[test]
fn replaced_symlink_in_prepared_cache_is_never_copied() {
    tampered_cache_is_never_published(|tree| {
        fs::remove_file(tree.join("link")).unwrap();
        symlink("tool", tree.join("link")).unwrap();
    });
}

#[test]
fn legacy_preparation_falls_back_until_reprepared() {
    let f = Repo::new();
    let tree = f.seed_tree();
    let seed = tree.parent().unwrap();
    let manifest = seed.join("seed.json");
    let mut old: serde_json::Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert!(old.as_object_mut().unwrap().remove("priming").is_some());
    fs::remove_file(seed.join("index")).unwrap();
    fs::write(&manifest, serde_json::to_vec(&old).unwrap()).unwrap();
    let dest = f.sibling("legacy path");
    let add = ["add", "--detach", path(&dest)];
    let cold = f.run(&f.source, &add, true);
    assert!(!String::from_utf8_lossy(&cold.stderr).contains("APFS checkout cache"));
    pass(cold);
    assert_eq!(fs::read(dest.join("message")).unwrap(), b"original\n");
    f.git(&f.source, &["worktree", "remove", path(&dest)]);

    pass(f.run(&f.source, &["prepare"], true));
    let warm = f.run(&f.source, &add, true);
    assert!(String::from_utf8_lossy(&warm.stderr).contains("APFS checkout cache"));
    pass(warm);
    assert_eq!(fs::read(dest.join("message")).unwrap(), b"original\n");
    f.git(&f.source, &["worktree", "remove", path(&dest)]);
    pass(f.run(&f.source, &["forget"], true));
    assert!(!seed.exists());
}

#[test]
fn paths_aliased_by_filesystem_retain_native_dirty_status() {
    let f = Repo::new();
    if !f.source.join("MESSAGE").is_file() {
        return; // On a case-sensitive volume, these paths can represent both blobs.
    }
    let blob = String::from_utf8(f.git(&f.source, &["rev-parse", "HEAD:tool"])).unwrap();
    f.git(
        &f.source,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{},MESSAGE", blob.trim_end()),
        ],
    );
    f.git(
        &f.source,
        &["commit", "-qm", "two distinct blobs with aliased names"],
    );
    let native = f.sibling("native alias");
    f.git(&f.source, &["worktree", "add", "--detach", path(&native)]);
    let expected = f.git(&native, &["status", "--porcelain=v1", "-z"]);
    assert!(!expected.is_empty());
    // A prepare refusal is also safe as long as subsequent add uses ordinary Git.
    let _ = f.run(&f.source, &["prepare"], true);
    let cached = f.sibling("cached alias");
    pass(f.run(&f.source, &["add", "--detach", path(&cached)], true));
    assert_eq!(
        f.git(&cached, &["status", "--porcelain=v1", "-z"]),
        expected
    );
    assert_eq!(
        fs::read(cached.join("message")).unwrap(),
        fs::read(native.join("message")).unwrap()
    );
    for work in [&native, &cached] {
        f.git(&f.source, &["worktree", "remove", "--force", path(work)]);
    }
}

#[test]
fn sha256_crlf_and_unusual_names_keep_native_index_and_checkout_bytes() {
    let f = Repo::with_format("sha256");
    fs::write(
        f.source.join(".gitattributes"),
        b"lines.txt text eol=crlf\n",
    )
    .unwrap();
    fs::write(f.source.join("lines.txt"), b"one\ntwo\n").unwrap();

    // Many APFS volumes reject non-UTF-8 names with EILSEQ. On those volumes
    // use a multibyte name with both a tab and newline to exercise NUL framing.
    let mut name = OsStr::from_bytes(b"odd-\xff\t\n");
    if let Err(e) = fs::write(f.source.join(name), b"name proof\n") {
        assert_eq!(e.raw_os_error(), Some(libc::EILSEQ));
        name = OsStr::new("odd-雪\t\n");
        fs::write(f.source.join(name), b"name proof\n").unwrap();
    }
    f.git(&f.source, &["add", "."]);
    f.git(&f.source, &["commit", "-qm", "checkout encodings"]);
    pass(f.run(&f.source, &["prepare"], true));
    let native = f.sibling("native encoded");
    let cached = f.sibling("cached encoded");
    f.git(&f.source, &["worktree", "add", "--detach", path(&native)]);
    let add = f.run(&f.source, &["add", "--detach", path(&cached)], true);
    assert!(
        String::from_utf8_lossy(&add.stderr).contains("APFS checkout cache"),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    pass(add);
    assert_eq!(
        f.git(&cached, &["rev-parse", "--show-object-format"]),
        b"sha256\n"
    );
    assert_eq!(
        f.git(&cached, &["ls-files", "--stage", "-z"]),
        f.git(&native, &["ls-files", "--stage", "-z"])
    );
    for w in [&native, &cached] {
        assert_eq!(fs::read(w.join(name)).unwrap(), b"name proof\n");
        assert_eq!(fs::read(w.join("lines.txt")).unwrap(), b"one\r\ntwo\r\n");
        assert!(f.git(w, &["status", "--porcelain=v1", "-z"]).is_empty());
        fs::write(w.join("lines.txt"), b"one\r\nchanged\r\n").unwrap();
        f.git(w, &["add", "lines.txt"]);
        assert_eq!(f.git(w, &["show", ":lines.txt"]), b"one\nchanged\n");
        f.git(&f.source, &["worktree", "remove", "--force", path(w)]);
    }
}

#[test]
fn explicit_git_stat_policy_delegates_and_detects_native_edits() {
    let f = Repo::new();
    pass(f.run(&f.source, &["forget"], true));
    f.git(&f.source, &["config", "core.trustctime", "false"]);
    pass(f.run(&f.source, &["prepare"], true));
    let native = f.sibling("native relaxed stat");
    let fallback = f.sibling("fallback relaxed stat");
    f.git(&f.source, &["worktree", "add", "--detach", path(&native)]);
    let add = f.run(&f.source, &["add", "--detach", path(&fallback)], true);
    assert!(!String::from_utf8_lossy(&add.stderr).contains("APFS checkout cache"));
    pass(add);
    for w in [&native, &fallback] {
        fs::write(w.join("message"), b"modified worktree contents\n").unwrap();
        assert_eq!(
            f.git(w, &["status", "--porcelain=v1", "-z"]),
            b" M message\0"
        );
        assert_eq!(f.git(w, &["show", ":message"]), b"original\n");
        assert!(
            !f.run(&f.source, &["worktree", "remove", path(w)], false)
                .status
                .success()
        );
        f.git(&f.source, &["worktree", "remove", "--force", path(w)]);
    }
}
