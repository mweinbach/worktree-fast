//! Exercise consecutive local commits through the same CLI used to publish a worktree.
#![cfg(target_os = "macos")]

use std::fs::{self, File, Permissions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Repo {
    root: TempDir,
    source: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        let repo = Self { root, source };
        repo.git(&["init", "-qb", "main", "--template="]);
        repo.git(&["config", "user.name", "Fixture"]);
        repo.git(&["config", "user.email", "fixture@example.invalid"]);
        repo
    }

    fn run(&self, binary: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new(binary);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        let out = cmd
            .current_dir(&self.source)
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("WORKTREE_FAST_TRACE", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn git(&self, args: &[&str]) -> String {
        String::from_utf8(self.run("git", args).stdout)
            .unwrap()
            .trim_end()
            .into()
    }

    fn fast(&self, args: &[&str]) -> Output {
        self.run(env!("CARGO_BIN_EXE_git-worktree-fast"), args)
    }

    fn commit(&self) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", "local change"]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn tree(&self, oid: &str) -> PathBuf {
        let repos = self.root.path().join("cache/worktree-fast/v1/checkouts");
        fs::read_dir(repos)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join(oid)
            .join("tree")
    }

    fn add(&self, dest: &str, oid: &str) -> PathBuf {
        let path = self.root.path().join(dest);
        let out = self.fast(&["add", "--detach", path.to_str().unwrap(), oid]);
        assert!(String::from_utf8_lossy(&out.stderr).contains("created from APFS checkout cache"));
        path
    }
}

// getattrlist(2) reports the same data-stream ID for pure clones. Identical bytes
// checked out independently have different IDs, even when disk usage looks similar.
fn clone_id(path: &Path) -> u64 {
    let file = File::open(path).unwrap();
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: libc::ATTR_CMNEXT_CLONEID,
    };
    // Darwin packs a four-byte result length followed by the eight-byte ID.
    let mut result = [0u8; 12];
    let status = unsafe {
        libc::fgetattrlist(
            file.as_raw_fd(),
            (&raw mut attrs).cast(),
            result.as_mut_ptr().cast(),
            result.len(),
            libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    assert_eq!(status, 0, "{}", std::io::Error::last_os_error());
    assert_eq!(u32::from_ne_bytes(result[..4].try_into().unwrap()), 12);
    let id = u64::from_ne_bytes(result[4..].try_into().unwrap());
    assert_ne!(id, 0);
    id
}

#[test]
fn consecutive_seeds_share_unchanged_data_and_can_be_forgotten_independently() {
    let repo = Repo::new();
    fs::write(repo.source.join("unchanged"), [0x5a; 8192]).unwrap();
    fs::write(repo.source.join("change"), b"before\n").unwrap();
    let parent = repo.commit();
    repo.fast(&["prepare", &parent]);
    fs::write(repo.source.join("change"), b"after\n").unwrap();
    let child = repo.commit();
    repo.fast(&["prepare", &child]);

    let original = repo.tree(&parent).join("unchanged");
    let cloned = repo.tree(&child).join("unchanged");
    assert_ne!(
        original.metadata().unwrap().ino(),
        cloned.metadata().unwrap().ino()
    );
    assert_eq!(clone_id(&original), clone_id(&cloned));
    let old = repo.add("old", &parent);
    repo.fast(&["forget", &parent]);
    let new = repo.add("new", &child);
    repo.fast(&["forget", &child]);
    assert_eq!(fs::read(old.join("change")).unwrap(), b"before\n");
    assert_eq!(fs::read(new.join("change")).unwrap(), b"after\n");
    assert!(
        repo.git(&["-C", "../new", "status", "--porcelain"])
            .is_empty()
    );
    fs::write(new.join("unchanged"), b"private edit").unwrap();
    assert_eq!(fs::read(old.join("unchanged")).unwrap(), [0x5a; 8192]);
}

#[test]
fn child_checkout_handles_deletions_modes_and_file_directory_transitions() {
    let repo = Repo::new();
    let dir = repo.source.join("dir");
    let file = repo.source.join("file");
    let script = repo.source.join("script");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("old"), b"deleted").unwrap();
    fs::write(&file, b"old file").unwrap();
    fs::write(&script, b"#!/bin/sh\n").unwrap();
    let parent = repo.commit();
    repo.fast(&["prepare", &parent]);
    fs::remove_dir_all(&dir).unwrap();
    symlink("script", &dir).unwrap();
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    fs::write(file.join("new\nname"), b"new file").unwrap();
    fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();
    let child = repo.commit();
    repo.fast(&["prepare", &child]);
    let checkout = repo.add("child", &child);
    assert_eq!(
        fs::read_link(checkout.join("dir")).unwrap(),
        Path::new("script")
    );
    assert_eq!(
        fs::read(checkout.join("file/new\nname")).unwrap(),
        b"new file"
    );
    assert_ne!(
        checkout.join("script").metadata().unwrap().mode() & 0o111,
        0
    );
    assert!(
        repo.git(&["-C", "../child", "status", "--porcelain"])
            .is_empty()
    );
}

#[test]
fn stale_parent_bytes_and_untracked_files_are_never_inherited() {
    let repo = Repo::new();
    fs::write(repo.source.join("stable"), b"committed").unwrap();
    let parent = repo.commit();
    repo.fast(&["prepare", &parent]);
    fs::write(repo.tree(&parent).join("stable"), b"corrupted").unwrap();
    fs::write(repo.tree(&parent).join(".env"), b"private").unwrap();
    fs::write(repo.source.join("new"), b"child").unwrap();
    let child = repo.commit();
    repo.fast(&["prepare", &child]);
    let checkout = repo.add("child", &child);
    assert_eq!(fs::read(checkout.join("stable")).unwrap(), b"committed");
    assert!(!checkout.join(".env").exists());
}

#[test]
fn changing_nested_attributes_rematerializes_unchanged_blobs() {
    let repo = Repo::new();
    let sub = repo.source.join("nested");
    fs::create_dir(&sub).unwrap();
    let attrs = sub.join(".gitattributes");
    fs::write(&attrs, b"*.txt text eol=lf\n").unwrap();
    fs::write(sub.join("message.txt"), b"one\ntwo\n").unwrap();
    let parent = repo.commit();
    repo.fast(&["prepare", &parent]);
    fs::write(&attrs, b"*.txt text eol=crlf\n").unwrap();
    let child = repo.commit();
    assert_eq!(
        repo.git(&["diff", "--name-only", &parent, &child]),
        "nested/.gitattributes"
    );
    repo.fast(&["prepare", &child]);
    let checkout = repo.add("child", &child);
    assert_eq!(
        fs::read(checkout.join("nested/message.txt")).unwrap(),
        b"one\r\ntwo\r\n"
    );
}

#[test]
fn removing_one_of_two_folded_parent_directories_preserves_child_spelling() {
    let repo = Repo::new();
    fs::write(repo.source.join("payload"), b"data").unwrap();
    let blob = repo.git(&["hash-object", "-w", "payload"]);
    for path in ["A/x", "a/y"] {
        repo.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{path}"),
        ]);
    }
    repo.git(&["commit", "-qm", "two directory spellings"]);
    let parent = repo.git(&["rev-parse", "HEAD"]);
    repo.fast(&["prepare", &parent]);
    repo.git(&["update-index", "--force-remove", "--", "A/x"]);
    repo.git(&["commit", "-qm", "retain lowercase directory"]);
    let child = repo.git(&["rev-parse", "HEAD"]);

    // On case-insensitive APFS, opening `a/y` would also succeed with stale
    // directory spelling `A/y`. Inspect the actual directory entry instead.
    let prepare_and_read_names = || {
        repo.fast(&["prepare", &child]);
        fs::read_dir(repo.tree(&child))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    };
    let incremental = prepare_and_read_names();
    repo.fast(&["forget", &child]);
    repo.fast(&["forget", &parent]);
    let cold = prepare_and_read_names();
    assert_eq!(cold, ["a"]);
    assert_eq!(incremental, cold);
}
