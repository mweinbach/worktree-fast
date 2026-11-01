//! Time a native Git checkout and an APFS cache hit on the same local volume.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

const USAGE: &str = "bench <repo> --bin <git-worktree-fast> [--rounds 3]";
const HIT: &[u8] = b"git-worktree-fast: created from APFS checkout cache";

struct Benchmark {
    cache: PathBuf,
    repo: PathBuf,
    binary: PathBuf,
}

impl Benchmark {
    fn command(&self, program: impl AsRef<OsStr>, cwd: &Path) -> Command {
        let mut cmd = Command::new(program);
        // Measure the same default Git configuration even when launched from a hook.
        for (key, _) in env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        cmd.current_dir(cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("XDG_CACHE_HOME", &self.cache)
            .stdin(Stdio::null());
        cmd
    }

    fn measure(&self, source: &Path, rounds: usize) -> Result<Value> {
        let head = output_text(run(self.command("git", source).args([
            "rev-parse",
            "--verify",
            "HEAD^{commit}",
        ]))?)?;
        let version = output_text(run(self.command("git", source).arg("--version"))?)?;
        let mut common = run(self.command("git", source).args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]))?
        .stdout;
        if common.last() == Some(&b'\n') {
            common.pop();
        }
        // A separate repo owns registrations/refs and borrows only Git objects.
        // Both worktree implementations use it; neither can change the source's refs.
        run(self
            .command("git", source)
            .args([
                "clone",
                "--no-checkout",
                "--shared",
                "--template=",
                "--no-tags",
                "--",
            ])
            .arg(OsString::from_vec(common))
            .arg(&self.repo))?;
        let initial =
            run(self
                .command("git", &self.repo)
                .args(["worktree", "list", "--porcelain", "-z"]))?
            .stdout;
        let (_, prepare_s) = timed(
            self.command(&self.binary, &self.repo)
                .args(["prepare", &head]),
        )?;

        let mut samples = Vec::new();
        for round in 0..rounds {
            let order = if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            };
            for cached in order {
                let path = self.repo.with_file_name("checkout");
                let mut add = if cached {
                    self.command(&self.binary, &self.repo)
                } else {
                    let mut cmd = self.command("git", &self.repo);
                    cmd.arg("worktree");
                    cmd
                };
                let (added, add_s) = timed(add.args(["add", "--detach"]).arg(&path).arg(&head))?;
                ensure!(
                    !cached || added.stderr.split(|c| *c == b'\n').any(|line| line == HIT),
                    "the accelerated command fell back to native Git; no APFS comparison was recorded"
                );
                let (status, first_status_s) = timed(self.command("git", &path).args([
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=all",
                    "-z",
                ]))?;
                ensure!(status.stdout.is_empty(), "new checkout contains changes");
                let checked_out =
                    output_text(run(self.command("git", &path).args(["rev-parse", "HEAD"]))?)?;
                ensure!(checked_out == head, "new checkout has unexpected HEAD");
                let (_, remove_s) = timed(
                    self.command("git", &self.repo)
                        .args(["worktree", "remove", "--force", "--"])
                        .arg(&path),
                )?;
                ensure!(
                    !path.try_exists()?,
                    "Git left the disposable checkout on disk"
                );
                let current = run(self.command("git", &self.repo).args([
                    "worktree",
                    "list",
                    "--porcelain",
                    "-z",
                ]))?
                .stdout;
                ensure!(initial == current, "Git left a worktree registration");
                samples.push(json!({
                    "round": round + 1, "mode": if cached { "apfs" } else { "git" },
                    "add_s": add_s, "first_status_s": first_status_s, "remove_s": remove_s
                }));
            }
        }
        let (_, forget_s) = timed(
            self.command(&self.binary, &self.repo)
                .args(["forget", &head]),
        )?;
        Ok(json!({
            "schema": 1, "commit": head, "git": version, "rounds": rounds,
            "os": env::consts::OS, "arch": env::consts::ARCH,
            "workload": "tracked files, detached worktrees, no install",
            "prepare_s": prepare_s, "forget_s": forget_s, "samples": samples
        }))
    }
}

fn run(cmd: &mut Command) -> Result<Output> {
    let output = cmd
        .output()
        .with_context(|| format!("could not launch {cmd:?}"))?;
    if !output.status.success() {
        bail!(
            "{:?} exited with {}: {}",
            cmd.get_program(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    Ok(output)
}

fn timed(cmd: &mut Command) -> Result<(Output, f64)> {
    let start = Instant::now();
    let output = run(cmd)?;
    Ok((output, start.elapsed().as_secs_f64()))
}

fn output_text(output: Output) -> Result<String> {
    Ok(String::from_utf8(output.stdout)?
        .trim_end_matches('\n')
        .to_owned())
}

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let repo = args.next().context(USAGE)?;
    if repo == "--help" || repo == "-h" {
        println!("{USAGE}");
        return Ok(());
    }
    let mut binary: Option<OsString> = None;
    let mut rounds = 3;
    while let Some(flag) = args.next() {
        let value = args.next().context(USAGE)?;
        match flag.to_str() {
            Some("--bin") => binary = Some(value),
            Some("--rounds") => rounds = value.to_str().context(USAGE)?.parse()?,
            _ => bail!(USAGE),
        }
    }
    ensure!(rounds > 0, "rounds must be positive");
    let binary = PathBuf::from(binary.context(USAGE)?)
        .canonicalize()
        .context("locate CLI binary")?;
    let source = PathBuf::from(repo)
        .canonicalize()
        .context("locate source repository")?;
    let parent = source
        .parent()
        .context("source must have a sibling directory")?;
    ensure!(
        source.metadata()?.dev() == parent.metadata()?.dev(),
        "the repository needs a writable sibling directory on its own volume"
    );
    let tmp = tempfile::Builder::new()
        .prefix(".worktree-fast-bench-")
        .tempdir_in(parent)?;
    let result = Benchmark {
        cache: tmp.path().join("cache"),
        repo: tmp.path().join("repo"),
        binary,
    }
    .measure(&source, rounds);
    tmp.close()
        .context("remove private benchmark repository and cache")?;
    println!("{}", serde_json::to_string_pretty(&result?)?);
    Ok(())
}
