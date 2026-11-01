//! Optional, repository-local post-commit preparation. Hooks enqueue exact commits.
//! One detached worker per cache publishes them and trims the retained history.
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::num::NonZeroUsize;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use tempfile::NamedTempFile;

const USAGE: &str = "git worktree-fast auto enable [--keep N] [--existing-hook] | keep N | disable | status | notify";
const EXTERNAL: &str = "keep your hook and add `git worktree-fast auto notify` to its post-commit action, then run `git worktree-fast auto enable --existing-hook`";

fn git(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git").args(args).output()?;
    ensure!(
        out.status.success(),
        "git: {}",
        String::from_utf8_lossy(&out.stderr).trim_end()
    );
    Ok(out.stdout)
}

fn line(mut raw: Vec<u8>) -> OsString {
    if raw.last() == Some(&b'\n') {
        raw.pop();
    }
    OsString::from_vec(raw)
}

fn private_dir(path: &Path, create: bool) -> Result<File> {
    if create {
        DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let stat = file.metadata()?;
    ensure!(
        stat.uid() == unsafe { libc::geteuid() } && stat.mode() & 0o077 == 0,
        "expected a private, user-owned directory: {}",
        path.display()
    );
    Ok(file)
}

fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::other("expected regular file"));
    }
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match read_file(path) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut tmp = NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(tmp.as_file_mut(), value)?;
    tmp.persist(path)?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Registration {
    generation: String,
    hook_digest: Option<String>,
    #[serde(default = "default_keep")]
    keep: NonZeroUsize,
}

fn default_keep() -> NonZeroUsize {
    NonZeroUsize::new(3).unwrap()
}

fn new_registration(state: &Path, keep: NonZeroUsize) -> Result<Registration> {
    let generation = NamedTempFile::new_in(state)?;
    Ok(Registration {
        generation: generation
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        hook_digest: None,
        keep,
    })
}

fn saved_keep(state: &Path) -> Result<NonZeroUsize> {
    Ok(read_json::<Registration>(&state.join("disabled"))?.map_or_else(default_keep, |r| r.keep))
}

fn hook_script() -> Result<Vec<u8>> {
    let mut script = b"#!/bin/sh\n# Installed by git-worktree-fast auto enable\nexec '".to_vec();
    for &b in std::env::current_exe()?.as_os_str().as_bytes() {
        if b == b'\'' {
            script.extend_from_slice(b"'\\''");
        } else {
            script.push(b);
        }
    }
    script.extend_from_slice(b"' auto notify\n");
    Ok(script)
}

fn default_hooks(common: &Path) -> Result<PathBuf> {
    let hooks = common.join("hooks");
    match fs::symlink_metadata(&hooks) {
        Ok(meta) => ensure!(meta.is_dir(), "indirect hooks directory; {EXTERNAL}"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&hooks)?,
        Err(e) => return Err(e.into()),
    }
    Ok(hooks)
}

fn enable(common: &Path, state: &Path, external: bool, keep: Option<NonZeroUsize>) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "automatic APFS preparation requires macOS"
    );
    let hooks = Command::new("git")
        .args(["config", "--name-only", "--get-regexp", "^hook\\."])
        .output()?;
    ensure!(
        !hooks.status.success(),
        "Git hook.* configuration is unsupported by the checkout cache; automatic preparation requires file-based hooks"
    );
    ensure!(
        hooks.status.code() == Some(1),
        "could not inspect Git hook configuration"
    );
    let control = private_dir(state, true)?;
    FileExt::lock_exclusive(&control)?;
    let path = state.join("enabled");
    let (mut reg, external) = match read_json::<Registration>(&path)? {
        Some(active) => {
            let using_external_hook = active.hook_digest.is_none();
            ensure!(
                !external || using_external_hook,
                "already enabled in another hook mode; disable first"
            );
            (active, using_external_hook)
        }
        None => (new_registration(state, saved_keep(state)?)?, external),
    };
    if let Some(keep) = keep {
        reg.keep = keep;
    }
    if external {
        save_json(&path, &reg)?;
    } else {
        install_hook(common, &path, &mut reg)?;
    }
    apply_retention(common, &reg)?;
    println!("Automatic preparation enabled for future commits on local branches.");
    Ok(())
}

fn install_hook(common: &Path, path: &Path, reg: &mut Registration) -> Result<()> {
    let conf = Command::new("git")
        .args(["config", "--get", "core.hooksPath"])
        .output()?;
    ensure!(!conf.status.success(), "custom core.hooksPath; {EXTERNAL}");
    ensure!(
        conf.status.code() == Some(1),
        "could not inspect core.hooksPath"
    );
    let hooks = default_hooks(common)?;
    let hook = hooks.join("post-commit");
    match fs::symlink_metadata(&hook) {
        Ok(_) => {
            ensure!(
                reg.hook_digest
                    .as_ref()
                    .is_some_and(|d| owned_hook(&hook, d)),
                "post-commit hook already exists or was modified; {EXTERNAL}"
            );
            return save_json(path, reg);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    let script = hook_script()?;
    let mut tmp = NamedTempFile::new_in(&hooks)?;
    tmp.write_all(&script)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o755))?;
    // Keep ownership information before publication. Repeating enable repairs an
    // interruption between this write and the exclusive installation of the hook.
    reg.hook_digest = Some(hex::encode(Sha256::digest(&script)));
    save_json(path, reg)?;
    if let Err(e) = tmp.persist_noclobber(&hook) {
        fs::remove_file(path)?;
        return Err(e).with_context(|| format!("could not install post-commit hook; {EXTERNAL}"));
    }
    Ok(())
}

fn owned_hook(path: &Path, digest: &str) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.mode() & 0o7777 == 0o755)
        && read_file(path).is_ok_and(|bytes| hex::encode(Sha256::digest(bytes)) == digest)
}

fn disable(common: &Path, state: &Path) -> Result<()> {
    if !state.try_exists()? {
        return Ok(());
    }
    let control = private_dir(state, false)?;
    FileExt::lock_exclusive(&control)?;
    let path = state.join("enabled");
    if let Some(active) = read_json::<Registration>(&path)? {
        if let Some(digest) = active.hook_digest {
            let hooks = common.join("hooks");
            let hook = hooks.join("post-commit");
            if fs::symlink_metadata(&hooks).is_ok_and(|m| m.is_dir()) && owned_hook(&hook, &digest)
            {
                fs::remove_file(hook)?;
            }
        }
        // Release the opt-in only after hook removal; an interrupted disable can retry.
        fs::rename(path, state.join("disabled"))?;
    }
    println!("Automatic preparation disabled.");
    Ok(())
}

fn set_keep(common: &Path, state: &Path, keep: NonZeroUsize) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "automatic APFS preparation requires macOS"
    );
    let control = private_dir(state, true)?;
    FileExt::lock_exclusive(&control)?;
    let enabled = state.join("enabled");
    let path = if enabled.try_exists()? {
        enabled
    } else {
        state.join("disabled")
    };
    let mut reg = match read_json::<Registration>(&path)? {
        Some(reg) => reg,
        None => new_registration(state, keep)?,
    };
    reg.keep = keep;
    save_json(&path, &reg)?;
    apply_retention(common, &reg)?;
    println!("Automatic cache retention limit set to {keep} per branch.");
    Ok(())
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Commit {
    // Hex preserves arbitrary Git ref bytes without using them as filesystem paths.
    branch: String,
    oid: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Queue {
    generation: String,
    // Both lists are ordered oldest first. Git timestamps need not be monotonic.
    pending: Vec<Commit>,
    recent: Vec<Commit>,
    retiring: BTreeSet<String>,
    last_error: Option<String>,
}

#[derive(Deserialize)]
struct BranchQueue {
    generation: String,
    pending: BTreeSet<String>,
    pins: BTreeMap<String, String>,
    retiring: BTreeSet<String>,
    last_error: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SavedQueue {
    Ordered(Queue),
    Branches(BranchQueue),
}

fn load_queue(dir: &Path, reg: &Registration) -> Result<Queue> {
    let mut q = match read_json(&dir.join("queue.json"))? {
        None => Queue::default(),
        Some(SavedQueue::Ordered(q)) => q,
        Some(SavedQueue::Branches(old)) => {
            // Old queues knew each branch's last success, but not their order.
            // Use the publication times of the surviving manifests for this upgrade.
            let cache = dir.parent().context("missing cache directory")?;
            let heads = local_heads()?;
            // The old worker also kept superseded pins while another branch pointed
            // at them. Those SHAs could still be in its retirement journal.
            let shared_tips = heads
                .iter()
                .filter(|(_, oid)| old.retiring.contains(*oid))
                .map(|(branch, oid)| (branch.clone(), oid.clone()));
            let mut entries: Vec<_> = old
                .pins
                .into_iter()
                .chain(shared_tips)
                .filter_map(|(branch, oid)| {
                    if !crate::checkout::valid_oid(&oid) {
                        return None;
                    }
                    let time = cache
                        .join(&oid)
                        .join("seed.json")
                        .symlink_metadata()
                        .ok()?
                        .modified()
                        .ok()?;
                    Some((time, Commit { branch, oid }))
                })
                .collect();
            entries.sort_by_key(|(time, _)| *time);
            let mut q = Queue {
                generation: old.generation,
                retiring: old.retiring,
                last_error: old.last_error,
                ..Queue::default()
            };
            for (_, entry) in entries {
                remember(&mut q.recent, entry);
            }
            if q.generation == reg.generation && !old.pending.is_empty() {
                for branch in old.pending {
                    if let Some(oid) = heads.get(&branch) {
                        remember(
                            &mut q.pending,
                            Commit {
                                branch,
                                oid: oid.clone(),
                            },
                        );
                    }
                }
            }
            q
        }
    };
    if q.generation != reg.generation {
        q.pending.clear();
        q.generation.clone_from(&reg.generation);
    }
    Ok(q)
}

fn remember(list: &mut Vec<Commit>, commit: Commit) {
    list.retain(|entry| entry != &commit);
    list.push(commit);
}

/// Keep each branch's newest entries, preserving the original processing order.
fn trim_per_branch(entries: &mut Vec<Commit>, keep: usize) -> Vec<Commit> {
    let mut counts = BTreeMap::<String, usize>::new();
    entries.reverse();
    let expired = entries
        .extract_if(.., |entry| {
            let count = counts.entry(entry.branch.clone()).or_default();
            *count += 1;
            *count > keep
        })
        .collect();
    entries.reverse();
    expired
}

impl Queue {
    fn trim(&mut self, keep: usize) {
        trim_per_branch(&mut self.pending, keep);
        self.retiring.extend(
            trim_per_branch(&mut self.recent, keep)
                .into_iter()
                .map(|c| c.oid),
        );
        // One physical seed can be retained by more than one branch.
        for entry in &self.recent {
            self.retiring.remove(&entry.oid);
        }
    }

    fn reconcile(&mut self, keep: usize) -> Result<()> {
        let heads = local_heads()?;
        self.pending
            .retain(|entry| heads.contains_key(&entry.branch));
        self.retiring.extend(
            self.recent
                .extract_if(.., |entry| !heads.contains_key(&entry.branch))
                .map(|entry| entry.oid),
        );
        self.trim(keep);
        Ok(())
    }
}

fn worker_lock(dir: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("worker.lock"))?)
}

fn try_lock(file: &File) -> io::Result<bool> {
    match FileExt::try_lock_exclusive(file) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e),
    }
}

fn notify(common: &Path, state: &Path) -> Result<()> {
    if !state.join("enabled").try_exists()? {
        return Ok(());
    }
    // Snapshot what triggered this hook before waiting for either lock. A
    // second commit can advance this same branch while the first hook waits.
    let out = Command::new("git")
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output()?;
    if out.status.code() == Some(1) {
        return Ok(());
    } // Detached HEAD.
    ensure!(out.status.success(), "could not identify committed branch");
    let branch = line(out.stdout);
    if !branch.as_bytes().starts_with(b"refs/heads/") {
        return Ok(());
    }

    let out = Command::new("git")
        .args(["rev-parse", "--verify", "--end-of-options"])
        .arg(&branch)
        .output()?;
    ensure!(out.status.success(), "could not resolve committed branch");
    let oid = String::from_utf8(out.stdout)?.trim_end().to_owned();
    ensure!(crate::checkout::valid_oid(&oid), "invalid commit from Git");
    let Some((_control, reg)) = registration(state)? else {
        return Ok(());
    };
    let dir = crate::checkout::cache_root(common)?.join("automatic");
    let lock = private_dir(&dir, true)?;
    FileExt::lock_exclusive(&lock)?;
    let mut q = load_queue(&dir, &reg)?;
    remember(
        &mut q.pending,
        Commit {
            branch: hex::encode(branch.as_bytes()),
            oid,
        },
    );
    q.trim(reg.keep.get());
    save_json(&dir.join("queue.json"), &q)?;
    // Probing while holding the queue lock pairs with the worker's locked exit.
    // A second notify during process startup can launch a short-lived contender.
    let probe = worker_lock(&dir)?;
    if try_lock(&probe)? {
        drop(probe);
        let mut worker = Command::new(std::env::current_exe()?);
        worker
            .current_dir(common)
            .args(["auto", "--worker", &reg.generation])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for key in git(&["rev-parse", "--local-env-vars"])?
            .split(|&b| b == b'\n')
            .filter(|k| !k.is_empty())
        {
            worker.env_remove(OsStr::from_bytes(key));
        }
        // setsid is async-signal-safe and disconnects the worker from Git's terminal.
        unsafe {
            worker.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        worker.spawn()?;
    }
    Ok(())
}

fn local_heads() -> Result<BTreeMap<String, String>> {
    let mut heads = BTreeMap::new();
    for row in git(&[
        "for-each-ref",
        "--format=%(objectname) %(refname)",
        "refs/heads/",
    ])?
    .split(|&b| b == b'\n')
    .filter(|r| !r.is_empty())
    {
        let pos = row
            .iter()
            .position(|&b| b == b' ')
            .context("invalid Git ref")?;
        heads.insert(
            hex::encode(&row[pos + 1..]),
            std::str::from_utf8(&row[..pos])?.to_owned(),
        );
    }
    Ok(heads)
}

// The worker lock (or an exclusive registration lock during reconfiguration)
// serializes retirement. Commit hooks can enqueue while files are being removed.
fn retire(
    common: &Path,
    dir: &Path,
    reg: &Registration,
    oids: BTreeSet<String>,
) -> Result<Option<String>> {
    if oids.is_empty() {
        return Ok(None);
    }
    let mut removed = Vec::new();
    let mut error = None;
    for oid in oids {
        match crate::checkout::forget_automatic(common, &oid) {
            Ok(()) => removed.push(oid),
            Err(e) => error = Some(format!("retiring {oid}: {e:#}")),
        }
    }
    let mutex = private_dir(dir, false)?;
    FileExt::lock_exclusive(&mutex)?;
    let mut q = load_queue(dir, reg)?;
    for oid in removed {
        q.retiring.remove(&oid);
    }
    if let Some(message) = &error {
        q.last_error = Some(message.clone());
    }
    save_json(&dir.join("queue.json"), &q)?;
    Ok(error)
}

fn apply_retention(common: &Path, reg: &Registration) -> Result<()> {
    let dir = crate::checkout::cache_root(common)?.join("automatic");
    if !dir.try_exists()? {
        return Ok(());
    }
    let mutex = private_dir(&dir, false)?;
    FileExt::lock_exclusive(&mutex)?;
    let mut q = load_queue(&dir, reg)?;
    q.reconcile(reg.keep.get())?;
    save_json(&dir.join("queue.json"), &q)?;
    drop(mutex);
    if let Some(error) = retire(common, &dir, reg, q.retiring)? {
        bail!("{error}");
    }
    Ok(())
}

fn drain(common: &Path, dir: &Path, reg: &Registration) -> Result<()> {
    let queue_file = dir.join("queue.json");
    let runner = worker_lock(dir)?;
    if !try_lock(&runner)? {
        return Ok(());
    }
    loop {
        let mutex = private_dir(dir, false)?;
        FileExt::lock_exclusive(&mutex)?;
        let mut q = load_queue(dir, reg)?;
        let Some(commit) = q.pending.first().cloned() else {
            drop(runner); // A hook enqueuing after this point can start the next worker.
            return Ok(());
        };
        // Persist the intent first so a crash after publication leaves a recoverable seed.
        q.retiring.insert(commit.oid.clone());
        save_json(&queue_file, &q)?;
        drop(mutex);

        let result = crate::checkout::prepare_automatic(common, &commit.oid);
        let mutex = private_dir(dir, false)?;
        FileExt::lock_exclusive(&mutex)?;
        let mut q = load_queue(dir, reg)?;
        // A burst can displace this job, or enqueue its SHA again in a newer position.
        if q.pending.first() == Some(&commit) {
            q.pending.remove(0);
        }
        match result {
            Ok(()) => {
                remember(&mut q.recent, commit);
                q.last_error = None;
            }
            Err(e) => q.last_error = Some(format!("{}: {e:#}", commit.oid)),
        }
        q.reconcile(reg.keep.get())?;
        save_json(&queue_file, &q)?;
        drop(mutex);
        retire(common, dir, reg, q.retiring)?;
    }
}

fn registration(state: &Path) -> Result<Option<(File, Registration)>> {
    if !state.try_exists()? {
        return Ok(None);
    }
    let control = private_dir(state, false)?;
    FileExt::lock_shared(&control)?;
    Ok(read_json(&state.join("enabled"))?.map(|reg| (control, reg)))
}

fn status(dir: &Path, reg: &Registration) -> Result<()> {
    println!("Automatic preparation enabled.");
    println!("Retention limit: {} per branch", reg.keep);
    if dir.try_exists()? {
        let mutex = private_dir(dir, false)?;
        FileExt::lock_shared(&mutex)?;
        let q = load_queue(dir, reg)?;
        for commit in q.pending {
            println!(
                "pending {} {}",
                String::from_utf8_lossy(&hex::decode(commit.branch)?),
                commit.oid
            );
        }
        let cache = dir.parent().unwrap();
        for Commit { branch, oid } in q.recent.iter().rev().cloned() {
            // A user can explicitly forget a pinned SHA. Publication and removal
            // rename whole entries, so status can observe them without blocking prepare.
            if crate::checkout::valid_oid(&oid)
                && fs::symlink_metadata(cache.join(&oid).join("seed.json"))
                    .is_ok_and(|m| m.is_file())
            {
                println!("{} {oid}", String::from_utf8_lossy(&hex::decode(branch)?));
            }
        }
        if let Some(message) = q.last_error {
            println!("Last preparation error: {message}");
        }
    }
    Ok(())
}

fn parse_keep(value: &OsStr) -> Result<NonZeroUsize> {
    value
        .to_str()
        .and_then(|value| value.parse().ok())
        .context("retention limit must be a positive integer")
}

fn enable_options(args: &[OsString]) -> Result<(bool, Option<NonZeroUsize>)> {
    let mut args = args.iter();
    let mut external = false;
    let mut keep = None;
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--existing-hook") if !external => external = true,
            Some("--keep") if keep.is_none() => {
                keep = Some(parse_keep(
                    args.next().context("--keep requires a positive integer")?,
                )?);
            }
            _ => bail!("expected {USAGE}"),
        }
    }
    Ok((external, keep))
}

pub(crate) fn run(args: &[OsString]) -> Result<ExitCode> {
    let cmd = args.first().and_then(|s| s.to_str());
    if matches!(cmd, None | Some("--help" | "-h")) {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let common = PathBuf::from(line(git(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ])?))
    .canonicalize()?;
    let state = common.join("worktree-fast-auto");
    match (cmd, &args[1..]) {
        (Some("enable"), options) => {
            let (external, keep) = enable_options(options)?;
            enable(&common, &state, external, keep)?;
        }
        (Some("keep"), [value]) => set_keep(&common, &state, parse_keep(value)?)?,
        (Some("disable"), []) => disable(&common, &state)?,
        (Some("notify"), []) => notify(&common, &state)?,
        (Some("status"), []) | (Some("--worker"), [_]) => {
            // Hold this shared lock while draining so disable waits for existing jobs.
            let Some((_control, reg)) = registration(&state)? else {
                if cmd == Some("status") {
                    println!("Automatic preparation disabled.");
                    println!("Retention limit: {} per branch", saved_keep(&state)?);
                }
                return Ok(ExitCode::SUCCESS);
            };
            let dir = crate::checkout::cache_root(&common)?.join("automatic");
            match cmd {
                Some("--worker") if args[1] == reg.generation.as_str() => {
                    drain(&common, &dir, &reg)?
                }
                Some("status") => status(&dir, &reg)?,
                _ => (),
            }
        }
        _ => bail!("expected {USAGE}"),
    }
    Ok(ExitCode::SUCCESS)
}
