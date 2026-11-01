//! Integration tests invoke the binary through Git's external-subcommand lookup.
#![cfg(unix)]

use std::env;
use std::ffi::OsStr;
use std::fs;
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const HIT: &str = "git-worktree-fast: created from APFS checkout cache";
struct Fixture {
    tmp: TempDir,
    repo: PathBuf,
    path: std::ffi::OsString,
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("source");
        fs::create_dir(&repo).unwrap();
        let bin = Path::new(env!("CARGO_BIN_EXE_git-worktree-fast"))
            .parent()
            .unwrap();
        let mut paths = vec![bin.to_owned()];
        paths.extend(env::split_paths(&env::var_os("PATH").unwrap()));
        let f = Self {
            tmp,
            repo,
            path: env::join_paths(paths).unwrap(),
        };
        f.git(&["init", "-qb", "main", "--template="]);
        f.git(&["config", "user.name", "Fixture"]);
        f.git(&["config", "user.email", "fixture@example.invalid"]);
        f.git(&["config", "commit.gpgSign", "false"]);
        fs::create_dir(f.repo.join("sub")).unwrap();
        fs::write(f.repo.join("sub/tracked"), "original\n").unwrap();
        symlink("sub/tracked", f.repo.join("link")).unwrap();
        fs::write(f.repo.join(".gitignore"), ".env\n").unwrap();
        f.git(&["add", "."]);
        f.git(&["commit", "-qm", "seed"]);
        f
    }
    fn run<I, S>(&self, cwd: &Path, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut git = Command::new("git");
        for (key, _) in env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                git.env_remove(key);
            }
        }
        git.current_dir(cwd)
            .env("PATH", &self.path)
            .env("HOME", self.tmp.path())
            .env("XDG_CACHE_HOME", self.tmp.path().join("cache"))
            .env("XDG_CONFIG_HOME", self.tmp.path().join("config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap()
    }
    fn git(&self, args: &[&str]) -> String {
        success(self.run(&self.repo, args))
    }
    fn fast(&self, args: &[&str]) -> Output {
        self.run(
            &self.repo,
            std::iter::once("worktree-fast").chain(args.iter().copied()),
        )
    }
    fn dest(&self, path: &str) -> PathBuf {
        self.tmp.path().join(path)
    }
}
fn success(run: Output) -> String {
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    String::from_utf8(run.stdout).unwrap().trim_end().into()
}
fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn unprepared_revision_uses_native_git_and_native_lifecycle() {
    let f = Fixture::new();
    let dest = f.dest("native");
    let result = f.fast(&["add", "--detach", path(&dest)]);
    assert!(!String::from_utf8_lossy(&result.stderr).contains(HIT));
    success(result);
    assert_eq!(
        fs::read_to_string(dest.join("sub/tracked")).unwrap(),
        "original\n"
    );
    f.git(&["worktree", "remove", path(&dest)]);
}

#[test]
#[cfg(target_os = "macos")]
fn tracked_only_seed_moves_commits_and_removes_like_git() {
    let f = Fixture::new();
    f.git(&["config", "worktree.useRelativePaths", "true"]);
    f.git(&["config", "branch.autoSetupMerge", "always"]);
    fs::write(f.repo.join("sub/tracked"), "source edit\n").unwrap();
    fs::write(f.repo.join(".env"), "private").unwrap();
    fs::write(f.repo.join("scratch"), "untracked").unwrap();
    let list = f.git(&["worktree", "list", "--porcelain"]);
    success(f.run(&f.repo.join("sub"), ["worktree-fast", "prepare"]));
    assert_eq!(f.git(&["worktree", "list", "--porcelain"]), list);
    let dest = f.dest("chosen path");
    let add = f.run(
        &f.repo.join("sub"),
        [
            "worktree-fast",
            "add",
            "-b",
            "team/feature",
            "../../chosen path",
            "main",
        ],
    );
    assert!(
        String::from_utf8_lossy(&add.stderr).contains(HIT),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    success(add);
    assert_eq!(
        fs::read_to_string(dest.join("sub/tracked")).unwrap(),
        "original\n"
    );
    assert_eq!(
        fs::read_link(dest.join("link")).unwrap(),
        Path::new("sub/tracked")
    );
    assert!(!dest.join(".env").exists() && !dest.join("scratch").exists());
    assert_eq!(
        f.git(&["config", "branch.team/feature.merge"]),
        "refs/heads/main"
    );
    assert_eq!(success(f.run(&dest, ["status", "--porcelain"])), "");
    let moved = f.dest("moved");
    f.git(&["worktree", "move", path(&dest), path(&moved)]);
    success(f.fast(&["forget"]));
    fs::write(moved.join("sub/tracked"), "new edit\n").unwrap();
    success(f.run(&moved, ["commit", "-qam", "working native branch"]));
    assert_eq!(
        f.git(&["rev-parse", "team/feature"]),
        success(f.run(&moved, ["rev-parse", "HEAD"]))
    );
    f.git(&["worktree", "remove", path(&moved)]);
    assert_eq!(f.git(&["worktree", "list", "--porcelain"]), list);
}

#[test]
#[cfg(target_os = "macos")]
fn active_hook_gets_original_git_environment_and_preserves_failed_worktree() {
    let f = Fixture::new();
    let hooks = f.repo.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    fs::write(&hook, "#!/bin/sh\ntest -z \"$GIT_DIR\" || exit 90\ngit ls-files --error-unmatch link >/dev/null || exit 91\nprintf '%s\\n' \"$@\" > .env\nexit 13\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    success(f.fast(&["prepare"]));
    assert!(!f.repo.join(".env").exists());
    let oid = f.git(&["rev-parse", "HEAD"]);
    let dest = f.dest("hooked");
    let result = f.fast(&["add", "-b", "hooked", path(&dest)]);
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stderr).contains(HIT));
    assert_eq!(
        fs::read_to_string(dest.join(".env")).unwrap(),
        format!("{}\n{oid}\n1\n", "0".repeat(oid.len()))
    );
    assert_eq!(f.git(&["rev-parse", "hooked"]), oid);
    f.git(&["worktree", "remove", path(&dest)]);
}

#[test]
#[cfg(target_os = "macos")]
fn misses_filters_and_hook_links_delegate_or_refuse_before_mutation() {
    let f = Fixture::new();
    let first = f.git(&["rev-parse", "HEAD"]);
    success(f.fast(&["prepare"]));
    let hook = f.dest("side-hook");
    fs::write(&hook, "#!/bin/sh\ntouch .env\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(f.repo.join(".hooks")).unwrap();
    symlink("../../side-hook", f.repo.join(".hooks/post-checkout")).unwrap();
    f.git(&["add", ".hooks"]);
    f.git(&["commit", "-qm", "relative hook"]);
    let dest = f.dest("missing");
    let result = f.fast(&["add", "--detach", path(&dest)]);
    assert!(!String::from_utf8_lossy(&result.stderr).contains(HIT));
    success(result);
    f.git(&["worktree", "remove", path(&dest)]);
    f.git(&["config", "core.hooksPath", ".hooks"]);
    success(f.fast(&["prepare"]));
    let result = f.fast(&["add", "--detach", path(&dest)]);
    assert!(!String::from_utf8_lossy(&result.stderr).contains(HIT));
    success(result);
    assert!(dest.join(".env").is_file());
    f.git(&["worktree", "remove", path(&dest)]);
    fs::write(f.repo.join(".gitattributes"), "sub/tracked filter=demo\n").unwrap();
    f.git(&["config", "filter.demo.smudge", "cat"]);
    f.git(&["add", ".gitattributes"]);
    f.git(&["commit", "-qm", "filtered"]);
    assert!(!f.fast(&["prepare"]).status.success());
    success(f.fast(&["forget", &first]));
}

#[test]
#[cfg(target_os = "macos")]
fn collision_has_one_winner_and_cannot_nest_or_replace_it() {
    let f = Fixture::new();
    success(f.fast(&["prepare"]));
    let dest = f.dest("contended");
    let successes = std::thread::scope(|scope| {
        let calls: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|branch| {
                let (f, dest) = (&f, &dest);
                scope.spawn(move || f.fast(&["add", "-b", branch, path(dest)]).status.success())
            })
            .collect();
        calls
            .into_iter()
            .filter_map(|h| h.join().ok())
            .filter(|v| *v)
            .count()
    });
    assert_eq!(successes, 1);
    assert_eq!(
        f.git(&["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        2
    );
    assert!(!dest.join("contended").exists());
    assert!(matches!(
        success(f.run(&dest, ["branch", "--show-current"])).as_str(),
        "one" | "two"
    ));
    f.git(&["worktree", "remove", path(&dest)]);
}

#[test]
#[cfg(target_os = "macos")]
fn direct_and_indirect_cache_poisoning_is_rejected_before_native_fallback() {
    let f = Fixture::new();
    success(f.fast(&["prepare"]));
    let root = f.dest("cache/worktree-fast/v1/checkouts");
    let generation = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
    let oid = f.git(&["rev-parse", "HEAD"]);
    let tree = generation.join(oid).join("tree");
    let alias = f.dest("alias");
    symlink(&tree, &alias).unwrap();
    for options in [
        vec!["add", "-b", "illegal", path(&tree), "HEAD"],
        vec!["add", "-q", "--detach", path(&alias)],
    ] {
        let result = f.fast(&options);
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("outside the tracked checkout cache")
        );
    }
    assert_eq!(
        f.git(&["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[test]
#[cfg(target_os = "macos")]
fn initialized_submodule_behaves_like_native_linked_checkout() {
    let f = Fixture::new();
    let module = f.dest("module-src");
    f.git(&["clone", "-q", path(&f.repo), path(&module)]);
    f.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        path(&module),
        "dependency",
    ]);
    f.git(&["commit", "-qm", "gitlink"]);
    f.git(&["config", "submodule.recurse", "true"]);
    success(f.fast(&["prepare"]));
    let native = f.dest("ordinary");
    let fast = f.dest("fast");
    f.git(&["worktree", "add", "--detach", path(&native)]);
    success(f.fast(&["add", "--detach", path(&fast)]));
    assert_eq!(
        success(f.run(&native, ["submodule", "status"])),
        success(f.run(&fast, ["submodule", "status"]))
    );
    assert!(f.repo.join("dependency/.git").is_file());
    assert!(
        fs::read_dir(fast.join("dependency"))
            .unwrap()
            .next()
            .is_none()
    );
    for w in [&native, &fast] {
        f.git(&["worktree", "remove", "--force", path(w)]);
    }
}
