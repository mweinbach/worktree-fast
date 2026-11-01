//! Single-use native worktrees for predictable bursts. Git owns claimed branches.
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, DirBuilder, File, OpenOptions},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
use tempfile::NamedTempFile;

const FILE: &str = ".worktree-fast-pool.json";
const USAGE: &str = "git worktree-fast pool prepare NEW_POOL N [--t3]\n\
git worktree-fast pool claim POOL\n\
git worktree-fast pool drain POOL\n\
git worktree-fast pool retire POOL";

#[derive(Serialize, Deserialize)]
struct Pool {
    root: PathBuf,
    root_id: [u64; 2],
    common: PathBuf,
    common_id: [u64; 2],
    base: String,
    prefix: String,
    t3: bool,
    published: bool,
    slots: Vec<Slot>,
}

#[derive(Serialize, Deserialize)]
struct Slot {
    id: [u64; 2],
    admin: PathBuf,
    admin_id: [u64; 2],
    ignored: String,
    claimed: bool,
}

fn id(path: &Path) -> Result<[u64; 2]> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir(),
        "expected a direct directory: {}",
        path.display()
    );
    Ok([m.dev(), m.ino()])
}

fn output(cmd: &mut Command) -> Result<Vec<u8>> {
    let out = cmd.output()?;
    ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr).trim_end()
    );
    Ok(out.stdout)
}

fn command(program: impl AsRef<OsStr>, dir: &Path) -> Command {
    let mut c = Command::new(program);
    c.current_dir(dir);
    for key in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
    ] {
        c.env_remove(key);
    }
    c
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(output(command("git", dir).args(args))?)?
        .strip_suffix('\n')
        .context("unexpected Git response")?
        .to_owned())
}

fn lock(path: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let m = f.metadata()?;
    ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "expected a private pool directory owned by this user"
    );
    FileExt::lock_exclusive(&f)?;
    ensure!(
        id(path)? == [m.dev(), m.ino()],
        "pool directory was replaced"
    );
    Ok(f)
}

fn save(pool: &Pool) -> Result<()> {
    let mut tmp = NamedTempFile::new_in(&pool.root)?;
    serde_json::to_writer(tmp.as_file_mut(), pool)?;
    tmp.persist(pool.root.join(FILE))?;
    Ok(())
}

fn load(path: &Path) -> Result<Pool> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.join(FILE))?;
    ensure!(f.metadata()?.is_file(), "invalid pool manifest");
    let p: Pool = serde_json::from_reader(f)?;
    ensure!(
        p.root == path
            && p.root_id == id(path)?
            && p.common_id == id(&p.common)?
            && p.common.canonicalize()? == p.common
            && p.slots.len() <= 64
            && p.prefix.len() == 20
            && p.prefix.bytes().all(|b| b.is_ascii_hexdigit()),
        "pool identity changed"
    );
    Ok(p)
}

impl Pool {
    fn work(&self, i: usize) -> PathBuf {
        self.root.join(format!("slot-{:04}", i + 1))
    }
    fn branch(&self, i: usize) -> String {
        format!("worktree-fast/pool-{}-{}", self.prefix, i + 1)
    }
}

fn inspect(p: &Pool, i: usize) -> Result<PathBuf> {
    let work = p.work(i);
    let pointer = work.join(".git");
    ensure!(
        work.canonicalize()? == work && fs::symlink_metadata(&pointer)?.is_file(),
        "checkout path changed"
    );
    let info = git(
        &work,
        &[
            "rev-parse",
            "--absolute-git-dir",
            "HEAD",
            "--symbolic-full-name",
            "HEAD",
        ],
    )?;
    let mut fields = info.lines();
    let admin = PathBuf::from(fields.next().context("missing Git admin path")?);
    ensure!(
        fields.next() == Some(&p.base)
            && fields.next() == Some(&format!("refs/heads/{}", p.branch(i)))
            && fields.next().is_none(),
        "pool checkout branch or commit changed: {}",
        work.display()
    );
    ensure!(
        admin.canonicalize()? == admin && admin.parent() == Some(&p.common.join("worktrees")),
        "foreign Git worktree"
    );
    let back = fs::read_to_string(admin.join("gitdir"))?;
    ensure!(
        admin.join(back.trim_end_matches('\n')).canonicalize()? == pointer,
        "wrong Git backlink"
    );
    Ok(admin)
}

// Git normally discards ignored data on removal. Refuse when ignored files
// were added, removed, replaced, or edited after this slot was provisioned.
fn ignored_fingerprint(work: &Path) -> Result<String> {
    let names = output(command("git", work).args([
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
    ]))?;
    let mut hash = Sha256::new();
    for name in names.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let m = fs::symlink_metadata(work.join(OsStr::from_bytes(name)))?;
        hash.update(name);
        hash.update([0]);
        for field in [
            m.dev(),
            m.ino(),
            m.len(),
            m.mode().into(),
            m.ctime() as u64,
            m.ctime_nsec() as u64,
        ] {
            hash.update(field.to_be_bytes());
        }
    }
    Ok(hex::encode(hash.finalize()))
}

fn prepare(root: &Path, count: usize, t3: bool) -> Result<()> {
    ensure!((1..=64).contains(&count), "pool capacity must be 1–64");
    let cwd = env::current_dir()?;
    let common = PathBuf::from(git(
        &cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)
    .canonicalize()?;
    let listed = output(command("git", &cwd).args(["worktree", "list", "--porcelain", "-z"]))?;
    let in_checkout = listed
        .split(|b| *b == 0)
        .filter_map(|s| s.strip_prefix(b"worktree "))
        .any(|s| root.starts_with(Path::new(OsStr::from_bytes(s))));
    ensure!(
        !root.starts_with(&common) && !in_checkout,
        "choose a new pool outside the repository"
    );
    let base = git(&cwd, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    DirBuilder::new().mode(0o700).create(root)?;
    let _guard = lock(root)?;
    let root_id = id(root)?;
    let mut digest = Sha256::new();
    digest.update(root.as_os_str().as_bytes());
    digest.update(root_id[1].to_be_bytes());
    digest.update(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
            .to_be_bytes(),
    );
    let mut p = Pool {
        root: root.to_owned(),
        root_id,
        common_id: id(&common)?,
        common,
        base,
        prefix: hex::encode(digest.finalize())[..20].to_owned(),
        t3,
        published: false,
        slots: Vec::new(),
    };
    save(&p)?;
    let exe = env::current_exe()?;
    for i in 0..count {
        let work = p.work(i);
        let result = output(
            command(&exe, &cwd)
                .args(["add", "-b", &p.branch(i)])
                .arg(&work)
                .arg(&p.base),
        );
        // A native post-checkout hook can fail after Git publishes a checkout.
        // Record it only if its reciprocal registration and exact branch match.
        if !work.try_exists()? {
            result?;
            bail!("Git did not create {}", work.display());
        }
        let admin = inspect(&p, i)?;
        p.slots.push(Slot {
            id: id(&work)?,
            admin_id: id(&admin)?,
            admin,
            ignored: ignored_fingerprint(&work)?,
            claimed: false,
        });
        save(&p)?;
        result?;
        if t3 {
            output(command(&exe, &cwd).arg("setup").arg(&work))?;
            p.slots[i].ignored = ignored_fingerprint(&work)?;
            save(&p)?;
        }
    }
    p.published = true;
    save(&p)
}

fn verify_slot(p: &Pool, i: usize) -> Result<()> {
    let slot = &p.slots[i];
    let admin = inspect(p, i)?;
    ensure!(
        id(&p.work(i))? == slot.id && slot.admin == admin && id(&admin)? == slot.admin_id,
        "slot was replaced"
    );
    Ok(())
}

fn claim(p: &mut Pool) -> Result<()> {
    ensure!(p.published, "pool is not published");
    let i = p
        .slots
        .iter()
        .position(|s| !s.claimed)
        .context("all pool slots have been claimed")?;
    verify_slot(p, i)?;
    let work = p.work(i);
    if p.t3 {
        ensure!(
            fs::symlink_metadata(work.join(".t3"))?.is_dir()
                && fs::symlink_metadata(work.join(".t3/setup-done"))
                    .is_ok_and(|m| m.is_file() && m.len() == 0)
                && !work.join(".t3/setup-running").exists()
                && !work.join(".t3/setup-failed").exists()
                && fs::symlink_metadata(work.join("node_modules/.modules.yaml"))
                    .is_ok_and(|m| m.is_file())
                && work.join("node_modules/.worktree-fast-store").is_dir(),
            "incomplete T3 setup"
        );
    }
    p.slots[i].claimed = true;
    save(p)?;
    println!("{}", work.display());
    Ok(())
}

fn drain(mut p: Pool) -> Result<()> {
    p.published = false;
    save(&p)?;
    for (i, slot) in p.slots.iter().enumerate().filter(|(_, s)| !s.claimed) {
        let work = p.work(i);
        if fs::symlink_metadata(&work).is_ok() {
            verify_slot(&p, i)?;
            ensure!(
                slot.ignored == ignored_fingerprint(&work)?,
                "ignored files changed: {}",
                work.display()
            );
            let index = output(command("git", &work).args(["ls-files", "-v", "-z"]))?;
            ensure!(
                index
                    .split(|b| *b == 0)
                    .all(|s| s.is_empty() || s.starts_with(b"H ")),
                "index contains unexamined files: {}",
                work.display()
            );
            // Git's own non-force command checks locked, dirty and untracked state.
            output(
                command("git", &p.common)
                    .args(["worktree", "remove", "--"])
                    .arg(&work),
            )?;
        }
        ensure!(
            !slot.admin.try_exists()?,
            "pool checkout moved or its registration remains: {}",
            work.display()
        );
        let branch = format!("refs/heads/{}", p.branch(i));
        // Never delete a branch that advanced, or one now used by another worktree.
        let listed =
            output(command("git", &p.common).args(["worktree", "list", "--porcelain", "-z"]))?;
        let line = format!("branch {branch}");
        if !listed.split(|b| *b == 0).any(|r| r == line.as_bytes()) {
            let got = output(command("git", &p.common).args([
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                &branch,
            ]))?;
            if got == format!("{branch} {}\n", p.base).as_bytes() {
                output(command("git", &p.common).args(["update-ref", "-d", &branch, &p.base]))?;
            }
        }
    }
    Ok(())
}

fn retire(p: &Pool) -> Result<()> {
    for item in fs::read_dir(&p.root)? {
        ensure!(
            item?.file_name() == FILE,
            "remove claimed worktrees and drain unclaimed slots before retiring"
        );
    }
    let listed = output(command("git", &p.common).args(["worktree", "list", "--porcelain", "-z"]))?;
    ensure!(
        !listed
            .split(|b| *b == 0)
            .filter_map(|s| s.strip_prefix(b"worktree "))
            .any(|s| Path::new(OsStr::from_bytes(s)).starts_with(&p.root)),
        "Git registration remains under the pool"
    );
    fs::remove_file(p.root.join(FILE))?;
    fs::remove_dir(&p.root)?;
    Ok(())
}

pub fn run(args: &[OsString]) -> Result<ExitCode> {
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let verb = args[0].to_str().unwrap_or("");
    let target = env::current_dir()?.join(args.get(1).context(USAGE)?);
    let root = target
        .parent()
        .context("expected a named directory")?
        .canonicalize()?
        .join(target.file_name().context("expected a named directory")?);
    if verb == "prepare" {
        ensure!(
            args.len() == 3 || (args.len() == 4 && args[3] == "--t3"),
            "{USAGE}"
        );
        let count = args[2].to_str().context("invalid capacity")?.parse()?;
        prepare(&root, count, args.len() == 4)?;
    } else {
        ensure!(args.len() == 2, "{USAGE}");
        let _guard = lock(&root)?;
        let mut pool = load(&root)?;
        match verb {
            "claim" => claim(&mut pool)?,
            "drain" => drain(pool)?,
            "retire" => retire(&pool)?,
            _ => bail!("{USAGE}"),
        }
    }
    Ok(ExitCode::SUCCESS)
}
