//! Seal a completed, private T3/pnpm install and clone its generated directories
//! into an ordinary Git worktree. The caller owns the cache and holds its lease.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;
use walkdir::WalkDir;

const MODULES: &str = "node_modules/.modules.yaml"; // pnpm 12's JSON serialization
const STATE: &str = "node_modules/.pnpm-workspace-state-v1.json";
const STORE_LINK: &str = "node_modules/.worktree-fast-store";
const PRIVATE: &str = "node_modules/.worktree-fast-local";
const ENV_FILES: [&str; 2] = [".env", "infra/relay/.env"];

#[derive(Debug)]
pub struct Miss(String);
impl std::error::Error for Miss {}
impl fmt::Display for Miss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
fn miss<T>(reason: impl Into<String>) -> Result<T> {
    Err(Miss(reason.into()).into())
}

#[derive(Serialize, Deserialize)]
struct Plan {
    version: u8,
    commit: String,
    repo: PathBuf,
    seed: PathBuf,
    store: PathBuf,
    store_id: [u64; 2],
    roots: Vec<PathBuf>,
    inputs: BTreeMap<PathBuf, String>,
    // Tracked and nonignored mobile source paths. File contents may be edited.
    sources: Vec<PathBuf>,
    inventory: BTreeMap<PathBuf, bool>,    // true for Git symlinks
    packages: BTreeMap<PathBuf, PathBuf>,  // workspace source -> private installed package
    hardlinks: BTreeMap<PathBuf, PathBuf>, // checkout-local destination -> tracked source
    registry_links: BTreeMap<PathBuf, PathBuf>,
    local_links: BTreeMap<PathBuf, PathBuf>,
    modules: Value,
    workspace: Value,
}

fn direct_relative(path: &Path) -> Result<()> {
    ensure!(
        path.components().all(|c| matches!(c, Component::Normal(_))),
        "invalid relative path: {}",
        path.display()
    );
    Ok(())
}

fn direct_dir(path: &Path) -> Result<()> {
    ensure!(
        path.is_dir() && fs::canonicalize(path)? == path,
        "indirect or missing directory: {}",
        path.display()
    );
    Ok(())
}

fn regular(path: &Path) -> Result<()> {
    direct_dir(path.parent().context("missing file parent")?)?;
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "expected regular file: {}",
        path.display()
    );
    Ok(())
}

fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn file_id(path: &Path) -> Result<[u64; 2]> {
    let st = fs::metadata(path)?;
    Ok([st.dev(), st.ino()])
}
fn audit_file(path: &Path) -> Result<String> {
    regular(path)?;
    Ok(hex::encode(Sha256::digest(fs::read(path)?)))
}
fn utf(path: &Path) -> Result<&str> {
    path.to_str().context("pnpm paths must be valid UTF-8")
}
fn object(value: &Value) -> Result<&serde_json::Map<String, Value>> {
    value.as_object().context("invalid pnpm object")
}
fn string(value: &Value) -> Result<&str> {
    value.as_str().context("invalid pnpm string")
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    let output = command.current_dir(root).args(args).output()?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn relative(from_dir: &Path, to: &Path) -> PathBuf {
    let shared = from_dir
        .components()
        .zip(to.components())
        .take_while(|(a, b)| a == b)
        .count();
    let mut path = PathBuf::new();
    for _ in from_dir.components().skip(shared) {
        path.push("..");
    }
    path.extend(to.components().skip(shared).map(|c| c.as_os_str()));
    path
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    regular(path)?;
    let mut tmp = NamedTempFile::new_in(path.parent().unwrap())?;
    tmp.write_all(bytes)?;
    tmp.as_file()
        .set_permissions(fs::metadata(path)?.permissions())?;
    tmp.persist(path)?;
    Ok(())
}

fn install_inputs(
    root: &Path,
    projects: &[PathBuf],
    sources: &[PathBuf],
) -> Result<BTreeMap<PathBuf, String>> {
    let mut paths: BTreeSet<PathBuf> = [
        "package.json",
        "pnpm-workspace.yaml",
        "pnpm-lock.yaml",
        ".pnpmfile.cjs",
        "t3.json",
        "scripts/setup-worktree.ts",
        "vite.config.ts",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    for p in projects.iter().chain(sources) {
        paths.insert(p.join("package.json"));
    }
    for optional in [
        ".npmrc",
        ".nvmrc",
        ".node-version",
        ".pnpmfile.mjs",
        "pnpmfile.cjs",
    ] {
        if present(&root.join(optional))? {
            paths.insert(optional.into());
        }
    }
    // Detect changes in the set of configured workspace importers as well as
    // their manifest contents, including a newly created, untracked importer.
    for dir in ["apps", "infra", "packages"] {
        for entry in fs::read_dir(root.join(dir))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path().join("package.json");
            if present(&path)? {
                paths.insert(path.strip_prefix(root)?.into());
            }
        }
    }
    for dir in ["evals", "oxlint-plugin-t3code", "scripts"] {
        let path = Path::new(dir).join("package.json");
        if present(&root.join(&path))? {
            paths.insert(path);
        }
    }
    let extra = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "patches/",
            "apps/mobile/deps/",
        ],
    )?;
    for item in std::str::from_utf8(&extra)?.split_terminator('\0') {
        paths.insert(item.into());
    }
    paths
        .into_iter()
        .map(|p| Ok((p.clone(), audit_file(&root.join(p))?)))
        .collect()
}

fn inventory(root: &Path, sources: &[PathBuf]) -> Result<BTreeMap<PathBuf, bool>> {
    let mut args = vec![
        "ls-files",
        "-z",
        "--stage",
        "--others",
        "--exclude-standard",
        "--",
    ];
    for source in sources {
        args.push(utf(source)?);
    }
    let entries = git(root, &args)?;
    let mut result = BTreeMap::new();
    for entry in std::str::from_utf8(&entries)?.split_terminator('\0') {
        let (header, name) = entry
            .split_once('\t')
            .context("untracked injected-package source")?;
        let fields: Vec<_> = header.split_whitespace().collect();
        ensure!(
            fields.len() == 3
                && fields[2] == "0"
                && matches!(fields[0], "100644" | "100755" | "120000"),
            "unmerged or unsupported injected source"
        );
        let p = Path::new(name);
        direct_relative(p)?;
        let full = root.join(p);
        direct_dir(full.parent().unwrap())?;
        let st = fs::symlink_metadata(full)?;
        ensure!(
            if fields[0] == "120000" {
                st.is_symlink()
            } else {
                st.is_file()
            },
            "injected source type changed: {name}"
        );
        result.insert(p.into(), fields[0] == "120000");
    }
    Ok(result)
}

fn walk(root: &Path) -> impl Iterator<Item = Result<walkdir::DirEntry, walkdir::Error>> {
    WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
}
fn projects_at(seed: &Path, state: &Value) -> Result<Vec<PathBuf>> {
    object(&state["projects"])?
        .keys()
        .map(|p| {
            let rel = Path::new(p).strip_prefix(seed)?;
            direct_relative(rel)?;
            Ok(rel.into())
        })
        .collect()
}

impl Plan {
    fn load(path: &Path) -> Result<Self> {
        regular(path)?;
        let st = fs::metadata(path)?;
        ensure!(
            st.uid() == unsafe { libc::geteuid() } && st.mode() & 0o022 == 0,
            "untrusted plan ownership/mode"
        );
        let plan: Self = serde_json::from_reader(File::open(path)?)?;
        ensure!(
            plan.version == 1 && plan.seed.is_absolute() && plan.store.is_absolute(),
            "unknown installation plan"
        );
        direct_dir(&plan.seed)?;
        direct_dir(&plan.store)?;
        ensure!(
            file_id(&plan.store)? == plan.store_id,
            "shared dependency store was replaced"
        );
        for root in &plan.roots {
            direct_relative(root)?;
            ensure!(
                root.file_name() == Some("node_modules".as_ref())
                    && !root
                        .parent()
                        .unwrap()
                        .components()
                        .any(|c| c.as_os_str() == "node_modules" || c.as_os_str() == ".git"),
                "invalid dependency root"
            );
        }
        for p in plan.hardlinks.keys().chain(plan.packages.values()) {
            direct_relative(p)?;
            ensure!(p.starts_with(PRIVATE), "invalid private slot");
        }
        for p in plan.hardlinks.values().chain(&plan.sources) {
            direct_relative(p)?;
            ensure!(
                p.starts_with("apps/mobile/modules"),
                "invalid mobile source"
            );
        }
        Ok(plan)
    }

    fn validate_work(&self, work: &Path) -> Result<()> {
        let projects = projects_at(&self.seed, &self.workspace)?;
        if install_inputs(work, &projects, &self.sources)? != self.inputs {
            bail!("install manifests, tool configuration, workspace roster or patches changed");
        }
        ensure!(
            inventory(work, &self.sources)? == self.inventory,
            "mobile package file inventory changed"
        );
        Ok(())
    }

    fn metadata(&self, work: &Path) -> Result<[(PathBuf, Vec<u8>); 2]> {
        let mut modules = self.modules.clone();
        modules["virtualStoreDir"] =
            json!(Path::new(STORE_LINK).file_name().unwrap().to_str().unwrap());
        for (source, pkg) in &self.packages {
            modules["injectedDeps"][utf(source)?] = json!([work.join(pkg)]);
        }
        let mut state = self.workspace.clone();
        let mut projects = serde_json::Map::new();
        for (p, v) in object(&state["projects"])? {
            projects.insert(
                utf(&work.join(Path::new(p).strip_prefix(&self.seed)?))?.into(),
                v.clone(),
            );
        }
        state["projects"] = projects.into();
        state["pnpmfiles"] = json!([work.join(".pnpmfile.cjs")]);
        Ok([
            (MODULES.into(), serde_json::to_vec(&modules)?),
            (STATE.into(), serde_json::to_vec(&state)?),
        ])
    }
}

pub fn seal(seed: &Path, store: &Path, repo: &Path, commit: &str, plan_file: &Path) -> Result<()> {
    direct_dir(seed)?;
    direct_dir(store)?;
    ensure!(
        !seed.starts_with(store) && !store.starts_with(seed),
        "seed and store overlap"
    );
    ensure!(
        String::from_utf8(git(seed, &["rev-parse", "HEAD"])?)?.trim() == commit
            && git(seed, &["status", "--porcelain", "-uall"])?.is_empty(),
        "dirty or mismatched seed"
    );
    regular(&seed.join(".t3/setup-done"))?;
    ensure!(
        !present(&seed.join(".t3/setup-running"))? && !present(&seed.join(".t3/setup-failed"))?,
        "incomplete T3 install"
    );
    let modules: Value = serde_json::from_reader(File::open(seed.join(MODULES))?)?;
    let state: Value = serde_json::from_reader(File::open(seed.join(STATE))?)?;
    ensure!(
        modules["layoutVersion"] == 5
            && modules["packageManager"] == "pnpm@12.14.0"
            && modules["pendingBuilds"] == json!([])
            && state["settings"]["enableGlobalVirtualStore"] == true
            && fs::canonicalize(
                seed.join("node_modules")
                    .join(string(&modules["virtualStoreDir"])?)
            )? == store
            && state["pnpmfiles"] == json!([seed.join(".pnpmfile.cjs")]),
        "unsupported pnpm install layout"
    );
    let projects = projects_at(seed, &state)?;
    let roots: Vec<_> = projects
        .iter()
        .map(|p| p.join("node_modules"))
        .filter(|p| seed.join(p).is_dir())
        .collect();
    ensure!(
        roots.contains(&PathBuf::from("node_modules")),
        "missing root install"
    );
    // Vendored tracked node_modules fixtures belong to Git. Never overlay
    // a generated root whose directory also contains tracked entries.
    let tracked = git(seed, &["ls-files", "-z"])?;
    for item in std::str::from_utf8(&tracked)?.split_terminator('\0') {
        if item.contains("node_modules") {
            ensure!(
                !roots.iter().any(|r| Path::new(item).starts_with(r)),
                "tracked dependency root: {item}"
            );
        }
    }
    let mut plan = Plan {
        version: 1,
        commit: commit.into(),
        repo: fs::canonicalize(repo)?,
        seed: seed.into(),
        store: store.into(),
        store_id: file_id(store)?,
        roots,
        inputs: BTreeMap::new(),
        sources: Vec::new(),
        inventory: BTreeMap::new(),
        packages: BTreeMap::new(),
        hardlinks: BTreeMap::new(),
        registry_links: BTreeMap::new(),
        local_links: BTreeMap::new(),
        modules: modules.clone(),
        workspace: state,
    };
    let mut slot_sources = BTreeMap::new(); // Store package -> checkout-private installed path.
    let mut slots = Vec::new();
    for (i, (source, targets)) in object(&modules["injectedDeps"])?.iter().enumerate() {
        let src = Path::new(source);
        direct_relative(src)?;
        ensure!(
            src.starts_with("apps/mobile/modules"),
            "unreviewed injected source"
        );
        let manifest: Value =
            serde_json::from_reader(File::open(seed.join(src).join("package.json"))?)?;
        let name = Path::new(string(&manifest["name"])?);
        direct_relative(name)?;
        ensure!(
            name.starts_with("@t3tools") && name.components().count() == 2,
            "unexpected injected package name"
        );
        let targets = targets.as_array().context("injectedDeps target array")?;
        ensure!(targets.len() == 1, "ambiguous injected package");
        let pkg = fs::canonicalize(string(&targets[0])?)?;
        let slot = pkg
            .ancestors()
            .nth(3)
            .context("invalid injected slot")?
            .to_path_buf();
        ensure!(
            slot.starts_with(store) && pkg == slot.join("node_modules").join(name),
            "foreign injected slot"
        );
        let private = Path::new(PRIVATE).join(i.to_string());
        let dest = private.join("node_modules").join(name);
        plan.sources.push(src.to_owned());
        plan.packages.insert(src.into(), dest.clone());
        ensure!(
            slot_sources.insert(pkg.clone(), dest.clone()).is_none(),
            "repeated injected slot"
        );
        slots.push((slot, private, pkg, src.to_owned(), dest));
    }
    ensure!(
        slots.len() == 4,
        "expected four T3 mobile file dependencies"
    );
    plan.inputs = install_inputs(seed, &projects, &plan.sources)?;
    plan.inventory = inventory(seed, &plan.sources)?;
    // Verify registry packages cannot reach another checkout or an injected
    // mutable slot. Dangling links make the private layout unverifiable.
    let seed_prefix = utf(seed)?.as_bytes();
    for entry in walk(store) {
        let e = entry?;
        let p = e.path();
        if e.file_type().is_symlink() {
            let target = fs::canonicalize(p)
                .with_context(|| format!("invalid registry link {}", p.display()))?;
            ensure!(
                target.starts_with(store),
                "registry link escapes the store: {}",
                p.display()
            );
            ensure!(
                !slots
                    .iter()
                    .any(|(slot, _, _, _, _)| target.starts_with(slot) && !p.starts_with(slot)),
                "registry package aliases mutable injected slot"
            );
        } else if e.file_type().is_file()
            && p.parent().and_then(Path::file_name) == Some(".bin".as_ref())
        {
            ensure!(
                !fs::read(p)?
                    .windows(seed_prefix.len())
                    .any(|w| w == seed_prefix),
                "registry shim embeds seed path"
            );
        }
    }
    // Normalize in the private seed ONCE. Each later clone inherits portable
    // symlinks and launchers without issuing thousands of symlink syscalls.
    for rel in &plan.roots {
        let root = seed.join(rel);
        direct_dir(&root)?;
        for entry in walk(&root) {
            let e = entry?;
            let p = e.path();
            let r = p.strip_prefix(seed)?;
            if e.file_type().is_symlink() {
                let target = fs::canonicalize(p)?;
                if let Some(local) = slot_sources.get(&target) {
                    plan.local_links.insert(r.into(), local.clone());
                } else if target.starts_with(store) {
                    plan.registry_links.insert(r.into(), target);
                } else {
                    ensure!(
                        target.starts_with(seed) && !fs::read_link(p)?.is_absolute(),
                        "unrelocatable workspace link: {}",
                        p.display()
                    );
                }
            } else if e.file_type().is_file() && r != Path::new(STATE) {
                let bytes = fs::read(p)?;
                ensure!(
                    !bytes.windows(seed_prefix.len()).any(|w| w == seed_prefix),
                    "installed file embeds seed path: {}",
                    r.display()
                );
                if p.parent().and_then(Path::file_name) == Some(".bin".as_ref()) {
                    let text = std::str::from_utf8(&bytes)?;
                    let old = relative(p.parent().unwrap(), store);
                    if text.contains(utf(&old)?) {
                        let new = relative(p.parent().unwrap(), &seed.join(STORE_LINK));
                        write_atomic(p, text.replace(utf(&old)?, utf(&new)?).as_bytes())?;
                    }
                }
            } else {
                ensure!(
                    e.file_type().is_dir() || r == Path::new(STATE),
                    "unsupported installed entry"
                );
            }
        }
    }
    for (rel, target) in &plan.registry_links {
        let p = seed.join(rel);
        fs::remove_file(&p)?;
        symlink(target, &p)?;
    }
    symlink(store, seed.join(STORE_LINK))?;
    fs::create_dir(seed.join(PRIVATE))?;
    for (slot, private, pkg, source, dest) in &slots {
        crate::apfs::clone_path(slot, &seed.join(private))?;
        for entry in walk(slot) {
            let e = entry?;
            let p = e.path();
            let r = p.strip_prefix(slot)?;
            let new = seed.join(private).join(r);
            if e.file_type().is_symlink() {
                let target = fs::canonicalize(p)?;
                ensure!(
                    target.starts_with(store) && !p.starts_with(pkg),
                    "unsupported injected source link"
                );
                fs::remove_file(&new)?;
                symlink(target, new)?;
            } else if e.file_type().is_file() {
                let tail = p.strip_prefix(pkg)?;
                let local = source.join(tail);
                let src = seed.join(&local);
                if src.is_file() && file_id(&src)? == file_id(p)? {
                    regular(&src)?;
                    fs::remove_file(&new)?;
                    fs::hard_link(src, new)?;
                    plan.hardlinks.insert(dest.join(tail), local);
                } else {
                    ensure!(
                        tail.starts_with("node_modules/.bin")
                            && fs::read_to_string(p)?.starts_with("#!/bin/sh"),
                        "unrecognized injected file: {}",
                        p.display()
                    );
                }
            }
        }
    }
    for (rel, local) in &plan.local_links {
        let p = seed.join(rel);
        fs::remove_file(&p)?;
        symlink(relative(p.parent().unwrap(), &seed.join(local)), p)?;
    }
    ensure!(
        plan.local_links.values().collect::<BTreeSet<_>>()
            == plan.packages.values().collect::<BTreeSet<_>>(),
        "missing injected dependency links"
    );
    ensure!(!present(plan_file)?, "plan already exists");
    let mut tmp = NamedTempFile::new_in(plan_file.parent().context("plan has no parent")?)?;
    serde_json::to_writer(tmp.as_file_mut(), &plan)?;
    tmp.persist_noclobber(plan_file)?;
    Ok(())
}

struct Created {
    path: PathBuf,
    id: [u64; 2],
    recursive: bool,
}

// Record only paths allocated by this invocation. Never follow an unexpected
// replacement on rollback. An interrupted/incomplete setup keeps its lease.
fn record(created: &mut Vec<Created>, path: &Path, recursive: bool) -> Result<()> {
    let s = fs::symlink_metadata(path)?;
    created.push(Created {
        path: path.into(),
        id: [s.dev(), s.ino()],
        recursive,
    });
    Ok(())
}
fn rollback(created: &[Created]) -> Result<()> {
    for part in created.iter().rev() {
        let s = fs::symlink_metadata(&part.path)?;
        ensure!(
            [s.dev(), s.ino()] == part.id,
            "setup artifact was replaced: {}",
            part.path.display()
        );
        if part.recursive {
            fs::remove_dir_all(&part.path)?;
        } else if s.is_dir() {
            fs::remove_dir(&part.path)?;
        } else {
            fs::remove_file(&part.path)?;
        }
    }
    Ok(())
}

fn preflight_env(repo: &Path, work: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut links = Vec::new();
    for rel in ENV_FILES {
        let src = repo.join(rel);
        if !present(&src)? || !fs::symlink_metadata(&src)?.is_file() {
            continue;
        }
        let dst = work.join(rel);
        direct_dir(dst.parent().unwrap())?;
        if present(&dst)? {
            if dst.is_symlink() && fs::read_link(&dst)? != src {
                return miss(format!("existing env link needs regular T3 setup: {rel}"));
            }
        } else {
            links.push((src, dst));
        }
    }
    Ok(links)
}

pub fn hydrate(plan_file: &Path, work: &Path, repo: &Path, commit: &str) -> Result<Value> {
    let plan = Plan::load(plan_file)?;
    direct_dir(work)?;
    ensure!(
        fs::canonicalize(repo)? == plan.repo
            && !work.starts_with(&plan.seed)
            && !plan.seed.starts_with(work)
            && !work.starts_with(&plan.store)
            && !plan.store.starts_with(work),
        "invalid cache or checkout location"
    );
    if commit != plan.commit || file_id(work)?[0] != plan.store_id[0] {
        return miss("revision or filesystem differs from the dependency seed");
    }
    if let Err(err) = plan.validate_work(work) {
        return miss(format!("incompatible install inputs: {err:#}"));
    }
    for r in &plan.roots {
        let dest = work.join(r);
        direct_dir(dest.parent().unwrap())?;
        if present(&dest)? {
            return miss(format!("dependency root already exists: {}", r.display()));
        }
    }
    let env = preflight_env(repo, work)?;
    let markers = work.join(".t3");
    if present(&markers)? {
        direct_dir(&markers)?;
    }
    let running = markers.join("setup-running");
    let done = markers.join("setup-done");
    ensure!(!present(&running)?, "another setup is running");
    if present(&done)? || present(&markers.join("setup-failed"))? {
        return miss("existing setup result requires the regular installer");
    }
    let mut created = Vec::new();
    let result = (|| -> Result<Value> {
        if !markers.exists() {
            fs::create_dir(&markers)?;
            record(&mut created, &markers, false)?;
        }
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&running)?;
        record(&mut created, &running, false)?;
        writeln!(marker, "{}", std::process::id())?;
        for r in &plan.roots {
            let dest = work.join(r);
            crate::apfs::clone_path(&plan.seed.join(r), &dest)
                .with_context(|| format!("clone {}", r.display()))?;
            record(&mut created, &dest, true)?;
        }
        for (dest, source) in &plan.hardlinks {
            let src = work.join(source);
            let dst = work.join(dest);
            regular(&src)?;
            regular(&dst)?;
            ensure!(
                file_id(&src)? != file_id(&plan.seed.join(source))?,
                "checkout source aliases seed: {}",
                source.display()
            );
            fs::remove_file(&dst)?;
            fs::hard_link(&src, &dst)?;
        }
        for (r, data) in plan.metadata(work)? {
            write_atomic(&work.join(r), &data)?;
        }
        for (src, dst) in env {
            symlink(src, &dst)?;
            record(&mut created, &dst, false)?;
        }
        let _done = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&done)?;
        record(&mut created, &done, false)?;
        fs::remove_file(&running)?;
        Ok(
            json!({"roots":plan.roots.len(),"injected_files":plan.hardlinks.len(),
            "registry_links":plan.registry_links.len()}),
        )
    })();
    if let Err(original) = &result {
        rollback(&created)
            .with_context(|| format!("could not roll back failed setup ({original:#})"))?;
    }
    result
}

pub fn refresh(plan_file: &Path, work: &Path, old_work: &Path, commit: &str) -> Result<Value> {
    let plan = Plan::load(plan_file)?;
    direct_dir(work)?;
    ensure!(
        old_work.is_absolute() && !old_work.components().any(|c| c == Component::ParentDir),
        "invalid original checkout path"
    );
    ensure!(
        commit == plan.commit,
        "leased revision changed; use the project's frozen installer"
    );
    plan.validate_work(work)?;
    let done = work.join(".t3/setup-done");
    regular(&done)?;
    ensure!(
        fs::metadata(&done)?.len() == 0
            && !present(&work.join(".t3/setup-running"))?
            && !present(&work.join(".t3/setup-failed"))?,
        "worktree setup is incomplete"
    );
    for root in &plan.roots {
        direct_dir(&work.join(root))?;
    }
    ensure!(
        fs::read_link(work.join(STORE_LINK))? == plan.store,
        "shared store alias was changed"
    );
    for (rel, target) in &plan.registry_links {
        ensure!(
            fs::read_link(work.join(rel))? == *target && target.exists(),
            "registry link changed: {}",
            rel.display()
        );
    }
    for (rel, private) in &plan.local_links {
        ensure!(
            fs::canonicalize(work.join(rel))? == work.join(private),
            "mobile package link changed"
        );
    }
    for (target, src) in &plan.hardlinks {
        ensure!(
            file_id(&work.join(target))? == file_id(&work.join(src))?,
            "checkout-local mobile package changed"
        );
    }
    let mut updates = Vec::new();
    for ((r, old), (_, new)) in plan
        .metadata(old_work)?
        .into_iter()
        .zip(plan.metadata(work)?)
    {
        let path = work.join(r);
        regular(&path)?;
        let current = fs::read(&path)?;
        if current != new {
            ensure!(
                current == old,
                "locally modified pnpm metadata: {}",
                path.display()
            );
            updates.push((path, new));
        }
    }
    // Preflight both first. A crash between replacements can be retried.
    for (path, data) in &updates {
        write_atomic(path, data)?;
    }
    Ok(json!({"rewritten_files":updates.len()}))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;
    struct Fixture {
        _tmp: TempDir,
        repo: PathBuf,
        plan: PathBuf,
        work: PathBuf,
        commit: String,
    }
    fn put(path: &Path, bytes: &[u8]) -> Result<()> {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, bytes)?;
        Ok(())
    }
    fn json_file(path: &Path, value: Value) -> Result<()> {
        put(path, &serde_json::to_vec(&value)?)
    }
    fn git_at(repo: &Path, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8(git(repo, args)?)?.trim().to_owned())
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let tmp = tempfile::tempdir()?;
            let base = fs::canonicalize(tmp.path())?;
            let repo = base.join("repo");
            let seed = base.join("generation/work");
            let store = base.join("generation/store");
            let plan = base.join("generation/plan.json");
            let work = base.join("arbitrary path").join("fixture checkout");
            fs::create_dir(&repo)?;
            fs::create_dir_all(work.parent().unwrap())?;
            git(&repo, &["init", "-qb", "main", "--template="])?;
            git(&repo, &["config", "core.hooksPath", "/dev/null"])?;
            put(&repo.join(".gitignore"), b".env\n.t3/\nnode_modules/\n")?;
            for p in [
                "pnpm-lock.yaml",
                "pnpm-workspace.yaml",
                ".pnpmfile.cjs",
                "t3.json",
                "scripts/setup-worktree.ts",
                "vite.config.ts",
                "packages/.keep",
                "infra/relay/file",
            ] {
                put(&repo.join(p), b"fixture")?;
            }
            for p in ["package.json", "apps/mobile/package.json"] {
                json_file(&repo.join(p), json!({}))?;
            }
            for i in 0..4 {
                let source = repo.join(format!("apps/mobile/modules/p{i}"));
                json_file(
                    &source.join("package.json"),
                    json!({"name":format!("@t3tools/p{i}")}),
                )?;
                put(&source.join("lib.js"), b"independent contents")?;
            }
            git(&repo, &["add", "."])?;
            git(
                &repo,
                &[
                    "-c",
                    "user.name=Worktree Test",
                    "-c",
                    "user.email=test@localhost",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-qm",
                    "fixture",
                ],
            )?;
            let commit = git_at(&repo, &["rev-parse", "HEAD"])?;
            fs::create_dir_all(&store)?;
            for w in [&seed, &work] {
                git(
                    &repo,
                    &["worktree", "add", "-q", "--detach", utf(w)?, &commit],
                )?;
            }
            put(&repo.join(".env"), b"local env")?;
            let node = seed.join("node_modules");
            let immutable = store.join("registry/node_modules/fixture");
            put(&immutable.join("bin"), b"registry command")?;
            fs::create_dir_all(node.join(".bin"))?;
            symlink(relative(&node, &immutable), node.join("fixture"))?;
            let bin = node.join(".bin/fixture");
            put(&bin,format!("#!/bin/sh\nbasedir=$(dirname \"$0\")\ncat \"$basedir/{}/registry/node_modules/fixture/bin\"\n",utf(&relative(bin.parent().unwrap(),&store))?).as_bytes())?;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
            let mobile = seed.join("apps/mobile");
            let mobile_targets = mobile.join("node_modules/@t3tools");
            fs::create_dir_all(&mobile_targets)?;
            let mut injections = serde_json::Map::new();
            for i in 0..4 {
                let source = format!("apps/mobile/modules/p{i}");
                let slot = store.join(format!("mobile-{i}"));
                let pkg = slot.join(format!("node_modules/@t3tools/p{i}"));
                fs::create_dir_all(&pkg)?;
                for name in ["package.json", "lib.js"] {
                    fs::hard_link(seed.join(&source).join(name), pkg.join(name))?;
                }
                let peer = slot.join("node_modules/peer");
                symlink(relative(peer.parent().unwrap(), &immutable), peer)?;
                symlink(
                    relative(&mobile_targets, &pkg),
                    mobile_targets.join(format!("p{i}")),
                )?;
                injections.insert(source, json!([pkg]));
            }
            json_file(
                &seed.join(MODULES),
                json!({"layoutVersion":5,"packageManager":"pnpm@12.14.0",
                "pendingBuilds":[],"virtualStoreDir":relative(&node,&store),"injectedDeps":injections}),
            )?;
            let projects = serde_json::Map::from_iter([
                (utf(&seed)?.to_owned(), json!({})),
                (utf(&mobile)?.to_owned(), json!({})),
            ]);
            json_file(
                &seed.join(STATE),
                json!({"settings":{"enableGlobalVirtualStore":true},"projects":projects,"pnpmfiles":[seed.join(".pnpmfile.cjs")]}),
            )?;
            put(&seed.join(".t3/setup-done"), b"")?;
            seal(&seed, &store, &repo, &commit, &plan)?;
            Ok(Self {
                _tmp: tmp,
                repo,
                plan,
                work,
                commit,
            })
        }
    }

    #[test]
    fn mobile_sources_and_bins_survive_native_move() -> Result<()> {
        let f = Fixture::new()?;
        let details = hydrate(&f.plan, &f.work, &f.repo, &f.commit)?;
        assert_eq!(details["injected_files"], 8);
        let src = f.work.join("apps/mobile/modules/p3/lib.js");
        let import = f.work.join("apps/mobile/node_modules/@t3tools/p3/lib.js");
        fs::write(&src, b"changed only here")?;
        assert_eq!(fs::read(&import)?, b"changed only here");
        assert_eq!(
            fs::read(f.repo.join("apps/mobile/modules/p3/lib.js"))?,
            b"independent contents"
        );
        let moved = f.work.with_file_name("moved $(printf marker)");
        git(
            &f.repo,
            &["worktree", "move", "--", utf(&f.work)?, utf(&moved)?],
        )?;
        assert_eq!(
            Command::new(moved.join("node_modules/.bin/fixture"))
                .output()?
                .stdout,
            b"registry command"
        );
        assert_eq!(fs::canonicalize(moved.join(".env"))?, f.repo.join(".env"));
        assert_eq!(
            refresh(&f.plan, &moved, &f.work, &f.commit)?["rewritten_files"],
            2
        );
        assert_eq!(
            refresh(&f.plan, &moved, &moved, &f.commit)?["rewritten_files"],
            0
        );
        let state = fs::read_to_string(moved.join(STATE))?;
        assert!(state.contains(utf(&moved)?) && !state.contains(utf(&f.work)?));
        fs::write(moved.join(STATE), b"edited by user")?;
        assert!(refresh(&f.plan, &moved, &f.work, &f.commit).is_err());
        assert_eq!(fs::read(moved.join(STATE))?, b"edited by user");
        Ok(())
    }

    #[test]
    fn changed_inputs_and_preexisting_packages_remain_untouched() -> Result<()> {
        let f = Fixture::new()?;
        let lock = f.work.join("pnpm-lock.yaml");
        fs::write(&lock, b"updated lockfile")?;
        assert!(
            hydrate(&f.plan, &f.work, &f.repo, &f.commit)
                .unwrap_err()
                .is::<Miss>()
        );
        assert!(!present(&f.work.join("node_modules"))? && !present(&f.work.join(".t3"))?);
        fs::copy(f.repo.join("pnpm-lock.yaml"), lock)?;
        let existing = f.work.join("apps/mobile/node_modules/local");
        put(&existing, b"keep")?;
        assert!(
            hydrate(&f.plan, &f.work, &f.repo, &f.commit)
                .unwrap_err()
                .is::<Miss>()
        );
        assert_eq!(fs::read(existing)?, b"keep");
        Ok(())
    }

    #[test]
    fn failed_clone_rolls_back_only_new_files() -> Result<()> {
        let f = Fixture::new()?;
        fs::remove_dir_all(
            f.plan
                .parent()
                .unwrap()
                .join("work/apps/mobile/node_modules"),
        )?;
        let local_env = f.work.join(".env");
        put(&local_env, b"keep contents")?;
        assert!(hydrate(&f.plan, &f.work, &f.repo, &f.commit).is_err());
        assert!(!present(&f.work.join("node_modules"))? && !present(&f.work.join(".t3"))?);
        assert_eq!(fs::read(local_env)?, b"keep contents");
        Ok(())
    }
}
