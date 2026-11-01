//! A cache of tracked checkout bytes. Git owns every published worktree and index.
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use tempfile::Builder;

fn git() -> Command {
    Command::new("git")
}

fn checked(output: Output) -> Result<Vec<u8>> {
    ensure!(
        output.status.success(),
        "git: {}",
        String::from_utf8_lossy(&output.stderr).trim_end()
    );
    Ok(output.stdout)
}

fn output(command: &mut Command) -> Result<Vec<u8>> {
    checked(command.output().context("starting git")?)
}

fn read_path(command: &mut Command) -> Result<PathBuf> {
    let mut bytes = output(command)?;
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn run_git(args: &[OsString]) -> Result<ExitCode> {
    Err(git().arg("worktree").args(args).exec().into())
}

fn valid_oid(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|v| matches!(v, b'0'..=b'9' | b'a'..=b'f'))
}

fn commit_id(revision: &OsStr) -> Result<String> {
    let mut spec = revision.to_os_string();
    spec.push("^{commit}");
    let raw = output(
        git()
            .args(["rev-parse", "--verify", "--end-of-options"])
            .arg(spec),
    )?;
    let id = std::str::from_utf8(&raw)?.trim_end();
    ensure!(valid_oid(id), "invalid commit ID from Git");
    Ok(id.into())
}

fn cache_root(common: &Path) -> Result<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|v| PathBuf::from(v).join("Library/Caches")))
        .context("set HOME or XDG_CACHE_HOME")?;
    let key = hex::encode(Sha256::digest(common.as_os_str().as_bytes()));
    ensure!(base.is_absolute(), "cache home must be absolute");
    let mut root = resolved_path(&base)?;
    for segment in ["worktree-fast", "v1", "checkouts", key.as_str()] {
        root.push(segment);
        match fs::symlink_metadata(&root) {
            Ok(m) => ensure!(
                m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0,
                "unsafe checkout cache namespace: {}",
                root.display()
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(root)
}

// Resolve existing ancestors, including a symlink that precedes a not-yet-created
// directory. Native fallback must never let a user checkout poison another seed.
fn resolved_path(path: &Path) -> Result<PathBuf> {
    let mut canonical = PathBuf::new();
    for part in std::path::absolute(path)?.components() {
        match part {
            Component::ParentDir => {
                canonical.pop();
            }
            Component::CurDir => (),
            other => {
                canonical.push(other.as_os_str());
                if fs::symlink_metadata(&canonical).is_ok_and(|m| m.is_symlink()) {
                    canonical = canonical
                        .canonicalize()
                        .with_context(|| format!("unresolved symlink {}", canonical.display()))?;
                }
            }
        }
    }
    Ok(canonical)
}

fn lock(root: &Path, write: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(root)?;
    let m = file.metadata()?;
    ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "expected private, user-owned checkout cache at {}",
        root.display()
    );
    if write {
        FileExt::lock_exclusive(&file)?
    } else {
        FileExt::lock_shared(&file)?
    }
    Ok(file)
}

fn context(common: &Path) -> Result<Option<String>> {
    const OVERRIDES: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_ATTR_SOURCE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_REPLACE_REF_BASE",
    ];
    if !cfg!(target_os = "macos") || OVERRIDES.iter().any(|k| env::var_os(k).is_some()) {
        return Ok(None);
    }
    if !output(git().args([
        "for-each-ref",
        "--count=1",
        "--format=%(refname)",
        "refs/replace/",
    ]))?
    .is_empty()
    {
        return Ok(None);
    }
    for location in ["GIT_ATTR_GLOBAL", "GIT_ATTR_SYSTEM"] {
        if read_path(git().args(["var", location]))?.is_file() {
            return Ok(None);
        }
    }
    if common.join("info/attributes").is_file() {
        return Ok(None);
    }

    // This single-threaded subcommand reads and immediately restores its umask.
    let mask = unsafe { libc::umask(0o777) };
    unsafe { libc::umask(mask) };
    let stat = common.metadata()?;
    let mut hash = Sha256::new();
    hash.update(format!("{}/{}/{mask:o}/", stat.dev(), stat.ino()));
    hash.update(output(git().arg("--version"))?);
    let config = output(git().args(["config", "--null", "--list"]))?;
    for record in config.split(|&v| v == 0).filter(|v| !v.is_empty()) {
        let key = record.split(|&v| v == b'\n').next().unwrap_or_default();
        if key.starts_with(b"includeif.")
            || key.starts_with(b"hook.")
            || matches!(
                key,
                b"core.worktree"
                    | b"core.sparsecheckout"
                    | b"extensions.worktreeconfig"
                    | b"core.fsmonitor"
            )
        {
            return Ok(None);
        }
        if [
            b"core.".as_slice(),
            b"extensions.",
            b"filter.",
            b"checkout.",
            b"submodule.",
        ]
        .iter()
        .any(|s| key.starts_with(s))
            && !matches!(
                key,
                b"core.repositoryformatversion" | b"extensions.relativeworktrees"
            )
        {
            hash.update(record);
            hash.update([0]);
        }
    }
    Ok(Some(hex::encode(hash.finalize())))
}

#[derive(Serialize, Deserialize)]
struct Seed {
    common_dev: u64,
    common_ino: u64,
    commit: String,
    context: String,
    tree_dev: u64,
    tree_ino: u64,
}

impl Seed {
    fn read(dir: &Path, common: &Path, oid: &str, deleting: bool) -> Result<Self> {
        ensure!(fs::symlink_metadata(dir)?.is_dir(), "invalid cache entry");
        let marker = dir.join("seed.json");
        ensure!(
            fs::symlink_metadata(&marker)?.is_file(),
            "invalid cache manifest"
        );
        let seed: Self = serde_json::from_reader(File::open(marker)?)?;
        let stat = common.metadata()?;
        ensure!(
            seed.commit == oid && seed.common_dev == stat.dev() && seed.common_ino == stat.ino(),
            "cache identity changed"
        );
        for entry in fs::read_dir(dir)? {
            let name = entry?.file_name();
            ensure!(
                name == "seed.json" || name == "tree",
                "unowned file in checkout cache"
            );
        }
        match fs::symlink_metadata(dir.join("tree")) {
            Ok(m) => ensure!(
                m.is_dir() && seed.tree_dev == m.dev() && seed.tree_ino == m.ino(),
                "cached tree was replaced"
            ),
            Err(e) if deleting && e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        Ok(seed)
    }
}

pub(crate) fn check_filters(common: &Path, oid: &str) -> Result<()> {
    // Read paths from a file rather than writing to a pipe while Git's
    // attribute output is blocked on a full stdout buffer.
    let mut paths = tempfile::tempfile()?;
    let names = git()
        .arg("--git-dir")
        .arg(common)
        .current_dir(common)
        .args(["ls-tree", "--full-tree", "-r", "--name-only", "-z", oid])
        .stdout(paths.try_clone()?)
        .status()?;
    ensure!(names.success(), "git ls-tree failed");
    std::io::Seek::rewind(&mut paths)?;
    let attrs = output(
        git()
            .arg("--git-dir")
            .arg(common)
            .current_dir(common)
            .args([
                "check-attr",
                &format!("--source={oid}"),
                "-z",
                "--stdin",
                "filter",
            ])
            .stdin(Stdio::from(paths)),
    )?;
    ensure!(
        attrs
            .split(|&x| x == 0)
            .skip(2)
            .step_by(3)
            .all(|v| v == b"unspecified" || v == b"unset"),
        "checkout filters must run in their native worktree"
    );
    Ok(())
}

fn prepare(root: &Path, common: &Path, oid: &str) -> Result<()> {
    let seal = context(common)?
        .context("this Git environment does not support prepared APFS checkouts")?;
    DirBuilder::new().recursive(true).mode(0o700).create(root)?;
    let _lock = lock(root, true)?;
    let dest = root.join(oid);
    ensure!(
        !root.join(format!(".forget-{oid}")).try_exists()?,
        "interrupted cleanup; run forget again"
    );
    if dest.try_exists()? {
        ensure!(
            Seed::read(&dest, common, oid, false)?.context == seal,
            "checkout settings changed; forget the old seed first"
        );
        return Ok(());
    }
    let tmp = Builder::new().prefix(".prepare-").tempdir_in(root)?;
    check_filters(common, oid)?;
    let seed = tmp.path().join("seed");
    let tree = seed.join("tree");
    DirBuilder::new().mode(0o700).create(&seed)?;
    fs::create_dir(&tree)?;
    for args in [
        &["read-tree", "--reset", "--no-sparse-checkout", oid][..],
        &["checkout-index", "--all"][..],
    ] {
        output(
            git()
                .current_dir(&tree)
                .env("GIT_INDEX_FILE", tmp.path().join("index"))
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "core.splitIndex=false",
                    "-c",
                    "core.fsmonitor=false",
                ])
                .arg("--git-dir")
                .arg(common)
                .arg("--work-tree")
                .arg(&tree)
                .args(args),
        )?;
    }
    let stat = common.metadata()?;
    let m = tree.metadata()?;
    let marker = Seed {
        common_dev: stat.dev(),
        common_ino: stat.ino(),
        commit: oid.into(),
        context: seal,
        tree_dev: m.dev(),
        tree_ino: m.ino(),
    };
    serde_json::to_writer(File::create(seed.join("seed.json"))?, &marker)?;
    fs::rename(seed, dest)?;
    Ok(())
}

fn remove_tree(path: &Path, device: u64) -> Result<()> {
    let stat = fs::symlink_metadata(path)?;
    if !stat.is_dir() {
        fs::remove_file(path)?;
        return Ok(());
    }
    ensure!(
        stat.dev() == device,
        "refusing to cross a mount in the checkout cache"
    );
    for item in fs::read_dir(path)? {
        remove_tree(&item?.path(), device)?;
    }
    fs::remove_dir(path)?;
    Ok(())
}

fn forget(root: &Path, common: &Path, oid: &str) -> Result<()> {
    let _lock = lock(root, true)?;
    let seed = root.join(oid);
    let retiring = root.join(format!(".forget-{oid}"));
    if seed.try_exists()? {
        Seed::read(&seed, common, oid, false)?;
        ensure!(
            fs::symlink_metadata(&retiring).is_err(),
            "conflicting retiring seed"
        );
        fs::rename(seed, &retiring)?;
    }
    // A crash after unlinking the manifest can leave only the empty tombstone.
    if fs::symlink_metadata(&retiring)?.is_dir() && fs::read_dir(&retiring)?.next().is_none() {
        fs::remove_dir(&retiring)?;
        return Ok(());
    }
    let meta = Seed::read(&retiring, common, oid, true)?;
    if retiring.join("tree").try_exists()? {
        remove_tree(&retiring.join("tree"), meta.tree_dev)?;
    }
    fs::remove_file(retiring.join("seed.json"))?;
    fs::remove_dir(retiring)?;
    Ok(())
}

fn has_hooks(common: &Path, tree: &Path, dest: &Path) -> Result<bool> {
    let raw = read_path(
        git()
            .args(["config", "--type=path", "--default"])
            .arg(common.join("hooks"))
            .args(["--get", "core.hooksPath"]),
    )?;
    let folder = if raw.is_absolute() {
        let resolved = resolved_path(&raw)?;
        match resolved.strip_prefix(dest) {
            Ok(relative) => tree.join(relative),
            Err(_) => resolved,
        }
    } else {
        let resolved = match resolved_path(&tree.join(raw)) {
            Ok(v) => v,
            Err(_) => return Ok(true),
        };
        if !resolved.starts_with(tree) {
            return Ok(true);
        }
        resolved
    };
    let mut hooks: Vec<_> = [
        "post-checkout",
        "post-index-change",
        "reference-transaction",
    ]
    .iter()
    .map(|n| folder.join(n))
    .collect();
    hooks.push(read_path(git().args([
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "hooks/reference-transaction",
    ]))?);
    Ok(hooks.iter().any(|path| {
        fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink() || m.mode() & 0o111 != 0)
    }))
}

fn cached_add(
    args: &[OsString],
    index: usize,
    dest: &Path,
    root: &Path,
    common: &Path,
) -> Result<bool> {
    let Some(parent) = dest.parent() else {
        return Ok(false);
    };
    let Some(name) = dest.file_name() else {
        return Ok(false);
    };
    if !root.is_dir() || !parent.is_dir() || fs::symlink_metadata(dest).is_ok() {
        return Ok(false);
    }
    let revision = args
        .get(index + 1)
        .map_or(OsStr::new("HEAD"), |v| v.as_os_str());
    let oid = commit_id(revision)?;
    let seed = root.join(&oid);
    let _lock = lock(root, false)?;
    if !seed.is_dir() {
        return Ok(false);
    }
    let original = Seed::read(&seed, common, &oid, false)?;
    let tree = seed.join("tree");
    if context(common)?.as_ref() != Some(&original.context) || has_hooks(common, &tree, dest)? {
        return Ok(false);
    }
    let tmp = Builder::new()
        .prefix(".worktree-fast-")
        .tempdir_in(parent)?;
    let payload = tmp.path().join("payload");
    if let Err(e) = crate::apfs::clone_path(&tree, &payload) {
        if matches!(
            e.raw_os_error(),
            Some(libc::EXDEV | libc::ENOTSUP | libc::ENOSYS | libc::EINVAL)
        ) {
            return Ok(false);
        }
        return Err(e.into());
    }
    let container = tmp.path().join("register");
    fs::create_dir(&container)?;
    let staged = container.join(name);
    let mut options = args[1..].to_vec();
    options[index - 1] = staged.clone().into_os_string();
    // Only remove a branch after Git reports that we created it. Existing
    // branches, public paths and moves completed by another process survive.
    output(
        git()
            .args(["worktree", "add", "--no-checkout"])
            .args(&options),
    )?;
    let created = args[1] == "-b";
    let actual = output(git().current_dir(&staged).args(["rev-parse", "HEAD"]))?;
    let actual = std::str::from_utf8(&actual)?.trim_end();
    let publish = (|| -> Result<bool> {
        if actual != oid {
            return Ok(false);
        }
        fs::rename(staged.join(".git"), payload.join(".git"))?;
        if let Err(e) = fs::rename(&payload, &staged) {
            fs::rename(payload.join(".git"), staged.join(".git"))?;
            return Err(e.into());
        }
        output(git().current_dir(&staged).args([
            "-c",
            "submodule.recurse=false",
            "read-tree",
            "--reset",
            "HEAD",
        ]))?;
        // Always pass the existing parent. Two calls targeting the same name
        // then collide; the loser cannot nest itself under the winner.
        let moved = git()
            .args(["worktree", "move", "--"])
            .arg(&staged)
            .arg(parent)
            .output()?;
        if !moved.status.success() && staged.is_dir() && fs::symlink_metadata(dest).is_err() {
            return Ok(false);
        }
        checked(moved)?;
        Ok(true)
    })();
    if !matches!(publish, Ok(true)) && staged.exists() {
        output(
            git()
                .args(["worktree", "remove", "--force", "--"])
                .arg(&staged),
        )?;
        if created {
            let mut branch = OsString::from("refs/heads/");
            branch.push(&args[2]);
            output(git().args(["update-ref", "-d"]).arg(branch).arg(actual))?;
        }
    }
    publish
}

/// Called as a Git external subcommand. Unknown forms keep Git's native behavior.
pub fn run(args: &[OsString]) -> Result<ExitCode> {
    let Some(verb) = args.first().and_then(|v| v.to_str()) else {
        return run_git(args);
    };
    if !matches!(verb, "prepare" | "forget" | "add" | "move") {
        return run_git(args);
    }
    if !cfg!(target_os = "macos") && matches!(verb, "add" | "move") {
        return run_git(args);
    }
    let common =
        read_path(git().args(["rev-parse", "--path-format=absolute", "--git-common-dir"]))?
            .canonicalize()?;
    let root = cache_root(&common)?;
    if verb == "add" || verb == "move" {
        let namespace = root.ancestors().nth(2).unwrap();
        for arg in &args[1..] {
            ensure!(
                !resolved_path(Path::new(arg))?.starts_with(namespace)
                    && !std::path::absolute(arg)?.starts_with(namespace),
                "a worktree must be outside the tracked checkout cache"
            );
        }
        let index = if verb == "add" && (4..=5).contains(&args.len()) && args[1] == "-b" {
            Some(3)
        } else if verb == "add" && (3..=4).contains(&args.len()) && args[1] == "--detach" {
            Some(2)
        } else {
            None
        };
        if let Some(i) = index
            && !args[i..].iter().any(|a| a.as_bytes().starts_with(b"-"))
            && cached_add(
                args,
                i,
                &resolved_path(Path::new(&args[i]))?,
                &root,
                &common,
            )?
        {
            eprintln!("git-worktree-fast: created from APFS checkout cache");
            return Ok(ExitCode::SUCCESS);
        }
        return run_git(args);
    }
    ensure!(args.len() <= 2, "expected {verb} [<revision>]");
    let rev = args.get(1).map_or(OsStr::new("HEAD"), |v| v.as_os_str());
    let raw = rev.to_str().unwrap_or_default();
    let oid = if verb == "forget" && valid_oid(raw) {
        raw.to_owned()
    } else {
        commit_id(rev)?
    };
    match verb {
        "prepare" => prepare(&root, &common, &oid)?,
        "forget" => forget(&root, &common, &oid)?,
        _ => unreachable!(),
    };
    println!("{verb}: {oid}");
    Ok(ExitCode::SUCCESS)
}
