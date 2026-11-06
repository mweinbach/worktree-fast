//! A cache of tracked checkout bytes. Git owns every published worktree and index.
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
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

pub(crate) fn valid_oid(id: &str) -> bool {
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

pub(crate) fn cache_root(common: &Path) -> Result<PathBuf> {
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

// Preserve user-selected index formats and stat policies that differ from
// the template. Native Git handles them, including its shared-repo permissions.
fn can_prime_index() -> Result<bool> {
    if env::var_os("GIT_INDEX_VERSION").is_some() {
        return Ok(false);
    }
    let config = output(git().args(["config", "--null", "--list"]))?;
    for record in config.split(|&v| v == 0) {
        let mut parts = record.splitn(2, |&v| v == b'\n');
        let key = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or(b"true").to_ascii_lowercase();
        let yes = matches!(value.as_slice(), b"true" | b"yes" | b"on" | b"1");
        let no = matches!(value.as_slice(), b"false" | b"no" | b"off" | b"0" | b"");
        let allowed = match key {
            b"core.trustctime" => yes,
            b"core.checkstat" => value == b"default",
            b"index.version" => value == b"2",
            b"core.sharedrepository" => no || value == b"umask",
            b"core.splitindex" | b"core.ignorestat" | b"feature.manyfiles" | b"index.skiphash" => {
                no
            }
            _ => true,
        };
        if !allowed {
            return Ok(false);
        }
    }
    Ok(true)
}

// These cannot change checkout bytes or the seed's index. Hook and index-stat
// settings are checked again on every add. Unlisted core settings stay sealed.
const OUTSIDE_CHECKOUT: &[&[u8]] = &[
    b"core.abbrev",
    b"core.askpass",
    b"core.bigfilethreshold",
    b"core.commentchar",
    b"core.commentstring",
    b"core.commitgraph",
    b"core.compression",
    b"core.deltabasecachelimit",
    b"core.editor",
    b"core.excludesfile",
    b"core.filesreflocktimeout",
    b"core.fsync",
    b"core.fsyncmethod",
    b"core.fsyncobjectfiles",
    b"core.gitproxy",
    b"core.hookspath",
    b"core.logallrefupdates",
    b"core.loosecompression",
    b"core.multipackindex",
    b"core.notesref",
    b"core.packedgitlimit",
    b"core.packedgitwindowsize",
    b"core.packedrefstimeout",
    b"core.pager",
    b"core.preloadindex",
    b"core.quotepath",
    b"core.sshcommand",
    b"core.untrackedcache",
    b"core.warnambiguousrefs",
    b"core.whitespace",
];

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
                b"core.repositoryformatversion"
                    | b"extensions.relativeworktrees"
                    | b"checkout.workers"
                    | b"checkout.thresholdforparallelism"
            )
            && !OUTSIDE_CHECKOUT.contains(&key)
        {
            hash.update(record);
            hash.update([0]);
        }
    }
    Ok(Some(hex::encode(hash.finalize())))
}

#[derive(Serialize, Deserialize)]
struct Priming {
    tree_seal: String,
    index_sha256: String,
}

#[derive(Serialize, Deserialize)]
struct Seed {
    // Entries written by older releases were all explicitly prepared.
    #[serde(default)]
    auto_only: bool,
    #[serde(default)]
    priming: Option<Priming>,
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
                name == "seed.json"
                    || name == "tree"
                    || (seed.priming.is_some() && name == "index"),
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

fn seed_git(common: &Path, seed: &Path, args: &[&str]) -> Result<()> {
    output(
        git()
            .current_dir(seed.join("tree"))
            .env("GIT_INDEX_FILE", seed.join("index"))
            .env("GIT_INDEX_VERSION", "2")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.splitIndex=false",
                "-c",
                "index.version=2",
                "-c",
                "index.skipHash=false",
                "-c",
                "index.threads=1",
                "-c",
                "core.untrackedCache=false",
                "-c",
                "core.fsmonitor=false",
            ])
            .arg("--git-dir")
            .arg(common)
            .arg("--work-tree")
            .arg(seed.join("tree"))
            .args(args),
    )?;
    Ok(())
}

fn git_paths(oid: &str) -> Result<Vec<u8>> {
    output(git().args(["ls-tree", "--full-tree", "-rtz", "--name-only", oid]))
}

fn prepare_from_parent(
    root: &Path,
    common: &Path,
    oid: &str,
    context: &str,
    seed: &Path,
) -> Result<Option<String>> {
    let Ok(parent) = commit_id(OsStr::new(&format!("{oid}^1"))) else {
        return Ok(None);
    };
    let source = root.join(&parent);
    if !source.is_dir() {
        return Ok(None);
    }
    let original = Seed::read(&source, common, &parent, false)?;
    let Some(priming) = &original.priming else {
        return Ok(None);
    };
    if original.context != context {
        return Ok(None);
    }
    // Attribute edits can change the checkout bytes of otherwise identical blobs.
    let changed = output(git().args([
        "diff-tree",
        "--no-commit-id",
        "--no-renames",
        "--no-relative",
        "--ignore-submodules=none",
        "--name-only",
        "-r",
        "-z",
        &parent,
        oid,
        "--",
    ]))?;
    if changed.split(|&c| c == 0).any(|p| {
        p.rsplit(|&c| c == b'/')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(b".gitattributes"))
    }) {
        return Ok(None);
    }
    let index = read_template(&source, priming).context("invalid parent index")?;
    DirBuilder::new().mode(0o700).create(seed)?;
    let tree = seed.join("tree");
    let parent_tree = source.join("tree");
    crate::apfs::clone_seed(&parent_tree, &tree, original.tree_dev, original.tree_ino)?;
    // Authenticate the source after cloning. Never adopt extra files or a
    // filesystem's collapsed spelling from an otherwise valid cold checkout.
    let actual =
        crate::checkout_proof::fingerprint_paths(&parent_tree, Some(&git_paths(&parent)?))?;
    ensure!(actual == priming.tree_seal, "parent checkout cache changed");
    fs::write(seed.join("index"), index)?;
    seed_git(
        common,
        seed,
        &[
            "read-tree",
            "--reset",
            "-u",
            "--no-sparse-checkout",
            "--no-recurse-submodules",
            &parent,
            oid,
        ],
    )?;
    let seal = crate::checkout_proof::fingerprint_paths(&tree, Some(&git_paths(oid)?))?;
    // Keep the usual fresh Git template, independent of the temporary merge index.
    fs::remove_file(seed.join("index"))?;
    seed_git(
        common,
        seed,
        &["read-tree", "--reset", "--no-sparse-checkout", oid],
    )?;
    Ok(Some(seal))
}

fn prepare(root: &Path, common: &Path, oid: &str, automatic: bool) -> Result<()> {
    let seal = context(common)?
        .context("this Git environment does not support prepared APFS checkouts")?;
    DirBuilder::new().recursive(true).mode(0o700).create(root)?;
    let _lock = lock(root, true)?;
    let dest = root.join(oid);
    ensure!(
        !root.join(format!(".forget-{oid}")).try_exists()?,
        "interrupted cleanup; run forget again"
    );
    let replacing = dest.try_exists()?;
    let mut auto_only = automatic;
    if replacing {
        let mut previous = Seed::read(&dest, common, oid, false)?;
        auto_only &= previous.auto_only;
        if previous.context == seal
            && let Some(priming) = &previous.priming
            && crate::checkout_proof::fingerprint(&dest.join("tree"))
                .ok()
                .as_ref()
                == Some(&priming.tree_seal)
            && read_template(&dest, priming).is_some()
        {
            if previous.auto_only && !automatic {
                previous.auto_only = false;
                let marker = tempfile::NamedTempFile::new_in(root)?;
                serde_json::to_writer(marker.as_file(), &previous)?;
                marker.persist(dest.join("seed.json"))?;
            }
            return Ok(());
        }
        // Never bless the damaged entry's existing bytes.
    }
    let tmp = Builder::new().prefix(".prepare-").tempdir_in(root)?;
    check_filters(common, oid)?;
    let attempt = tmp.path().join("incremental");
    let incremental = prepare_from_parent(root, common, oid, &seal, &attempt);
    let (seed, tree_seal) = if let Ok(Some(tree_seal)) = incremental {
        if env::var_os("WORKTREE_FAST_TRACE").as_deref() == Some(OsStr::new("1")) {
            let _ = writeln!(
                io::stderr(),
                "git-worktree-fast: prepared from first-parent APFS checkout cache"
            );
        }
        (attempt, tree_seal)
    } else {
        if let Err(error) = incremental
            && env::var_os("WORKTREE_FAST_TRACE").as_deref() == Some(OsStr::new("1"))
        {
            let _ = writeln!(
                io::stderr(),
                "git-worktree-fast: preparing from Git objects: {error:#}"
            );
        }
        if attempt.exists() {
            fs::remove_dir_all(&attempt)?;
        }
        let seed = tmp.path().join("full");
        DirBuilder::new().mode(0o700).create(&seed)?;
        let tree = seed.join("tree");
        fs::create_dir(&tree)?;
        seed_git(
            common,
            &seed,
            &["read-tree", "--reset", "--no-sparse-checkout", oid],
        )?;
        seed_git(common, &seed, &["checkout-index", "--all"])?;
        (seed, crate::checkout_proof::fingerprint(&tree)?)
    };
    let stat = common.metadata()?;
    let m = seed.join("tree").metadata()?;
    let template = seed.join("index");
    let priming = Priming {
        tree_seal,
        index_sha256: hex::encode(Sha256::digest(fs::read(&template)?)),
    };
    let marker = Seed {
        auto_only,
        priming: Some(priming),
        common_dev: stat.dev(),
        common_ino: stat.ino(),
        commit: oid.into(),
        context: seal,
        tree_dev: m.dev(),
        tree_ino: m.ino(),
    };
    serde_json::to_writer(File::create(seed.join("seed.json"))?, &marker)?;
    if replacing {
        forget_locked(root, common, oid)?;
    }
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
    forget_locked(root, common, oid)
}

pub(crate) fn prepare_automatic(common: &Path, oid: &str) -> Result<()> {
    ensure!(valid_oid(oid), "invalid automatic preparation commit");
    prepare(&cache_root(common)?, common, oid, true)
}

pub(crate) fn forget_automatic(common: &Path, oid: &str) -> Result<()> {
    ensure!(valid_oid(oid), "invalid automatic preparation commit");
    let root = cache_root(common)?;
    if root.is_dir() {
        let _lock = lock(&root, true)?;
        let seed = root.join(oid);
        let retiring = root.join(format!(".forget-{oid}"));
        let auto_only = if seed.try_exists()? {
            Seed::read(&seed, common, oid, false)?.auto_only
        } else if retiring.try_exists()? {
            // A previous retirement can stop after the initial rename, or
            // after deleting its manifest but before removing the empty dir.
            fs::symlink_metadata(&retiring)?.is_dir()
                && (fs::read_dir(&retiring)?.next().is_none()
                    || Seed::read(&retiring, common, oid, true)?.auto_only)
        } else {
            false
        };
        if auto_only {
            forget_locked(&root, common, oid)?;
        }
    }
    Ok(())
}

fn forget_locked(root: &Path, common: &Path, oid: &str) -> Result<()> {
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
    if meta.priming.is_some() && fs::symlink_metadata(retiring.join("index")).is_ok() {
        fs::remove_file(retiring.join("index"))?;
    }
    fs::remove_file(retiring.join("seed.json"))?;
    fs::remove_dir(retiring)?;
    Ok(())
}

fn read_template(seed: &Path, priming: &Priming) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(seed.join("index"))
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    (hex::encode(Sha256::digest(&bytes)) == priming.index_sha256).then_some(bytes)
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

// Parse only forms that preserve their meaning with --no-checkout and a staging
// path of the same basename. Git handles the full option grammar on a miss.
struct AddForm<'a> {
    path: usize,
    branch: Option<&'a OsStr>,
    detach: bool,
    quiet: bool,
}

fn add_form(args: &[OsString]) -> Option<AddForm<'_>> {
    if args.first()? != "add" {
        return None;
    }
    let mut form = AddForm {
        path: 1,
        branch: None,
        detach: false,
        quiet: false,
    };
    while let Some(value) = args.get(form.path) {
        match value.as_bytes() {
            b"--" => {
                form.path += 1;
                break;
            }
            b"-d" | b"--detach" if form.branch.is_none() => form.detach = true,
            b"-b" if !form.detach && form.branch.is_none() => {
                form.path += 1;
                form.branch = Some(args.get(form.path)?.as_os_str());
            }
            b"-q" | b"--quiet" => form.quiet = true,
            b"--track" | b"--no-track" | b"--relative-paths" | b"--no-relative-paths" => (),
            flag if flag.starts_with(b"-") => return None,
            _ => break,
        }
        form.path += 1;
    }
    (matches!(args.len() - form.path, 1 | 2)
        && !args
            .get(form.path + 1)
            .is_some_and(|r| r.as_bytes().starts_with(b"-")))
    .then_some(form)
}

struct Candidate<'a> {
    oid: String,
    // Passing an explicit -b for an implicit *new* branch proves ownership
    // even if another worktree races to create the same branch.
    implicit_new: Option<&'a OsStr>,
}

fn candidate<'a>(args: &[OsString], form: &AddForm<'_>, name: &'a OsStr) -> Option<Candidate<'a>> {
    if let Some(rev) = args.get(form.path + 1) {
        return Some(Candidate {
            oid: commit_id(rev).ok()?,
            implicit_new: None,
        });
    }
    if form.branch.is_some() || form.detach {
        return Some(Candidate {
            oid: commit_id(OsStr::new("HEAD")).ok()?,
            implicit_new: None,
        });
    }
    let mut local = OsString::from("refs/heads/");
    local.push(name);
    if let Ok(oid) = commit_id(&local) {
        return Some(Candidate {
            oid,
            implicit_new: None,
        });
    }

    // Git may set up a tracking branch instead of branching from HEAD.
    // Delegate remote guessing (including checkout.defaultRemote) to Git.
    let guess = output(git().args([
        "config",
        "--type=bool",
        "--default=false",
        "--get",
        "worktree.guessRemote",
    ]))
    .ok()?;
    if guess != b"false\n" {
        return None;
    }
    Some(Candidate {
        oid: commit_id(OsStr::new("HEAD")).ok()?,
        implicit_new: Some(name),
    })
}

fn relay(out: &Output) -> io::Result<()> {
    io::stderr().write_all(&out.stderr)?;
    io::stdout().write_all(&out.stdout)
}

fn cached_add(
    args: &[OsString],
    form: &AddForm<'_>,
    dest: &Path,
    root: &Path,
    common: &Path,
) -> Result<Option<ExitCode>> {
    let Some(parent) = dest.parent() else {
        return Ok(None);
    };
    let Some(name) = dest.file_name() else {
        return Ok(None);
    };
    if !root.is_dir() || fs::symlink_metadata(dest).is_ok() {
        return Ok(None);
    }
    let Some(Candidate { oid, implicit_new }) = candidate(args, form, name) else {
        return Ok(None);
    };
    let seed = root.join(&oid);
    let _lock = lock(root, false)?;
    if !seed.is_dir() {
        return Ok(None);
    }
    let Ok(original) = Seed::read(&seed, common, &oid, false) else {
        return Ok(None);
    };
    let tree = seed.join("tree");
    let Some(priming) = &original.priming else {
        return Ok(None);
    };
    if context(common)?.as_ref() != Some(&original.context)
        || !can_prime_index()?
        || has_hooks(common, &tree, dest)?
    {
        return Ok(None);
    }
    // Stock Git creates missing leading directories; a failure keeps its diagnostics.
    if !parent.is_dir() && fs::create_dir_all(parent).is_err() {
        return Ok(None);
    }
    let tmp = Builder::new()
        .prefix(".worktree-fast-")
        .tempdir_in(parent)?;
    let payload = tmp.path().join("payload");
    if let Err(e) = crate::apfs::clone_seed(&tree, &payload, original.tree_dev, original.tree_ino) {
        if e.kind() == io::ErrorKind::InvalidData
            || matches!(
                e.raw_os_error(),
                Some(libc::EXDEV | libc::ENOTSUP | libc::ENOSYS | libc::EINVAL)
            )
        {
            return Ok(None);
        }
        return Err(e.into());
    }
    // The syscall clones the identity-checked source directory. Validate its
    // complete inventory *after* the clone, before touching a branch or index.
    if crate::checkout_proof::fingerprint(&tree).ok().as_ref() != Some(&priming.tree_seal) {
        return Ok(None);
    }
    let Some(template) = read_template(&seed, priming) else {
        return Ok(None);
    };
    // A folded name collision or unsupported entry stays entirely native.
    let Ok(index_data) = crate::stat_index::build(&template, &payload, oid.len() / 2) else {
        return Ok(None);
    };
    let container = tmp.path().join("register");
    fs::create_dir(&container)?;
    let staged = container.join(name);
    let mut options = args[1..].to_vec();
    options[form.path - 1] = staged.clone().into_os_string();
    // Only remove a branch after Git reports that we created it. Existing
    // branches, public paths and moves completed by another process survive.
    let mut register = git();
    register.args(["worktree", "add", "--no-checkout"]);
    if let Some(branch) = implicit_new {
        register.arg("-b").arg(branch);
    }
    let registration = register.args(&options).output()?;
    if !registration.status.success() {
        if implicit_new.is_some() {
            // An implicit branch may have appeared since the probe. Replay the
            // original form at the real path, with native diagnostics.
            return Ok(None);
        }
        relay(&registration)?;
        use std::os::unix::process::ExitStatusExt;
        let code = registration
            .status
            .code()
            .unwrap_or_else(|| 128 + registration.status.signal().unwrap_or(1));
        return Ok(Some(ExitCode::from(code as u8)));
    }
    let mut branch_tip = oid;
    let publish = (|| -> Result<Option<Output>> {
        let raw = output(git().current_dir(&staged).args(["rev-parse", "HEAD"]))?;
        let actual = std::str::from_utf8(&raw)?.trim_end();
        if actual != branch_tip {
            branch_tip = actual.to_owned();
            return Ok(None);
        }
        fs::rename(staged.join(".git"), payload.join(".git"))?;
        if let Err(e) = fs::rename(&payload, &staged) {
            fs::rename(payload.join(".git"), staged.join(".git"))?;
            return Err(e.into());
        }
        let index = read_path(git().current_dir(&staged).args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index",
        ]))?;
        crate::stat_index::install(&index_data, &index)?;
        // Stock worktree add follows registration with this reset. Its own
        // output respects locale, quiet, tracking, abbreviations and empty
        // commit messages, and it initializes ORIG_HEAD and the reset reflog.
        // The verified index already describes these files at their real stats.
        let mut reset = git();
        reset
            .current_dir(&staged)
            .args(["reset", "--hard", "--no-recurse-submodules"]);
        if form.quiet {
            reset.arg("--quiet");
        }
        let reset = reset.output()?;
        ensure!(
            reset.status.success(),
            "git reset: {}",
            String::from_utf8_lossy(&reset.stderr).trim_end()
        );
        // Always pass the existing parent. Two calls targeting the same name
        // then collide; the loser cannot nest itself under the winner.
        let moved = git()
            .args(["worktree", "move", "--"])
            .arg(&staged)
            .arg(parent)
            .output()?;
        if !moved.status.success() && staged.is_dir() && fs::symlink_metadata(dest).is_err() {
            return Ok(None);
        }
        checked(moved)?;
        Ok(Some(reset))
    })();
    if !matches!(publish, Ok(Some(_))) {
        // Keep the private tempdir reserved, but vacate the registered path so
        // Git only unlinks its metadata, even if the staged .git file was lost.
        fs::rename(&container, tmp.path().join("discard"))?;
        output(git().args(["worktree", "remove", "--"]).arg(&staged))?;
        if let Some(created) = form.branch.or(implicit_new) {
            let mut branch = OsString::from("refs/heads/");
            branch.push(created);
            output(
                git()
                    .args(["update-ref", "-d"])
                    .arg(branch)
                    .arg(&branch_tip),
            )?;
        }
    }
    let Some(reset) = publish? else {
        return Ok(None);
    };
    // Publication is complete. A consumer closing its output pipe must not
    // turn the successful add into an apparent error or trigger another add.
    let _ = relay(&registration);
    let _ = relay(&reset);
    if !form.quiet && env::var_os("WORKTREE_FAST_TRACE").as_deref() == Some(OsStr::new("1")) {
        let _ = writeln!(
            io::stderr(),
            "git-worktree-fast: created from APFS checkout cache"
        );
    }
    Ok(Some(ExitCode::SUCCESS))
}

/// Called as a Git external subcommand. Unknown forms keep Git's native behavior.
pub fn run(args: &[OsString]) -> Result<ExitCode> {
    let Some(verb) = args.first().and_then(|v| v.to_str()) else {
        return run_git(args);
    };
    if !matches!(verb, "prepare" | "forget" | "add" | "move") {
        return run_git(args);
    }
    let native_verb = matches!(verb, "add" | "move");
    if native_verb && !cfg!(target_os = "macos") {
        return run_git(args);
    }
    let common =
        match read_path(git().args(["rev-parse", "--path-format=absolute", "--git-common-dir"])) {
            Ok(path) => path.canonicalize()?,
            Err(_) if native_verb => return run_git(args),
            Err(err) => return Err(err),
        };
    let root = cache_root(&common)?;
    if native_verb {
        let namespace = root.ancestors().nth(2).unwrap();
        let form = add_form(args);
        // Only inspect the destination on understood forms: a revision can
        // legally have the same name as a broken symlink in the source tree.
        // On passthrough forms any positional argument might be the destination.
        let possible_paths = form.as_ref().map_or(&args[1..], |f| &args[f.path..=f.path]);
        let ns = fs::metadata(namespace).ok();
        for arg in possible_paths {
            let path = resolved_path(Path::new(arg))?;
            // APFS can be case-insensitive. Compare existing ancestor inodes
            // as well, so alternate spellings cannot put a checkout in a seed.
            let aliases_cache = ns.as_ref().is_some_and(|ns| {
                path.ancestors().any(|p| {
                    fs::metadata(p).is_ok_and(|m| m.dev() == ns.dev() && m.ino() == ns.ino())
                })
            });
            ensure!(
                !path.starts_with(namespace)
                    && !std::path::absolute(arg)?.starts_with(namespace)
                    && !aliases_cache,
                "a worktree must be outside the tracked checkout cache"
            );
        }
        if let Some(form) = form
            && let Some(code) = cached_add(
                args,
                &form,
                &resolved_path(Path::new(&args[form.path]))?,
                &root,
                &common,
            )?
        {
            return Ok(code);
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
        "prepare" => prepare(&root, &common, &oid, false)?,
        "forget" => forget(&root, &common, &oid)?,
        _ => unreachable!(),
    };
    println!("{verb}: {oid}");
    Ok(ExitCode::SUCCESS)
}
