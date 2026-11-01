//! Bake the audited T3 frozen install into a private APFS image, then rebase
//! its generated metadata to each actual checkout before publishing readiness.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use walkdir::WalkDir;

pub(crate) const PIN: &str = "15fe2832727893cbdd2214383f9f971474849695";
const HOOK: &str = "scripts/setup-worktree.ts";
const HOOK_SHA: &str = "73f8e86a244c267637fbcb5f620e367785b1cabd90bae40ccde711d1bc57681f";
const STATE: &str = "node_modules/.pnpm-workspace-state-v1.json";
const MODULES: &str = "node_modules/.modules.yaml";
const PLAN: &str = ".t3/worktree-fast-image.json";
const DONE: &str = ".t3/setup-done";
const ENV_FILES: [&str; 2] = [".env", "infra/relay/.env"];

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Toolchain {
    node: Vec<String>,
    pnpm: String,
    vp: String,
    kernel: String,
}

#[derive(Serialize, Deserialize)]
struct Plan {
    schema: u32,
    origin: PathBuf,
    commit: String,
    tools: Toolchain,
    inputs: BTreeMap<PathBuf, String>,
    generated: BTreeMap<PathBuf, String>,
    packages: BTreeMap<String, PathBuf>,
    hardlinks: Vec<Vec<PathBuf>>,
}

fn command(program: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd).stdin(Stdio::null());
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"GIT_") {
            cmd.env_remove(name);
        }
    }
    cmd
}

fn output(cwd: &Path, program: &str, args: &[&str]) -> Result<String> {
    let out = command(program, cwd)
        .args(args)
        .output()
        .with_context(|| format!("run {program}"))?;
    ensure!(
        out.status.success(),
        "{program} {args:?} exited with {}",
        out.status
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

pub(crate) fn activation_toolchain(work: &Path) -> Result<Toolchain> {
    let node = output(
        work,
        "node",
        &[
            "-p",
            "JSON.stringify([process.version,process.arch,process.versions.modules,process.execPath])",
        ],
    )?;
    Ok(Toolchain {
        node: serde_json::from_str(&node)?,
        pnpm: output(work, "pnpm", &["--version"])?,
        // Later lines describe the *current checkout's* installed packages.
        // Only the first line identifies the host Vite+ executable.
        vp: output(work, "vp", &["--version"])?
            .lines()
            .next()
            .context("empty Vite+ version")?
            .to_owned(),
        kernel: output(work, "uname", &["-rm"])?,
    })
}

// Metadata writes and hardlink repairs must never follow an alias to another
// file or traverse an external directory. Generated paths are relative to work.
fn direct(work: &Path, rel: &Path) -> Result<PathBuf> {
    ensure!(
        !rel.as_os_str().is_empty()
            && rel
                .components()
                .all(|p| matches!(p, Component::Normal(s) if s != ".git")),
        "invalid image member {}",
        rel.display()
    );
    let p = work.join(rel);
    ensure!(
        p.parent().context("missing parent")?.canonicalize()? == p.parent().unwrap()
            && !p.is_symlink(),
        "indirect image member {}",
        p.display()
    );
    Ok(p)
}

fn json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_reader(
        File::open(path).with_context(|| format!("read {}", path.display()))?,
    )?)
}

fn hash(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn replace_file(path: &Path, data: &[u8]) -> Result<()> {
    let mut t = tempfile::NamedTempFile::new_in(path.parent().context("missing file parent")?)?;
    t.as_file()
        .set_permissions(fs::metadata(path)?.permissions())?;
    t.write_all(data)?;
    t.persist(path)?;
    Ok(())
}

fn validate_packages(work: &Path, packages: &BTreeMap<String, PathBuf>) -> Result<()> {
    ensure!(
        packages.len() == 4,
        "the reviewed four mobile packages changed"
    );
    for (name, path) in packages {
        let module = name
            .strip_prefix("@t3tools/")
            .context("unsupported mobile package name")?;
        ensure!(
            !module.is_empty() && !module.contains('/') && module != "." && module != "..",
            "invalid mobile package name"
        );
        let src = direct(work, &path.join("package.json"))?;
        let dest = work
            .join("apps/mobile/node_modules")
            .join(name)
            .join("package.json")
            .canonicalize()?;
        let a = src.metadata()?;
        let b = dest.metadata()?;
        ensure!(
            dest.starts_with(work) && a.is_file() && (a.dev(), a.ino()) == (b.dev(), b.ino()),
            "missing checkout-local source hardlink for {name}"
        );
    }
    let registry = work.join("node_modules/knip/package.json").canonicalize()?;
    ensure!(
        registry.starts_with(work) && registry.metadata()?.dev() == work.metadata()?.dev(),
        "image does not own its registry packages"
    );
    Ok(())
}

fn find_packages(work: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mobile = json(&direct(work, Path::new("apps/mobile/package.json"))?)?;
    let mut packages = BTreeMap::new();
    for (name, spec) in mobile["dependencies"]
        .as_object()
        .context("missing mobile dependencies")?
    {
        if let Some(path) = spec
            .as_str()
            .and_then(|s| s.strip_prefix("file:./modules/"))
        {
            packages.insert(name.clone(), Path::new("apps/mobile/modules").join(path));
        }
    }
    validate_packages(work, &packages)?;
    Ok(packages)
}

// Keep no env bytes or source checkout env links in a reusable image. On
// activation create fresh links only to the current project owner's real files.
fn env_files(work: &Path, project: Option<&Path>) -> Result<()> {
    for name in ENV_FILES {
        let target = work.join(name);
        let parent = target.parent().unwrap();
        ensure!(parent.canonicalize()? == parent, "unsafe env directory");
        match fs::symlink_metadata(&target) {
            Ok(m) if m.is_symlink() && project.is_none() => fs::remove_file(&target)?,
            Ok(_) => bail!("unexpected existing env file in private image: {name}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        if let Some(project) = project {
            let source = project.join(name);
            if !source.is_symlink() && source.is_file() {
                symlink(source, target)?;
            }
        }
    }
    Ok(())
}

fn member_roots(work: &Path, state: &Value) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    ensure!(
        state["pnpmfiles"] == serde_json::json!([work.join(".pnpmfile.cjs")]),
        "unexpected pnpm configuration"
    );
    let mut projects = vec![];
    let mut roots = vec![];
    for name in state["projects"]
        .as_object()
        .context("missing workspace projects")?
        .keys()
    {
        let rel = Path::new(name).strip_prefix(work)?.to_owned();
        let deps = direct(work, &rel.join("node_modules"))?;
        if deps.is_dir() {
            roots.push(deps);
        }
        projects.push(rel);
    }
    ensure!(
        projects.iter().any(|p| p.as_os_str().is_empty()),
        "missing root workspace importer"
    );
    Ok((projects, roots))
}

fn hardlink_groups(work: &Path) -> Result<Vec<Vec<PathBuf>>> {
    let mut groups: HashMap<(u64, u64), (u64, Vec<PathBuf>)> = HashMap::new();
    for entry in WalkDir::new(work).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let m = entry.metadata()?;
        if m.nlink() > 1 {
            groups
                .entry((m.dev(), m.ino()))
                .or_insert_with(|| (m.nlink(), vec![]))
                .1
                .push(entry.path().strip_prefix(work)?.to_owned());
        }
    }
    let mut result = vec![];
    for (expected, mut members) in groups.into_values() {
        ensure!(
            members.len() as u64 == expected,
            "image contains a file hardlinked outside its payload"
        );
        members.sort();
        result.push(members);
    }
    result.sort();
    Ok(result)
}

// Search binary data too: unknown native artifacts embedding the prepare
// location cannot be safely patched. Reuse a bounded buffer for large files.
fn contains(path: &Path, finder: &memchr::memmem::Finder<'_>, buf: &mut [u8]) -> Result<bool> {
    let mut file = File::open(path)?;
    let mut kept = 0;
    loop {
        let n = file.read(&mut buf[kept..])?;
        if n == 0 {
            return Ok(false);
        }
        let end = kept + n;
        if finder.find(&buf[..end]).is_some() {
            return Ok(true);
        }
        kept = end.min(finder.needle().len() - 1);
        buf.copy_within(end - kept..end, 0);
    }
}

fn generated(work: &Path, roots: &[PathBuf]) -> Result<BTreeMap<PathBuf, String>> {
    let finder = memchr::memmem::Finder::new(work.as_os_str().as_encoded_bytes());
    let mut buf = vec![0; 128 * 1024 + finder.needle().len()];
    let mut files = BTreeMap::new();
    for root in roots {
        for entry in WalkDir::new(root).follow_links(false) {
            let entry = entry?;
            let p = entry.path();
            if entry.file_type().is_symlink() {
                ensure!(
                    !fs::read_link(p)?.is_absolute() && p.canonicalize()?.starts_with(work),
                    "dependency link escapes private image: {}",
                    p.display()
                );
            } else if entry.file_type().is_file() {
                let rel = p.strip_prefix(work)?;
                let metadata = rel == Path::new(STATE) || rel == Path::new(MODULES);
                if !metadata && !contains(p, &finder, &mut buf)? {
                    continue;
                }
                ensure!(
                    metadata || rel.components().any(|part| part.as_os_str() == ".bin"),
                    "unrecognized seed path embedded in {}",
                    rel.display()
                );
                let data = fs::read(p)?;
                ensure!(
                    !data.contains(&0) && std::str::from_utf8(&data).is_ok(),
                    "binary generated metadata {}",
                    rel.display()
                );
                files.insert(rel.to_owned(), hash(&data));
            }
        }
    }
    ensure!(
        files.contains_key(Path::new(STATE)) && files.contains_key(Path::new(MODULES)),
        "incomplete pnpm metadata"
    );
    Ok(files)
}

pub(crate) fn install_and_seal(work: &Path, project_root: &Path, commit: &str) -> Result<()> {
    let work = work.canonicalize()?;
    let project = project_root.canonicalize()?;
    ensure!(
        commit == PIN && output(&work, "git", &["rev-parse", "HEAD"])? == PIN,
        "image --t3 supports only the audited T3 commit {PIN}"
    );
    ensure!(
        hash(&fs::read(direct(&work, Path::new(HOOK))?)?) == HOOK_SHA,
        "T3 frozen setup script differs from the audited version"
    );
    ensure!(
        !work
            .to_str()
            .context("pnpm requires UTF-8 paths")?
            .contains(['\n', '\r']),
        "invalid pnpm seed path"
    );
    let tools = activation_toolchain(&work)?;
    let manifest = json(&direct(&work, Path::new("package.json"))?)?;
    ensure!(
        manifest["packageManager"] == format!("pnpm@{}", tools.pnpm)
            && tools.node.first()
                == Some(&format!(
                    "v{}",
                    manifest["engines"]["node"]
                        .as_str()
                        .context("expected pinned Node")?
                )),
        "T3 requires its pinned Node and pnpm versions"
    );
    let host_store = PathBuf::from(output(&project, "pnpm", &["store", "path"])?).canonicalize()?;
    ensure!(
        host_store.file_name() == Some(OsStr::new("v11"))
            && host_store.is_dir()
            && host_store.metadata()?.dev() != work.metadata()?.dev(),
        "expected host pnpm v11 content store on another volume"
    );
    let config = tempfile::tempdir()?;
    let dir = config.path().join("pnpm");
    fs::create_dir(&dir)?;
    fs::write(
        dir.join("config.yaml"),
        format!(
            "extendNodePath: false\nstoreDir: {}\n",
            serde_json::to_string(host_store.parent().context("invalid store")?)?
        ),
    )?;
    let mut install = command("node", &work);
    for (key, _) in std::env::vars_os() {
        let keytext = key.to_string_lossy().to_ascii_uppercase();
        if keytext.starts_with("PNPM_CONFIG_") || keytext.starts_with("NPM_CONFIG_") {
            install.env_remove(key);
        }
    }
    // The hook's env-link step sees an empty private project root. Env files
    // from the user's source checkout are never copied/read during seed prep.
    let empty_project = tempfile::tempdir()?;
    let status = install
        .arg(HOOK)
        .env("T3CODE_PROJECT_ROOT", empty_project.path())
        .env("T3CODE_WORKTREE_PATH", &work)
        .env("XDG_CONFIG_HOME", config.path())
        .env("PNPM_CONFIG_VIRTUAL_STORE_TYPE", "project")
        .stdout(Stdio::from(std::io::stderr()))
        .status()?;
    ensure!(status.success(), "T3 frozen setup exited with {status}");
    seal(&work, commit, tools, &host_store)
}

fn seal(work: &Path, commit: &str, tools: Toolchain, host_store: &Path) -> Result<()> {
    let done = direct(work, Path::new(DONE))?;
    ensure!(
        done.is_file()
            && done.metadata()?.len() == 0
            && fs::read_dir(work.join(".t3"))?.count() == 1,
        "expected successful fresh T3 setup"
    );
    let modules = json(&direct(work, Path::new(MODULES))?)?;
    let state = json(&direct(work, Path::new(STATE))?)?;
    ensure!(
        modules["virtualStoreDir"] == ".pnpm"
            && modules["nodeLinker"] == "isolated"
            && modules["packageManager"] == format!("pnpm@{}", tools.pnpm)
            && modules["storeDir"].as_str().map(Path::new) == Some(host_store)
            && state["settings"]["enableGlobalVirtualStore"] != true,
        "expected private project-local pnpm layout"
    );
    let (projects, roots) = member_roots(work, &state)?;
    let mut inputs = BTreeMap::new();
    for path in projects.iter().map(|p| p.join("package.json")).chain(
        [
            HOOK,
            "pnpm-lock.yaml",
            "pnpm-workspace.yaml",
            ".pnpmfile.cjs",
        ]
        .map(PathBuf::from),
    ) {
        inputs.insert(path.clone(), hash(&fs::read(direct(work, &path)?)?));
    }
    env_files(work, None)?;
    let plan = Plan {
        schema: 1,
        origin: work.to_owned(),
        commit: commit.to_owned(),
        tools,
        inputs,
        packages: find_packages(work)?,
        hardlinks: hardlink_groups(work)?,
        generated: generated(work, &roots)?,
    };
    let dest = direct(work, Path::new(PLAN))?;
    let mut f = File::create_new(dest)?;
    serde_json::to_writer(&mut f, &plan)?;
    fs::remove_file(done)?;
    Ok(())
}

fn load(work: &Path) -> Result<Plan> {
    let plan: Plan = serde_json::from_reader(File::open(direct(work, Path::new(PLAN))?)?)?;
    ensure!(
        plan.schema == 1 && plan.origin.is_absolute(),
        "invalid image install plan"
    );
    Ok(plan)
}

pub(crate) fn repair_payload_links(original: &Path, copy: &Path) -> Result<()> {
    let original = original.canonicalize()?;
    let copy = copy.canonicalize()?;
    ensure!(
        original != copy && !copy.starts_with(&original) && !original.starts_with(&copy),
        "distinct payloads required"
    );
    for group in load(&original)?.hardlinks {
        ensure!(group.len() >= 2, "invalid hardlink group");
        let first = direct(&copy, &group[0])?;
        let src = direct(&original, &group[0])?.metadata()?;
        let base = first.metadata()?;
        ensure!(
            src.is_file() && base.is_file() && (src.dev(), src.ino()) != (base.dev(), base.ino()),
            "copy aliases original payload"
        );
        for name in &group[1..] {
            let dest = direct(&copy, name)?;
            let m = dest.metadata()?;
            ensure!(
                m.is_file()
                    && m.dev() == base.dev()
                    && (m.dev(), m.ino()) != (src.dev(), src.ino()),
                "invalid cloned hardlink"
            );
            if m.ino() != base.ino() {
                fs::remove_file(&dest)?;
                fs::hard_link(&first, &dest)?;
            }
        }
    }
    Ok(())
}

fn convert_paths(v: &mut Value, old: &Path, new: &Path) -> Result<()> {
    fn convert(s: &str, old: &Path, new: &Path) -> Result<String> {
        match Path::new(s).strip_prefix(old) {
            Ok(rest) => Ok(if rest.as_os_str().is_empty() {
                new.to_owned()
            } else {
                new.join(rest)
            }
            .to_str()
            .context("pnpm requires UTF-8 workspace paths")?
            .to_owned()),
            Err(_) => Ok(s.to_owned()),
        }
    }
    match v {
        Value::String(s) => *s = convert(s, old, new)?,
        Value::Array(a) => {
            for child in a {
                convert_paths(child, old, new)?;
            }
        }
        Value::Object(map) => {
            for (k, mut child) in std::mem::take(map) {
                convert_paths(&mut child, old, new)?;
                ensure!(
                    map.insert(convert(&k, old, new)?, child).is_none(),
                    "workspace key collision"
                );
            }
        }
        _ => (),
    }
    Ok(())
}

pub(crate) fn activate_with(
    work: &Path,
    project_root: &Path,
    commit: &str,
    tools: &Toolchain,
) -> Result<()> {
    let work = work.canonicalize()?;
    ensure!(
        fs::symlink_metadata(work.join(".git"))?.is_file()
            && output(&work, "git", &["rev-parse", "HEAD"])? == commit,
        "wrong linked checkout for image activation"
    );
    activate_payload(&work, &project_root.canonicalize()?, commit, tools)
}

fn activate_payload(work: &Path, project: &Path, commit: &str, tools: &Toolchain) -> Result<()> {
    let plan = load(work)?;
    ensure!(
        plan.commit == commit && plan.tools == *tools,
        "image commit or package toolchain changed"
    );
    for marker in ["setup-done", "setup-running", "setup-failed"] {
        ensure!(
            fs::symlink_metadata(work.join(".t3").join(marker))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "conflicting T3 setup marker"
        );
    }
    for (rel, digest) in &plan.inputs {
        ensure!(
            hash(&fs::read(direct(work, rel)?)?) == *digest,
            "image install input changed: {}",
            rel.display()
        );
    }
    validate_packages(work, &plan.packages)?;
    let old = plan.origin.to_str().context("non-UTF8 image origin")?;
    let new = work.to_str().context("non-UTF8 image checkout")?;
    // This is a fresh ephemeral clone. Its caller discards it on any failure;
    // never publish a completion marker until all rebasing and validation pass.
    for (rel, digest) in &plan.generated {
        let path = direct(work, rel)?;
        let data = fs::read(&path)?;
        ensure!(
            hash(&data) == *digest,
            "image install metadata changed: {}",
            rel.display()
        );
        if rel == Path::new(STATE) || rel == Path::new(MODULES) {
            let mut doc = serde_json::from_slice(&data)?;
            convert_paths(&mut doc, &plan.origin, work)?;
            replace_file(&path, &serde_json::to_vec(&doc)?)?;
        } else {
            ensure!(
                rel.components().any(|c| c.as_os_str() == ".bin"),
                "unrecognized image metadata"
            );
            let shim = std::str::from_utf8(&data)?;
            // Only unquoted-safe substitutions can occur in executable shell
            // code. The pinned pnpm templates also include target paths in comments.
            let shell_safe = |s: &str| {
                s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
            };
            ensure!(
                !new.contains(['\n', '\r'])
                    && ((shell_safe(old) && shell_safe(new))
                        || shim
                            .lines()
                            .all(|line| !line.contains(old) || line.starts_with('#'))),
                "cannot safely relocate executable path in {}",
                rel.display()
            );
            replace_file(&path, shim.replace(old, new).as_bytes())?;
        }
    }
    member_roots(work, &json(&direct(work, Path::new(STATE))?)?)?;
    env_files(work, Some(project))?;
    fs::remove_file(direct(work, Path::new(PLAN))?)?;
    File::create_new(direct(work, Path::new(DONE))?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Toolchain {
        Toolchain {
            node: vec![
                "v26.12.1".into(),
                "arm64".into(),
                "test-abi".into(),
                "node".into(),
            ],
            pnpm: "12.14.0".into(),
            vp: "fixture".into(),
            kernel: "fixture-kernel".into(),
        }
    }

    fn put(path: &Path, data: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        project: PathBuf,
        seed: PathBuf,
        store: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let parent = temp.path().canonicalize().unwrap();
            let seed = parent.join("seed");
            let project = parent.join("source");
            let store = parent.join("host-store/v11");
            fs::create_dir_all(&store).unwrap();
            fs::create_dir_all(project.join("infra/relay")).unwrap();
            fs::create_dir_all(seed.join("infra/relay")).unwrap();
            put(&project.join(".env"), b"fixture value");
            put(&seed.join(DONE), b"");
            put(&seed.join(HOOK), b"fixture hook");
            for rel in [
                ".pnpmfile.cjs",
                "pnpm-lock.yaml",
                "pnpm-workspace.yaml",
                "package.json",
            ] {
                put(&seed.join(rel), b"{}");
            }
            fs::create_dir_all(seed.join("apps/mobile/node_modules/@t3tools")).unwrap();
            let mut local = serde_json::Map::new();
            for i in 0..4 {
                let name = format!("fixture-{i}");
                local.insert(
                    format!("@t3tools/{name}"),
                    Value::String(format!("file:./modules/{name}")),
                );
                let source = seed
                    .join("apps/mobile/modules")
                    .join(&name)
                    .join("package.json");
                let package = seed
                    .join("node_modules/.pnpm")
                    .join(&name)
                    .join("node_modules/@t3tools")
                    .join(&name);
                put(&source, name.as_bytes());
                fs::create_dir_all(&package).unwrap();
                fs::hard_link(&source, package.join("package.json")).unwrap();
                symlink(
                    format!("../../../../node_modules/.pnpm/{name}/node_modules/@t3tools/{name}"),
                    seed.join("apps/mobile/node_modules/@t3tools").join(name),
                )
                .unwrap();
            }
            put(
                &seed.join("apps/mobile/package.json"),
                &serde_json::to_vec(&serde_json::json!({"dependencies":local})).unwrap(),
            );
            put(
                &seed.join("node_modules/.pnpm/knip/node_modules/knip/package.json"),
                b"{}",
            );
            symlink(
                ".pnpm/knip/node_modules/knip",
                seed.join("node_modules/knip"),
            )
            .unwrap();
            put(
                &seed.join("node_modules/.bin/demo"),
                format!("#!/bin/sh\n# target {}/bin\n", seed.display()).as_bytes(),
            );
            put(&seed.join(STATE), &serde_json::to_vec(&serde_json::json!({"projects":{(seed.to_str().unwrap()): {},
                (seed.join("apps/mobile").to_str().unwrap()): {}}, "pnpmfiles":[seed.join(".pnpmfile.cjs")], "settings": {}})).unwrap());
            put(&seed.join(MODULES), &serde_json::to_vec(&serde_json::json!({"virtualStoreDir":".pnpm","nodeLinker":"isolated","packageManager":"pnpm@12.14.0",
                "storeDir":store})).unwrap());
            Self {
                _temp: temp,
                project,
                seed,
                store,
            }
        }

        fn seal(&self) -> Result<()> {
            seal(&self.seed, PIN, tools(), &self.store)
        }

        fn duplicate(&self) -> PathBuf {
            let copy = self.seed.with_file_name("private checkout");
            for entry in WalkDir::new(&self.seed).follow_links(false) {
                let entry = entry.unwrap();
                let p = entry.path();
                let out = copy.join(p.strip_prefix(&self.seed).unwrap());
                if entry.file_type().is_dir() {
                    fs::create_dir(&out).unwrap();
                } else if entry.file_type().is_symlink() {
                    symlink(fs::read_link(p).unwrap(), out).unwrap();
                } else {
                    fs::copy(p, out).unwrap();
                }
            }
            copy
        }
    }

    #[test]
    fn relocates_private_graph_and_rebinds_all_checkout_sources() {
        let f = Fixture::new();
        symlink(f.project.join(".env"), f.seed.join(".env")).unwrap();
        f.seal().unwrap();
        assert!(fs::symlink_metadata(f.seed.join(".env")).is_err());
        let copy = f.duplicate();
        assert!(activate_payload(&copy, &f.project, PIN, &tools()).is_err());
        repair_payload_links(&f.seed, &copy).unwrap();
        activate_payload(&copy, &f.project, PIN, &tools()).unwrap();
        assert!(copy.join(DONE).is_file());
        assert!(!copy.join(PLAN).exists());
        assert_eq!(
            fs::read_link(copy.join(".env")).unwrap(),
            f.project.join(".env")
        );
        assert!(
            json(&copy.join(STATE)).unwrap()["projects"]
                .get(copy.to_str().unwrap())
                .is_some()
        );
        assert!(
            fs::read_to_string(copy.join("node_modules/.bin/demo"))
                .unwrap()
                .contains(copy.to_str().unwrap())
        );
        let own = Path::new("apps/mobile/modules/fixture-0/package.json");
        fs::write(copy.join(own), b"edited").unwrap();
        assert_eq!(
            fs::read(copy.join("apps/mobile/node_modules/@t3tools/fixture-0/package.json"))
                .unwrap(),
            b"edited"
        );
        assert_ne!(fs::read(f.seed.join(own)).unwrap(), b"edited");
        fs::write(copy.join("node_modules/knip/package.json"), b"changed").unwrap();
        assert_eq!(
            fs::read(f.seed.join("node_modules/knip/package.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn rejects_external_data_unrecognized_paths_and_changed_inputs() {
        let f = Fixture::new();
        let p = f.seed.join("node_modules/incorrect");
        symlink(f.project.join(".env"), &p).unwrap();
        assert!(f.seal().is_err());
        fs::remove_file(&p).unwrap();
        fs::write(&p, format!("\0{}", f.seed.display())).unwrap();
        assert!(f.seal().is_err());
        fs::remove_file(&p).unwrap();
        fs::write(f.seed.join(".env"), b"must not ship").unwrap();
        assert!(f.seal().is_err());
        fs::remove_file(f.seed.join(".env")).unwrap();
        f.seal().unwrap();
        fs::write(f.seed.join("pnpm-lock.yaml"), b"changed").unwrap();
        assert!(activate_payload(&f.seed, &f.project, PIN, &tools()).is_err());
        assert!(!f.seed.join(DONE).exists());
    }
}
