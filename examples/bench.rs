//! Time a native Git checkout and an APFS cache hit on the same local volume.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

const USAGE: &str = "bench <repo> --bin <git-worktree-fast> [--rounds 3] [--jobs 1..8] [--checkout-workers N] [--min-speedup FACTOR]";
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
            .env("WORKTREE_FAST_TRACE", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("XDG_CACHE_HOME", &self.cache)
            .stdin(Stdio::null());
        cmd
    }

    fn measure(
        &self,
        source: &Path,
        rounds: usize,
        jobs: usize,
        checkout_workers: Option<i32>,
    ) -> Result<(Value, f64)> {
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
        // Both native Git and the accelerator's child Git processes see this
        // setting, including while preparing the pristine checkout cache.
        if let Some(workers) = checkout_workers {
            run(self
                .command("git", &self.repo)
                .args(["config", "--local", "checkout.workers"])
                .arg(workers.to_string()))?;
        }
        let initial =
            run(self
                .command("git", &self.repo)
                .args(["worktree", "list", "--porcelain", "-z"]))?
            .stdout;
        let (_, prepare_s) = timed(
            self.command(&self.binary, &self.repo)
                .args(["prepare", &head]),
        )?;

        let paths: Vec<_> = (0..jobs)
            .map(|job| self.repo.with_file_name(format!("checkout-{job}")))
            .collect();
        let mut samples = Vec::new();
        let mut native_ready = Vec::new();
        let mut cached_ready = Vec::new();
        for round in 0..rounds {
            let order = if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            };
            for cached in order {
                let (added, add_s) = batch(&paths, |path| {
                    let mut cmd = if cached {
                        self.command(&self.binary, &self.repo)
                    } else {
                        let mut cmd = self.command("git", &self.repo);
                        cmd.arg("worktree");
                        cmd
                    };
                    cmd.args(["add", "--detach"]).arg(path).arg(&head);
                    cmd
                })?;
                ensure!(
                    !cached
                        || added
                            .iter()
                            .all(|out| out.stderr.split(|c| *c == b'\n').any(|line| line == HIT)),
                    "the accelerated command fell back to native Git; no APFS comparison was recorded"
                );
                let (statuses, first_status_s) = batch(&paths, |path| {
                    let mut cmd = self.command("git", path);
                    cmd.args(["status", "--porcelain=v1", "--untracked-files=all", "-z"]);
                    cmd
                })?;
                ensure!(
                    statuses.iter().all(|out| out.stdout.is_empty()),
                    "new checkout contains changes"
                );
                for path in &paths {
                    let checked_out =
                        output_text(run(self.command("git", path).args(["rev-parse", "HEAD"]))?)?;
                    ensure!(checked_out == head, "new checkout has unexpected HEAD");
                }
                let (_, remove_s) = batch(&paths, |path| {
                    let mut cmd = self.command("git", &self.repo);
                    cmd.args(["worktree", "remove", "--force", "--"]).arg(path);
                    cmd
                })?;
                for path in &paths {
                    ensure!(
                        !path.try_exists()?,
                        "Git left the disposable checkout on disk"
                    );
                }
                let current = run(self.command("git", &self.repo).args([
                    "worktree",
                    "list",
                    "--porcelain",
                    "-z",
                ]))?
                .stdout;
                ensure!(initial == current, "Git left a worktree registration");
                let add_and_status_s = add_s + first_status_s;
                if cached {
                    cached_ready.push(add_and_status_s);
                } else {
                    native_ready.push(add_and_status_s);
                }
                samples.push(json!({
                    "round": round + 1, "mode": if cached { "apfs" } else { "git" },
                    "add_s": add_s, "first_status_s": first_status_s, "remove_s": remove_s,
                    "add_and_status_s": add_and_status_s,
                    "lifecycle_s": add_and_status_s + remove_s
                }));
            }
        }
        let (_, forget_s) = timed(
            self.command(&self.binary, &self.repo)
                .args(["forget", &head]),
        )?;
        let git_median = median(native_ready)?;
        let apfs_median = median(cached_ready)?;
        let speedup = git_median / apfs_median;
        let report = json!({
            "schema": 1, "commit": head, "git": version, "rounds": rounds, "jobs": jobs,
            "checkout_workers": checkout_workers,
            "os": env::consts::OS, "arch": env::consts::ARCH,
            "workload": "tracked files, detached worktrees, no install",
            "timing_scope": "sum of batch phase wall times; excludes cache preparation, retirement, HEAD and registration checks",
            "median_add_and_status_s": { "git": git_median, "apfs": apfs_median },
            "speedup": speedup,
            "prepare_s": prepare_s, "forget_s": forget_s, "samples": samples
        });
        Ok((report, speedup))
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

fn batch(paths: &[PathBuf], command: impl Fn(&Path) -> Command) -> Result<(Vec<Output>, f64)> {
    let mut commands: Vec<_> = paths.iter().map(|p| command(p)).collect();
    if let [cmd] = commands.as_mut_slice() {
        let (out, secs) = timed(cmd)?;
        return Ok((vec![out], secs));
    }
    let start = Instant::now();
    let completed = std::thread::scope(|scope| {
        let workers: Vec<_> = commands
            .into_iter()
            .map(|mut cmd| scope.spawn(move || run(&mut cmd)))
            .collect();
        // Collect every joined outcome before propagating errors. No child can
        // remain writing to a checkout when its private fixture is cleaned up.
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("benchmark worker panicked")))
            })
            .collect::<Vec<_>>()
    });
    let secs = start.elapsed().as_secs_f64();
    Ok((completed.into_iter().collect::<Result<_>>()?, secs))
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

fn median(mut durations: Vec<f64>) -> Result<f64> {
    ensure!(!durations.is_empty(), "no timed samples");
    durations.sort_unstable_by(f64::total_cmp);
    let upper = durations.len() / 2;
    let lower = (durations.len() - 1) / 2;
    let seconds = durations[lower].midpoint(durations[upper]);
    ensure!(seconds > 0.0, "cannot compare a zero median time");
    Ok(seconds)
}

fn publish(
    mut report: Value,
    speedup: f64,
    minimum: Option<f64>,
    mut out: impl Write,
) -> Result<()> {
    report["min_speedup"] = json!(minimum);
    report["met_min_speedup"] = json!(minimum.map(|factor| speedup >= factor));
    writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
    if let Some(factor) = minimum {
        ensure!(
            speedup >= factor,
            "median checkout + first status speedup {speedup:.3}x is below target {factor}x"
        );
    }
    Ok(())
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
    let mut jobs = 1;
    let mut checkout_workers = None;
    let mut min_speedup: Option<f64> = None;
    while let Some(flag) = args.next() {
        let value = args.next().context(USAGE)?;
        match flag.to_str() {
            Some("--bin") => binary = Some(value),
            Some("--rounds") => rounds = value.to_str().context(USAGE)?.parse()?,
            Some("--jobs") => jobs = value.to_str().context(USAGE)?.parse()?,
            Some("--checkout-workers") => {
                checkout_workers = Some(value.to_str().context(USAGE)?.parse()?)
            }
            Some("--min-speedup") => {
                let factor: f64 = value.to_str().context(USAGE)?.parse()?;
                ensure!(
                    factor.is_finite() && factor > 0.0,
                    "minimum speedup must be positive and finite"
                );
                min_speedup = Some(factor);
            }
            _ => bail!(USAGE),
        }
    }
    ensure!(rounds > 0, "rounds must be positive");
    ensure!((1..=8).contains(&jobs), "jobs must be between 1 and 8");
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
    .measure(&source, rounds, jobs, checkout_workers);
    tmp.close()
        .context("remove private benchmark repository and cache")?;
    let (report, speedup) = result?;
    publish(report, speedup, min_speedup, io::stdout().lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn medians_handle_order_and_outliers() {
        assert_eq!(median(vec![8.0, 1.0, 90.0, 4.0]).unwrap(), 6.0);
        assert_eq!(median(vec![90.0, 1.0, 4.0]).unwrap(), 4.0);
        assert_eq!(median(vec![7.0]).unwrap(), 7.0);
    }

    #[test]
    fn reports_results_on_either_side_of_the_requested_floor() {
        for (minimum, passed) in [
            (None, None),
            (Some(1.5), Some(true)),
            (Some(2.0), Some(false)),
        ] {
            let mut out = Vec::new();
            let result = publish(json!({"speedup": 1.5}), 1.5, minimum, &mut out);
            assert_eq!(result.is_ok(), passed.unwrap_or(true));
            let report: Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(report["min_speedup"], json!(minimum));
            assert_eq!(report["met_min_speedup"], json!(passed));
            assert_eq!(report["speedup"], 1.5);
        }
    }
}
