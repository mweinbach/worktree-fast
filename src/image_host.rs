use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(super) type Id = (u64, u64);
pub(super) type Inventory = BTreeMap<String, Id>;

pub(super) fn identity(path: &Path) -> Result<Id> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        !m.is_symlink(),
        "indirect managed object: {}",
        path.display()
    );
    Ok((m.dev(), m.ino()))
}

pub(super) fn entries(path: &Path) -> Result<Inventory> {
    fs::read_dir(path)?
        .map(|e| {
            let e = e?;
            Ok((
                e.file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("unknown non-Unicode path at image root"))?,
                identity(&e.path())?,
            ))
        })
        .collect()
}

pub(super) fn new_dir(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().context("expected a new directory name")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    let path = parent.join(name);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .with_context(|| format!("create new private directory {}", path.display()))?;
    Ok(path)
}

pub(super) fn lock(path: &Path, exclusive: bool) -> Result<(PathBuf, File)> {
    let path = path.absolute_path()?;
    let fd = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&path)?;
    let m = fd.metadata()?;
    // SAFETY: geteuid has no preconditions and returns the calling process' effective uid.
    ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "expected a private user-owned directory"
    );
    if exclusive {
        fs2::FileExt::lock_exclusive(&fd)?;
    } else {
        fs2::FileExt::lock_shared(&fd)?;
    }
    ensure!(
        identity(&path)? == (m.dev(), m.ino()),
        "directory was replaced"
    );
    Ok((path, fd))
}

trait DirectPath {
    fn absolute_path(&self) -> Result<PathBuf>;
}
impl DirectPath for Path {
    fn absolute_path(&self) -> Result<PathBuf> {
        ensure!(
            !fs::symlink_metadata(self)?.is_symlink(),
            "refusing a symlink: {}",
            self.display()
        );
        Ok(self.canonicalize()?)
    }
}

pub(super) fn read<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(f.metadata()?.is_file(), "expected a regular manifest");
    Ok(serde_json::from_reader(f)?)
}

pub(super) fn save<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().context("manifest parent")?)?;
    serde_json::to_writer(&mut tmp, v)?;
    tmp.flush()?;
    tmp.persist(path)?;
    Ok(())
}

pub(super) fn git_cmd(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(repo);
    for (key, _) in std::env::vars_os() {
        // Ignore caller worktree/index overrides. Preserve config and
        // identity variables so Git sees the same policies and hooks.
        if matches!(
            key.to_str(),
            Some(
                "GIT_DIR"
                    | "GIT_COMMON_DIR"
                    | "GIT_WORK_TREE"
                    | "GIT_INDEX_FILE"
                    | "GIT_PREFIX"
                    | "GIT_INTERNAL_SUPER_PREFIX"
            )
        ) {
            c.env_remove(key);
        }
    }
    c.env("GIT_TERMINAL_PROMPT", "0");
    c
}
pub(super) fn checked(cmd: &mut Command) -> Result<Vec<u8>> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("starting {cmd:?}"))?;
    if !out.status.success() {
        bail!(
            "{}: {}",
            cmd.get_program().to_string_lossy(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}
pub(super) fn git<I, S>(repo: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let bytes = checked(git_cmd(repo).args(args))?;
    Ok(String::from_utf8(bytes)?.trim_end_matches('\n').to_string())
}
fn hdi(args: &[&OsStr]) -> Result<Vec<u8>> {
    checked(Command::new("/usr/bin/hdiutil").args(args))
}
fn key<'a>(v: &'a plist::Value, name: &str) -> Result<&'a plist::Value> {
    v.as_dictionary()
        .and_then(|d| d.get(name))
        .with_context(|| format!("missing hdiutil field {name}"))
}
fn points(info: &plist::Value) -> Result<(Vec<PathBuf>, Option<String>)> {
    let entities = key(info, "system-entities")?
        .as_array()
        .context("missing image device list")?;
    let mut paths = Vec::new();
    let mut dev = None;
    for e in entities {
        let d = e.as_dictionary().context("invalid image device")?;
        if let Some(s) = d.get("mount-point").and_then(plist::Value::as_string) {
            paths.push(PathBuf::from(s));
        }
        if dev.is_none() {
            dev = d
                .get("dev-entry")
                .and_then(plist::Value::as_string)
                .map(str::to_string);
        }
    }
    Ok((paths, dev))
}
pub(super) fn attached(backing: &Path) -> Result<Option<plist::Value>> {
    let plist =
        plist::Value::from_reader(Cursor::new(hdi(&["info".as_ref(), "-plist".as_ref()])?))?;
    let images = key(&plist, "images")?
        .as_array()
        .context("invalid hdiutil image list")?;
    let mut found = None;
    for image in images {
        let path = key(image, "image-path")
            .ok()
            .and_then(plist::Value::as_string)
            .map(Path::new);
        if path.is_some_and(|p| p == backing || fs::canonicalize(p).is_ok_and(|p| p == backing)) {
            ensure!(found.is_none(), "backing image is attached multiple times");
            found = Some(image.clone());
        }
    }
    Ok(found)
}
pub(super) fn mounted(mount: &Path) -> Result<bool> {
    if !mount.try_exists()? {
        return Ok(false);
    }
    Ok(identity(mount)?.0 != identity(mount.parent().context("mountpoint parent")?)?.0)
}
pub(super) fn format(backing: &Path, size: &str) -> Result<()> {
    hdi(&[
        "create".as_ref(),
        "-type".as_ref(),
        "SPARSE".as_ref(),
        "-size".as_ref(),
        size.as_ref(),
        "-fs".as_ref(),
        "APFS".as_ref(),
        "-volname".as_ref(),
        "WorktreeFast".as_ref(),
        "-nospotlight".as_ref(),
        backing.as_os_str(),
    ])?;
    Ok(())
}
pub(super) fn attach(backing: &Path, mount: &Path) -> Result<()> {
    fs::create_dir(mount)?;
    let data = hdi(&[
        "attach".as_ref(),
        "-plist".as_ref(),
        "-nobrowse".as_ref(),
        "-noautoopen".as_ref(),
        "-noverify".as_ref(),
        "-noautofsck".as_ref(),
        "-mount".as_ref(),
        "required".as_ref(),
        "-mountpoint".as_ref(),
        mount.as_os_str(),
        backing.as_os_str(),
    ])?;
    let p = plist::Value::from_reader(Cursor::new(data))?;
    ensure!(
        mounted(mount)? && points(&p)?.0 == [mount],
        "unexpected image mountpoint"
    );
    Ok(())
}
pub(super) fn verify_mount(backing: &Path, mount: &Path) -> Result<()> {
    let image = attached(backing)?.context("image is no longer attached")?;
    ensure!(
        mounted(mount)? && points(&image)?.0 == [mount],
        "mountpoint belongs to a different image"
    );
    Ok(())
}
fn no_nested_mounts(root: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: getmntinfo initializes the borrowed immutable Mount table on
    // success. This synchronous subcommand never calls it from another thread.
    let mut table = std::ptr::null_mut();
    let count = unsafe { libc::getmntinfo(&mut table, libc::MNT_NOWAIT) };
    ensure!(
        count > 0 && !table.is_null(),
        "cannot inspect filesystem mounts"
    );
    let mounts = unsafe { std::slice::from_raw_parts(table, count as usize) };
    for volume in mounts {
        // f_mntonname is a kernel-populated NUL-terminated array.
        let name = unsafe { std::ffi::CStr::from_ptr(volume.f_mntonname.as_ptr()) };
        let path = Path::new(OsStr::from_bytes(name.to_bytes()));
        ensure!(
            path == root || !path.starts_with(root),
            "another filesystem is mounted below {}",
            root.display()
        );
    }
    Ok(())
}
pub(super) fn detach(backing: &Path, mount: &Path) -> Result<()> {
    if let Some(image) = attached(backing)? {
        let (paths, dev) = points(&image)?;
        if paths.is_empty() {
            let dev = dev.context("missing image device")?;
            hdi(&["detach".as_ref(), dev.as_ref()])?;
        } else {
            ensure!(
                paths == [mount] && mounted(mount)?,
                "image mounted at an unexpected location"
            );
            no_nested_mounts(mount)?;
            hdi(&["detach".as_ref(), mount.as_os_str()])?;
        }
    }
    ensure!(
        !mounted(mount)?,
        "unexpected mount remains: {}",
        mount.display()
    );
    Ok(())
}
pub(super) fn remove_empty_mount(mount: &Path) -> Result<()> {
    if mount.try_exists()? {
        ensure!(!mounted(mount)?, "refusing to remove a mounted directory");
        fs::remove_dir(mount)?;
    }
    Ok(())
}
pub(super) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
