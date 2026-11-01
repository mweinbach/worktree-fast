//! A repository-scoped, opt-in cache for the audited T3 frozen install.
//! Git owns user worktrees; a lease in each Git admin dir holds the store alive.
use anyhow::{Context, Result, anyhow, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        io::AsRawFd,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};
use tempfile::NamedTempFile;

const COMMIT: &str = "15fe2832727893cbdd2214383f9f971474849695";
const HOOK: &str = "scripts/setup-worktree.ts";
const HOOK_DIGEST: &str = "73f8e86a244c267637fbcb5f620e367785b1cabd90bae40ccde711d1bc57681f";
const LEASE: &str = "worktree-fast-pnpm-v1.json";

fn cmd(name: &str, cwd: &Path) -> Command {
    let mut c = Command::new(name);
    c.current_dir(cwd);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_PREFIX",
    ] {
        c.env_remove(key);
    }
    c
}
fn output(c: &mut Command) -> Result<Vec<u8>> {
    let o = c
        .output()
        .with_context(|| format!("starting {}", c.get_program().to_string_lossy()))?;
    ensure!(
        o.status.success(),
        "{} failed: {}",
        c.get_program().to_string_lossy(),
        String::from_utf8_lossy(&o.stderr).trim()
    );
    Ok(o.stdout)
}
fn trim_line(mut data: Vec<u8>) -> Vec<u8> {
    if data.last() == Some(&b'\n') {
        data.pop();
    }
    data
}
fn git(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    output(cmd("git", cwd).args(args))
}
fn git_path(cwd: &Path, arg: &str) -> Result<PathBuf> {
    let bytes = git(cwd, &["rev-parse", "--path-format=absolute", arg])?;
    Ok(PathBuf::from(OsString::from_vec(trim_line(bytes))).canonicalize()?)
}
fn main_checkout(work: &Path) -> Result<PathBuf> {
    let listing = git(work, &["worktree", "list", "--porcelain", "-z"])?;
    let first = listing
        .split(|b| *b == 0)
        .next()
        .and_then(|l| l.strip_prefix(b"worktree "))
        .context("no main Git checkout")?;
    Ok(Path::new(OsStr::from_bytes(first)).canonicalize()?)
}
fn id(p: &Path) -> Result<[u64; 2]> {
    let m = fs::symlink_metadata(p)?;
    ensure!(!m.is_symlink(), "unexpected symlink at {}", p.display());
    Ok([m.dev(), m.ino()])
}
fn checksum(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}
fn save(p: &Path, v: &impl Serialize) -> Result<()> {
    let mut tmp = NamedTempFile::new_in(p.parent().context("missing parent")?)?;
    serde_json::to_writer(&mut tmp, v)?;
    tmp.flush()?;
    tmp.persist(p)?;
    Ok(())
}
fn read<T: serde::de::DeserializeOwned>(p: &Path) -> Result<T> {
    ensure!(
        fs::symlink_metadata(p)?.is_file(),
        "expected a regular file: {}",
        p.display()
    );
    Ok(serde_json::from_reader(File::open(p)?)?)
}
fn exists(p: &Path) -> Result<bool> {
    match p.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn secure_dir(p: &Path, create: bool) -> Result<()> {
    if create
        && let Err(e) = fs::DirBuilder::new().mode(0o700).create(p)
        && e.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(e.into());
    }
    let m = p.symlink_metadata()?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "expected a private owned directory: {}",
        p.display()
    );
    Ok(())
}
fn cache_root(common: &Path) -> Result<PathBuf> {
    let base = if let Some(p) = env::var_os("XDG_CACHE_HOME").filter(|p| !p.is_empty()) {
        PathBuf::from(p)
    } else {
        PathBuf::from(env::var_os("HOME").context("no cache home")?).join(
            if cfg!(target_os = "macos") {
                "Library/Caches"
            } else {
                ".cache"
            },
        )
    };
    ensure!(base.is_absolute(), "cache home must be absolute");
    fs::create_dir_all(&base)?;
    let mut p = base.canonicalize()?;
    // The shared application namespace may also be created by another backend.
    for name in [
        "worktree-fast",
        "v1",
        "deps",
        &checksum(common.as_os_str().as_bytes()),
    ] {
        p.push(name);
        if name == "worktree-fast" || name == "v1" {
            fs::create_dir_all(&p)?;
            ensure!(
                p.symlink_metadata()?.is_dir()
                    && p.metadata()?.uid() == unsafe { libc::geteuid() }
                    && p.metadata()?.mode() & 0o022 == 0,
                "unsafe cache namespace {}",
                p.display()
            );
        } else {
            secure_dir(&p, true)?;
        }
    }
    Ok(p)
}
fn lock_dir(dir: &Path, exclusive: bool) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)?;
    if exclusive {
        FileExt::lock_exclusive(&f)?;
    } else {
        FileExt::lock_shared(&f)?;
    }
    Ok(f)
}
fn admin_dir(work: &Path, common: &Path) -> Result<PathBuf> {
    ensure!(
        work.join(".git")
            .symlink_metadata()
            .is_ok_and(|m| m.is_file()),
        "pass a linked Git worktree root"
    );
    let admin = git_path(work, "--absolute-git-dir")?;
    ensure!(
        admin.parent() == Some(common.join("worktrees").as_path()),
        "worktree belongs to another Git project"
    );
    let back = admin.join("gitdir");
    ensure!(back.symlink_metadata()?.is_file(), "invalid Git backlink");
    let pointer = PathBuf::from(OsString::from_vec(trim_line(fs::read(back)?)));
    ensure!(
        admin.join(pointer).canonicalize()? == work.join(".git"),
        "Git worktree backlink mismatch"
    );
    Ok(admin)
}

#[derive(Serialize, Deserialize)]
struct Generation {
    schema: u32,
    common: PathBuf,
    common_id: [u64; 2],
    identity: [u64; 2],
    store: [u64; 2],
    seed: Option<[u64; 2]>,
    ready: bool,
    retiring: bool,
    context: String,
    plan_digest: String,
}
#[derive(Serialize, Deserialize)]
struct Lease {
    generation: PathBuf,
    generation_id: [u64; 2],
    checkout: PathBuf,
    identity: [u64; 2],
}

fn default_macos_sdk(override_path: &OsStr) -> bool {
    #[cfg(target_os = "macos")]
    {
        let Ok(selected) = Path::new(override_path).canonicalize() else {
            return false;
        };
        // xcrun must resolve the SDK as it would with no override. The
        // unversioned SDK path and its versioned alias identify one directory.
        let Ok(probe) = Command::new("/usr/bin/xcrun")
            .env_remove("SDKROOT")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .output()
        else {
            return false;
        };
        probe.status.success()
            && PathBuf::from(OsString::from_vec(trim_line(probe.stdout)))
                .canonicalize()
                .is_ok_and(|default| default == selected)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = override_path;
        false
    }
}

fn installation_context(repo: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    let mut feed = |b: &[u8]| {
        hash.update((b.len() as u64).to_le_bytes());
        hash.update(b);
    };
    let mut vars: Vec<_> = env::vars_os()
        .filter(|(k, _)| {
            let s = k.to_string_lossy().to_ascii_uppercase();
            ["NODE_", "NPM_", "PNPM_", "VITE_", "DYLD_"]
                .iter()
                .any(|prefix| s.starts_with(prefix))
                || matches!(
                    s.as_str(),
                    "CI" | "XDG_CONFIG_HOME"
                        | "XDG_DATA_HOME"
                        | "CC"
                        | "CXX"
                        | "CFLAGS"
                        | "LDFLAGS"
                        | "MACOSX_DEPLOYMENT_TARGET"
                        | "SDKROOT"
                )
        })
        .collect();
    vars.sort();
    for (k, v) in vars {
        if k == "SDKROOT" && default_macos_sdk(&v) {
            continue;
        }
        feed(k.as_bytes());
        feed(v.as_bytes());
    }
    for entry in git(repo, &["config", "--null", "--list"])?.split(|b| *b == 0) {
        if !entry.starts_with(b"branch.") {
            feed(entry);
        }
    }
    for name in ["node", "pnpm", "vp"] {
        let path = env::split_paths(&env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(name))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
            })
            .with_context(|| format!("{name} is missing from PATH"))?
            .canonicalize()?;
        let m = path.metadata()?;
        feed(path.as_os_str().as_bytes());
        feed(&m.ino().to_le_bytes());
        feed(&m.mtime().to_le_bytes());
        feed(&m.mtime_nsec().to_le_bytes());
        feed(&m.len().to_le_bytes());
        let version = if name == "node" {
            output(cmd(name,repo).args(["-p","JSON.stringify([process.version,process.arch,process.versions.modules,process.execPath])"]))?
        } else {
            output(cmd(name, repo).arg("--version"))?
        };
        // VP also prints the current checkout's installed package inventory.
        // Only the first line describes the launcher; the inventory is empty
        // by design before hydrating a fresh worktree.
        if name == "vp" {
            feed(version.split(|b| *b == b'\n').next().unwrap_or_default());
        } else {
            feed(&version);
        }
    }
    feed(&output(cmd("uname", repo).arg("-r"))?);
    Ok(hex::encode(hash.finalize()))
}
fn validate_generation(g: &Path, common: &Path) -> Result<Generation> {
    secure_dir(g, false)?;
    let s: Generation = read(&g.join("generation.json"))?;
    ensure!(
        s.schema == 1 && s.common == common && s.common_id == id(common)? && s.identity == id(g)?,
        "generation identity changed"
    );
    for (name, expected) in [("work", s.seed), ("store", Some(s.store))] {
        let path = g.join(name);
        if let Some(expected) = expected
            && (!s.retiring || exists(&path)?)
        {
            ensure!(id(&path)? == expected, "managed {name} was replaced");
        }
    }
    Ok(s)
}
fn guarded_seed(g: &Path, common: &Path) -> Result<()> {
    let work = g.join("work");
    let _admin = admin_dir(&work, common)?;
    ensure!(
        trim_line(git(&work, &["rev-parse", "HEAD"])?) == COMMIT.as_bytes(),
        "seed HEAD changed"
    );
    ensure!(
        git(&work, &["symbolic-ref", "-q", "HEAD"]).is_err(),
        "seed now has a checked-out branch"
    );
    ensure!(
        git(
            &work,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        )?
        .is_empty(),
        "seed contains edits"
    );
    Ok(())
}
fn disk_tree(p: &Path) -> Result<()> {
    let dev = p.symlink_metadata()?.dev();
    for entry in walkdir::WalkDir::new(p).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_dir() {
            ensure!(
                entry.metadata()?.dev() == dev,
                "unexpected mounted filesystem at {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}
fn clear_generation(repo: &Path, common: &Path, g: &Path) -> Result<()> {
    let mut s = validate_generation(g, common)?;
    let listing = git(repo, &["worktree", "list", "--porcelain", "-z"])?;
    for entry in listing
        .split(|c| *c == 0)
        .filter_map(|l| l.strip_prefix(b"worktree "))
    {
        let p = Path::new(OsStr::from_bytes(entry));
        ensure!(
            !p.starts_with(g) || p == g.join("work"),
            "another Git worktree is inside the generation: {}",
            p.display()
        );
    }
    for entry in fs::read_dir(g)? {
        let name = entry?.file_name();
        ensure!(
            [
                "store",
                "work",
                "config",
                "generation.json",
                "plan.json",
                "install.log"
            ]
            .iter()
            .any(|v| name == OsStr::new(v)),
            "unrecognized entry in generation: {:?}",
            name
        );
    }
    let admins = common.join("worktrees");
    if admins.exists() {
        for a in fs::read_dir(&admins)? {
            let a = a?.path();
            let p = a.join(LEASE);
            if !exists(&p)? {
                continue;
            }
            let l: Lease = read(&p)?;
            ensure!(
                l.generation != g || l.generation_id != s.identity,
                "a Git worktree still uses this store: {}",
                fs::read_to_string(a.join("gitdir"))?.trim()
            );
        }
    }
    // Finish all destructive preflight checks before changing the registration.
    disk_tree(g)?;
    if exists(&g.join("work"))? {
        guarded_seed(g, common)?;
        ensure!(
            !exists(&admin_dir(&g.join("work"), common)?.join("locked"))?,
            "private seed worktree is locked"
        );
    }
    if !s.retiring {
        s.retiring = true;
        save(&g.join("generation.json"), &s)?;
    }
    if exists(&g.join("work"))? {
        output(
            cmd("git", repo)
                .args(["worktree", "remove", "--force", "--"])
                .arg(g.join("work")),
        )?;
    }
    // Keep the ownership record until every potentially large deletion has finished.
    for name in ["store", "config"] {
        let p = g.join(name);
        if exists(&p)? {
            fs::remove_dir_all(p)?;
        }
    }
    for name in ["install.log", "plan.json", "generation.json"] {
        let p = g.join(name);
        if exists(&p)? {
            fs::remove_file(p)?;
        }
    }
    fs::remove_dir(g)?;
    Ok(())
}

fn prepare(repo: &Path, common: &Path, g: &Path) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "APFS dependency preparation requires macOS"
    );
    ensure!(
        !g.starts_with(repo) && !repo.starts_with(g),
        "keep the cache outside the checkout"
    );
    ensure!(
        trim_line(git(repo, &["rev-parse", "HEAD"])?) == COMMIT.as_bytes(),
        "dependency preparation requires audited T3 commit {COMMIT}"
    );
    ensure!(
        !exists(g)?,
        "generation already exists at {}; retire it before preparing another",
        g.display()
    );
    let node = trim_line(output(cmd("node", repo).args(["-p", "process.version"]))?);
    ensure!(
        node == b"v26.12.1",
        "the audited T3 install needs Node 26.12.1 (found {})",
        String::from_utf8_lossy(&node)
    );
    let pnpm = trim_line(output(cmd("pnpm", repo).arg("--version"))?);
    ensure!(
        pnpm == b"12.14.0",
        "the audited T3 install needs pnpm 12.14.0"
    );
    {
        let probe = tempfile::tempdir_in(g.parent().context("missing cache parent")?)?;
        let from = probe.path().join("source");
        fs::write(&from, b"APFS clone probe")?;
        crate::apfs::clone_path(&from, &probe.path().join("clone"))
            .context("the dependency cache requires a volume supporting APFS clones")?;
    }
    fs::DirBuilder::new().mode(0o700).create(g)?;
    let store = g.join("store");
    fs::create_dir(&store)?;
    let mut state = Generation {
        schema: 1,
        common: common.to_path_buf(),
        common_id: id(common)?,
        identity: id(g)?,
        store: id(&store)?,
        seed: None,
        ready: false,
        retiring: false,
        context: String::new(),
        plan_digest: String::new(),
    };
    let manifest = g.join("generation.json");
    save(&manifest, &state)?;
    let result = (|| -> Result<()> {
        let work = g.join("work");
        output(
            cmd("git", repo)
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "checkout.workers=4",
                    "worktree",
                    "add",
                    "--detach",
                    "--",
                ])
                .arg(&work)
                .arg(COMMIT),
        )?;
        state.seed = Some(id(&work)?);
        save(&manifest, &state)?;
        ensure!(
            checksum(fs::read(work.join(HOOK))?) == HOOK_DIGEST,
            "unexpected T3 setup recipe"
        );
        let cas = PathBuf::from(OsString::from_vec(trim_line(output(
            cmd("pnpm", &work).args(["store", "path"]),
        )?)));
        ensure!(
            cas.is_absolute() && cas.file_name() == Some(OsStr::new("v11")),
            "expected pnpm's v11 content store"
        );
        let config = g.join("config/pnpm");
        fs::create_dir_all(&config)?;
        fs::write(
            config.join("config.yaml"),
            format!(
                "extendNodePath: false\nstoreDir: {}\n",
                serde_json::to_string(cas.parent().context("no CAS parent")?)?
            ),
        )?;
        let log = File::create(g.join("install.log"))?;
        let mut c = cmd("node", &work);
        for (k, _) in env::vars_os() {
            let name = k.to_string_lossy().to_ascii_uppercase();
            if name.starts_with("PNPM_CONFIG_") || name.starts_with("NPM_CONFIG_") {
                c.env_remove(k);
            }
        }
        let ok = c
            .arg(HOOK)
            .env("XDG_CONFIG_HOME", g.join("config"))
            .env("PNPM_CONFIG_VIRTUAL_STORE_TYPE", "global")
            .env("PNPM_CONFIG_GLOBAL_VIRTUAL_STORE_DIR", &store)
            .env("T3CODE_PROJECT_ROOT", repo)
            .env("T3CODE_WORKTREE_PATH", &work)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .status()?;
        ensure!(ok.success(), "T3's frozen dependency setup failed ({ok})");
        guarded_seed(g, common)?;
        crate::deps::seal(&work, &store, repo, COMMIT, &g.join("plan.json"))?;
        state.plan_digest = checksum(fs::read(g.join("plan.json"))?);
        state.context = installation_context(&work)?;
        state.ready = true;
        save(&manifest, &state)?;
        Ok(())
    })();
    if let Err(reason) = result {
        // Retain useful diagnostics before disposing of the incomplete store.
        if let Ok(file) = File::open(g.join("install.log")) {
            use std::io::{BufRead, BufReader};
            let mut tail = std::collections::VecDeque::new();
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                if tail.len() == 8 {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            for line in tail {
                eprintln!("{line}");
            }
        }
        if let Err(e) = clear_generation(repo, common, g) {
            bail!(
                "{reason:#}; incomplete generation retained at {}: {e:#}",
                g.display()
            );
        }
        return Err(reason);
    }
    println!(
        "{}",
        json!({"setup":"prepared","commit":COMMIT,"generation":g})
    );
    Ok(())
}

fn marker_state(work: &Path) -> Result<()> {
    let dir = work.join(".t3");
    if exists(&dir)? {
        ensure!(dir.symlink_metadata()?.is_dir(), "invalid .t3 directory");
    }
    ensure!(
        !exists(&dir.join("setup-running"))?,
        "worktree already has a setup-running marker"
    );
    for name in ["setup-done", "setup-failed"] {
        let path = dir.join(name);
        if exists(&path)? {
            ensure!(path.symlink_metadata()?.is_file(), "invalid setup marker");
        }
    }
    Ok(())
}
fn frozen(repo: &Path, work: &Path, lock: &File) -> Result<ExitCode> {
    let hook = work.join(HOOK);
    ensure!(
        hook.canonicalize()? == hook && hook.is_file(),
        "expected T3's tracked setup action"
    );
    ensure!(
        trim_line(git(work, &["ls-files", "--error-unmatch", "--", HOOK])?) == HOOK.as_bytes(),
        "untracked setup action"
    );
    let config: Value = read(&work.join("t3.json"))?;
    let selected = config
        .get("scripts")
        .and_then(Value::as_array)
        .and_then(|scripts| {
            scripts
                .iter()
                .find(|a| a.get("runOnWorktreeCreate").and_then(Value::as_bool) == Some(true))
        });
    ensure!(
        selected
            .and_then(|s| s.get("command"))
            .and_then(Value::as_str)
            == Some("node scripts/setup-worktree.ts"),
        "no recognized frozen T3 setup action"
    );
    ensure!(
        !work.join("node_modules").is_symlink(),
        "refusing an indirect node_modules destination"
    );
    // exec gives the original Node hook the T3 terminal's signals. Keep the
    // admin-dir flock across exec so two setup commands cannot race it.
    let fd = lock.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Err(cmd("node", work)
        .arg(HOOK)
        .env("T3CODE_PROJECT_ROOT", repo)
        .env("T3CODE_WORKTREE_PATH", work)
        .exec()
        .into())
}

#[derive(Debug)]
struct Miss(String);
impl std::fmt::Display for Miss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Miss {}
fn apply(repo: &Path, common: &Path, g: &Path, work: &Path, admin: &Path) -> Result<Value> {
    let file = admin.join(LEASE);
    let leased = exists(&file)?;
    if !exists(g)? {
        ensure!(
            !leased,
            "worktree has a lease on another or missing generation"
        );
        return Err(Miss("no prepared dependency generation".into()).into());
    }
    let s = validate_generation(g, common)?;
    ensure!(
        s.ready && !s.retiring,
        "incomplete dependency generation; inspect and retire it before retrying"
    );
    let previous = if leased {
        let prev: Lease = read(&file)?;
        ensure!(
            prev.generation == g && prev.generation_id == s.identity && prev.identity == id(work)?,
            "existing worktree lease belongs to another generation or checkout"
        );
        Some(prev)
    } else {
        None
    };
    let head = trim_line(git(work, &["rev-parse", "HEAD"])?);
    let populated_lease = leased && exists(&work.join("node_modules"))?;
    // A moved checkout can contain later code commits. For an already
    // populated graph, refresh checks every sealed installation input again.
    let unavailable = if !populated_lease && head != COMMIT.as_bytes() {
        Some("worktree revision is not covered by the prepared install")
    } else if installation_context(work)? != s.context {
        Some("worktree toolchain or install configuration differs from the seed")
    } else {
        None
    };
    if let Some(reason) = unavailable {
        ensure!(
            !leased,
            "{reason}; this worktree still has a shared-store lease"
        );
        return Err(Miss(reason.into()).into());
    }
    let plan = g.join("plan.json");
    ensure!(
        plan.symlink_metadata()?.is_file() && checksum(fs::read(&plan)?) == s.plan_digest,
        "dependency plan has changed"
    );
    let info = Lease {
        generation: g.to_owned(),
        generation_id: s.identity,
        checkout: work.to_owned(),
        identity: id(work)?,
    };
    if let Some(prev) = &previous {
        if exists(&work.join("node_modules"))? {
            let details = crate::deps::refresh(&plan, work, &prev.checkout, COMMIT)?;
            save(&file, &info)?;
            return Ok(json!({"setup":"existing","details":details}));
        }
    } else {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&file)?;
        serde_json::to_writer(&mut f, &info)?;
        f.flush()?;
    }
    let result = crate::deps::hydrate(&plan, work, repo, COMMIT);
    match result {
        Ok(details) => {
            if leased {
                save(&file, &info)?;
            }
            Ok(json!({"setup":"apfs","details":details}))
        }
        Err(e) if e.is::<crate::deps::Miss>() && !leased => {
            fs::remove_file(file)?;
            Err(Miss(e.to_string()).into())
        }
        // The lease protects the store if a process dies or cannot roll back.
        Err(e) => Err(e),
    }
}

pub fn run(args: &[OsString]) -> Result<ExitCode> {
    let cwd = env::current_dir()?.canonicalize()?;
    match args.first().and_then(|x| x.to_str()) {
        Some("prepare" | "retire") => {
            let arg = match &args[1..] {
                [] => PathBuf::from(
                    env::var_os("T3CODE_PROJECT_ROOT")
                        .unwrap_or_else(|| cwd.as_os_str().to_owned()),
                ),
                [flag, p] if flag == "--repo" => PathBuf::from(p),
                _ => bail!("expected deps prepare|retire [--repo PATH]"),
            };
            let common = git_path(&arg, "--git-common-dir")?;
            let repo = main_checkout(&arg)?;
            let cache = cache_root(&common)?;
            let generation = cache.join("generation");
            let _guard = lock_dir(&cache, true)?;
            if args[0] == "prepare" {
                prepare(&repo, &common, &generation)?;
            } else {
                clear_generation(&repo, &common, &generation)?;
                println!("{}", json!({"setup":"retired"}));
            }
        }
        Some("setup") => {
            let mut fallback = false;
            let mut checkout = None;
            for a in &args[1..] {
                if a == "--fallback" {
                    fallback = true;
                } else {
                    ensure!(
                        checkout.is_none() && !a.as_bytes().starts_with(b"-"),
                        "expected setup [--fallback] [CHECKOUT]"
                    );
                    checkout = Some(PathBuf::from(a));
                }
            }
            let work = checkout.as_deref().unwrap_or(&cwd).canonicalize()?;
            let common = git_path(&work, "--git-common-dir")?;
            let mut repo = main_checkout(&work)?;
            if checkout.is_none() {
                if let Some(p) = env::var_os("T3CODE_WORKTREE_PATH") {
                    ensure!(
                        PathBuf::from(p).canonicalize()? == work,
                        "T3 worktree does not match cwd"
                    );
                }
                if let Some(p) = env::var_os("T3CODE_PROJECT_ROOT") {
                    let p = PathBuf::from(p).canonicalize()?;
                    ensure!(
                        git_path(&p, "--git-common-dir")? == common && p != work,
                        "T3 project root does not match Git registration"
                    );
                    repo = p;
                }
            }
            let admin = admin_dir(&work, &common)?;
            let _admin_lock = lock_dir(&admin, true)?;
            marker_state(&work)?;
            let cache = cache_root(&common)?;
            ensure!(
                !work.starts_with(&cache),
                "refusing to set up the immutable seed itself"
            );
            let result = {
                let _cache_lock = lock_dir(&cache, false)?;
                apply(&repo, &common, &cache.join("generation"), &work, &admin)
            };
            match result {
                Ok(v) => println!("{v}"),
                Err(e) if e.is::<Miss>() && fallback => {
                    eprintln!("Using T3's frozen setup: {e}");
                    return frozen(&repo, &work, &_admin_lock);
                }
                Err(e) => return Err(e),
            }
        }
        _ => {
            return Err(anyhow!(
                "expected deps prepare|retire [--repo PATH], or setup [--fallback] [CHECKOUT]"
            ));
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn explicit_default_sdk_is_equivalent_to_an_unset_sdkroot() {
        let output = Command::new("/usr/bin/xcrun")
            .env_remove("SDKROOT")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let sdk = OsString::from_vec(trim_line(output.stdout));
        assert!(default_macos_sdk(&sdk));
        let alternative = tempfile::tempdir().unwrap();
        assert!(!default_macos_sdk(alternative.path().as_os_str()));
        assert!(!default_macos_sdk(
            alternative.path().join("missing.sdk").as_os_str()
        ));
    }
}
