//! End-to-end checks use temporary Git objects, named branches and real APFS images.
#![cfg(target_os = "macos")]
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run<I, S>(exe: &str, cwd: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(exe);
    cmd.current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_")
            && k != "GIT_CONFIG_GLOBAL"
            && k != "GIT_CONFIG_NOSYSTEM"
        {
            cmd.env_remove(k);
        }
    }
    cmd.output().unwrap()
}
fn output(result: Output) -> String {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}
fn git(p: &Path, args: &[&str]) -> String {
    output(run("git", p, args))
}
fn image(p: &Path, args: &[&str]) -> Output {
    run(
        env!("CARGO_BIN_EXE_git-worktree-fast"),
        p,
        ["image"].into_iter().chain(args.iter().copied()),
    )
}
fn fail(r: Output) {
    assert!(
        !r.status.success(),
        "unexpected success: {}",
        String::from_utf8_lossy(&r.stdout)
    );
}

#[test]
fn grouped_image_git_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = base.join("source");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-qb", "main", "--template="]);
    git(&repo, &["config", "user.name", "Example"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "worktree.useRelativePaths", "true"]);
    fs::write(repo.join("source.txt"), "initial\n").unwrap();
    fs::write(repo.join(".gitignore"), "local\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);
    fs::write(repo.join("local"), "user secret").unwrap();
    let hooks = repo.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    fs::write(
        &hook,
        "#!/bin/sh\ntest -f source.txt && printf '%s\\n' \"$@\" > local\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let seed = base.join("my seed");
    let seed = seed.to_str().unwrap();
    let instance = base.join("my image");
    let instance = instance.to_str().unwrap();
    output(image(
        &repo,
        &["prepare", seed, "--size", "64m", "--count", "2"],
    ));
    assert_eq!(
        git(&repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
    // A late collision must preserve the existing branch and release every
    // image/registration/branch created during this failed call.
    fail(image(
        &repo,
        &["create", seed, instance, "new-branch", "main"],
    ));
    assert!(!Path::new(instance).exists());
    assert!(git(&repo, &["branch", "--list", "new-branch"]).is_empty());
    // One failing callback must also roll back its successfully registered peer.
    let good_hook = fs::read(&hook).unwrap();
    fs::write(
        &hook,
        "#!/bin/sh\ntest \"$(git branch --show-current)\" != fail-one\n",
    )
    .unwrap();
    fail(image(
        &repo,
        &["create", seed, instance, "ok-peer", "fail-one"],
    ));
    assert!(!Path::new(instance).exists());
    assert!(git(&repo, &["branch", "--list", "ok-peer", "fail-one"]).is_empty());
    fs::write(&hook, good_hook).unwrap();
    let works: Vec<PathBuf> = output(image(
        &repo,
        &["create", seed, instance, "topic/one", "topic/two"],
    ))
    .lines()
    .map(PathBuf::from)
    .collect();
    assert_eq!(works.len(), 2);
    // The sealed template has no live registrations and may be retired early.
    output(image(&repo, &["retire", seed]));
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    for (i, w) in works.iter().enumerate() {
        assert_eq!(git(w, &["rev-parse", "HEAD"]), commit);
        assert_eq!(
            git(w, &["branch", "--show-current"]),
            ["topic/one", "topic/two"][i]
        );
        assert_eq!(git(w, &["status", "--porcelain"]), "");
        assert_eq!(
            fs::read_to_string(w.join("local")).unwrap(),
            format!("{}\n{}\n1\n", "0".repeat(commit.len()), commit)
        );
    }
    fs::write(works[0].join("source.txt"), "edited\n").unwrap();
    assert_eq!(
        fs::read_to_string(works[1].join("source.txt")).unwrap(),
        "initial\n"
    );
    fail(image(&repo, &["remove", instance]));
    git(&works[0], &["commit", "-qam", "work"]);
    let advanced = git(&works[0], &["rev-parse", "HEAD"]);
    git(&repo, &["worktree", "lock", works[1].to_str().unwrap()]);
    fail(image(&repo, &["remove", instance, "--discard"]));
    git(&repo, &["worktree", "unlock", works[1].to_str().unwrap()]);
    let extra = works[0].parent().unwrap().join("unrelated");
    fs::write(&extra, "keep").unwrap();
    fail(image(&repo, &["remove", instance, "--discard"]));
    assert_eq!(fs::read_to_string(&extra).unwrap(), "keep");
    fs::remove_file(extra).unwrap();
    git(&repo, &["worktree", "remove", works[1].to_str().unwrap()]);
    // A machine restart may detach the group while native worktrees remain.
    output(run(
        "/usr/bin/hdiutil",
        &repo,
        ["detach".as_ref(), works[0].parent().unwrap().as_os_str()],
    ));
    output(image(&repo, &["remove", instance]));
    assert!(!Path::new(instance).exists());
    assert_eq!(git(&repo, &["rev-parse", "topic/one"]), advanced);
    assert_eq!(git(&repo, &["rev-parse", "topic/two"]), commit);
    assert_eq!(
        git(&repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[test]
fn interrupted_hook_can_be_physically_recovered() {
    use std::time::{Duration, Instant};
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = base.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-qb", "main", "--template="]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Lab",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "root",
        ],
    );
    let seed = base.join("seed");
    let instance = base.join("instance");
    let seed = seed.to_str().unwrap();
    let instance = instance.to_str().unwrap();
    output(image(&repo, &["prepare", seed, "--size", "64m"]));
    let hooks = repo.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    fs::write(
        &hook,
        r#"#!/bin/sh
touch "$IMAGE_GATE.started"
while [ ! -f "$IMAGE_GATE.release" ]; do sleep 0.05; done
"#,
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let gate = base.join("probe");
    let mut child = Command::new(env!("CARGO_BIN_EXE_git-worktree-fast"))
        .args(["image", "create", seed, instance, "task"])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("IMAGE_GATE", &gate)
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !gate.with_extension("started").is_file() {
        assert!(start.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(25));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    fs::write(gate.with_extension("release"), "ok").unwrap();
    fail(image(&repo, &["remove", instance]));
    // A detached child Git hook may briefly retain its cwd after releasing the gate.
    let start = Instant::now();
    loop {
        let r = image(&repo, &["remove", instance, "--discard"]);
        if r.status.success() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{}",
            String::from_utf8_lossy(&r.stderr)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!Path::new(instance).exists());
    assert_eq!(
        git(&repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
    assert_eq!(
        git(&repo, &["rev-parse", "task"]),
        git(&repo, &["rev-parse", "HEAD"])
    );
    output(image(&repo, &["retire", seed]));
}

#[test]
fn eight_new_registrations_share_one_mount() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = base.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-qb", "main", "--template="]);
    fs::write(repo.join("file"), "tracked").unwrap();
    git(&repo, &["add", "file"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Example",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "root",
        ],
    );
    git(&repo, &["config", "worktree.useRelativePaths", "true"]);
    let seed = base.join("seed");
    let instance = base.join("group");
    let seed = seed.to_str().unwrap();
    let instance = instance.to_str().unwrap();
    output(image(
        &repo,
        &["prepare", seed, "--size", "64m", "--count", "8"],
    ));
    let mut args = vec!["create", seed, instance];
    let names: Vec<_> = (1..=8).map(|i| format!("agent/{i}")).collect();
    args.extend(names.iter().map(String::as_str));
    let start = std::time::Instant::now();
    let out = output(image(&repo, &args));
    let ready = start.elapsed();
    let works: Vec<_> = out.lines().map(PathBuf::from).collect();
    assert_eq!(works.len(), 8);
    for (work, branch) in works.iter().zip(&names) {
        assert_eq!(git(work, &["status", "--porcelain"]), "");
        assert_eq!(git(work, &["branch", "--show-current"]), *branch);
        assert_eq!(
            std::fs::read_to_string(work.join("file")).unwrap(),
            "tracked"
        );
        assert_eq!(works[0].parent(), work.parent());
    }
    eprintln!("eight source-only fixture worktrees ready in {ready:?}");
    // Keep a reader in the volume through the first normal eject request.
    // A transient busy result must be retryable without forcing an unmount.
    let reader = fs::File::open(works[0].join("file")).unwrap();
    let owner = Path::new(instance).join("instance.json");
    let hold = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if fs::read(&owner)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|o| o["removing"] == true)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        drop(reader);
    });
    let removed = image(&repo, &["remove", instance]);
    hold.join().unwrap();
    output(removed);
    output(image(&repo, &["retire", seed]));
}
