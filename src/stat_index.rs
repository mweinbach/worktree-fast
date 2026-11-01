//! Give a verified APFS clone a normal Git index with its own stat data.
//! The caller authenticates the Git-generated template and the cloned tree
//! before priming. Never use this to mark an arbitrary working tree clean.
use anyhow::{Context, Result, bail, ensure};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempPath;

fn checksum(bytes: &[u8], hash_len: usize) -> Result<Vec<u8>> {
    match hash_len {
        20 => Ok(Sha1::digest(bytes).to_vec()),
        32 => Ok(Sha256::digest(bytes).to_vec()),
        _ => bail!("unsupported Git index object format"),
    }
}

fn uint32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

fn valid_name(name: &[u8]) -> bool {
    name.split(|&c| c == b'/').all(|part| {
        !part.is_empty() && part != b"." && part != b".." && !part.eq_ignore_ascii_case(b".git")
    })
}

// Git's v2 format stores 10 big-endian u32 stat fields, the object ID, flags,
// a NUL-terminated byte path, and padding to an 8-byte entry boundary.
fn filled(template: &[u8], payload: &Path, hash_len: usize) -> Result<(Vec<u8>, i64)> {
    let end = template
        .len()
        .checked_sub(hash_len)
        .context("truncated Git index")?;
    ensure!(
        end >= 12 && &template[..8] == b"DIRC\0\0\0\x02",
        "expected a Git v2 index"
    );
    ensure!(
        checksum(&template[..end], hash_len)? == template[end..],
        "Git template checksum mismatch"
    );
    let count = uint32(&template[8..12]);
    let fixed = 40 + hash_len + 2;
    let mut index = template.to_vec();
    let mut pos = 12;
    let mut newest_ctime = 0;
    let mut inodes = HashSet::new();
    for _ in 0..count {
        let entry = &mut index[pos..end];
        ensure!(entry.len() > fixed, "truncated Git index entry");
        let flags = u16::from_be_bytes([entry[fixed - 2], entry[fixed - 1]]);
        let length = memchr::memchr(0, &entry[fixed..]).context("unterminated Git index path")?;
        // Only ordinary stage-zero entries. Reject assume-valid, extended flags,
        // and alternate stages rather than propagating them to a new worktree.
        ensure!(
            usize::from(flags) == length.min(0xfff),
            "unsupported Git index entry flags"
        );
        let padded = (fixed + length + 1).next_multiple_of(8);
        ensure!(padded <= entry.len(), "truncated Git index padding");
        let name = &entry[fixed..fixed + length];
        ensure!(valid_name(name), "invalid path in Git index");
        let path = payload.join(std::ffi::OsStr::from_bytes(name));
        let st =
            fs::symlink_metadata(&path).with_context(|| format!("statting {}", path.display()))?;
        // Two distinct Git blobs can collapse onto one name on a
        // case- or normalization-insensitive filesystem. Do not certify both.
        ensure!(
            inodes.insert((st.dev(), st.ino())),
            "tracked paths alias the same filesystem entry at {}",
            path.display()
        );
        let mode = uint32(&entry[24..28]);
        // core.symlinks=false checks a symlink blob out as ordinary file bytes.
        ensure!(
            match mode {
                0o100644 | 0o100755 => st.is_file(),
                0o120000 => st.is_symlink() || st.is_file(),
                0o160000 => st.is_dir(),
                _ => false,
            },
            "materialized path type differs from Git at {}",
            path.display()
        );
        let size = st.size() as u32;
        let stat_size = if size == 0 && st.size() != 0 {
            0x80000000
        } else {
            size
        };
        let fields = [
            st.ctime() as u32,
            st.ctime_nsec() as u32,
            st.mtime() as u32,
            st.mtime_nsec() as u32,
            st.dev() as u32,
            st.ino() as u32,
            mode,
            st.uid(),
            st.gid(),
            stat_size,
        ];
        let (words, _) = entry[..40].as_chunks_mut::<4>();
        for (word, value) in words.iter_mut().zip(fields) {
            *word = value.to_be_bytes();
        }
        newest_ctime = newest_ctime.max(st.ctime());
        pos += padded;
    }
    // A fresh read-tree can contain a cache-tree and parallel-read hints.
    // All retain the same entry offsets and object names. In particular never
    // accept a split index, fsmonitor bits, or an untracked cache from a seed.
    while pos < end {
        ensure!(end - pos >= 8, "truncated Git index extension");
        let sig = &index[pos..pos + 4];
        ensure!(
            matches!(sig, b"TREE" | b"IEOT" | b"EOIE"),
            "unsupported Git index extension"
        );
        let size = uint32(&index[pos + 4..pos + 8]) as usize;
        pos += 8;
        ensure!(size <= end - pos, "truncated Git index extension body");
        pos += size;
    }
    let trailer = checksum(&index[..end], hash_len)?;
    index[end..].copy_from_slice(&trailer);
    Ok((index, newest_ctime))
}

pub(crate) fn build(template: &[u8], payload: &Path, hash_len: usize) -> Result<Vec<u8>> {
    let (data, newest_ctime) = filled(template, payload, hash_len)?;
    // Apple Git can compare ctime only to whole seconds. APFS clones retain
    // the seed's older mtimes. Once the following second begins, an in-place
    // same-size edit that restores mtime must differ from the recorded ctime.
    let safe_time = UNIX_EPOCH + Duration::from_secs(newest_ctime as u64 + 1);
    while let Ok(delay) = safe_time.duration_since(SystemTime::now()) {
        ensure!(
            delay <= Duration::from_secs(1),
            "clone ctime is in the future"
        );
        std::thread::sleep(delay);
    }

    Ok(data)
}

pub(crate) fn install(data: &[u8], index_path: &Path) -> Result<()> {
    let lock_name = index_path.with_file_name("index.lock");
    let mut lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_name)?;
    let lock_name = TempPath::try_from_path(lock_name)?;
    lock.write_all(data)?;
    lock_name.persist_noclobber(index_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs::{File, FileTimes};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;

    fn prime(template: &[u8], payload: &Path, index: &Path, hash_len: usize) -> Result<()> {
        install(&build(template, payload, hash_len)?, index)
    }

    fn git(repo: &Path, args: &[&str], index: Option<&Path>, trace: Option<&Path>) -> Vec<u8> {
        let mut cmd = Command::new("git");
        for (k, _) in std::env::vars_os() {
            if k.as_bytes().starts_with(b"GIT_") {
                cmd.env_remove(k);
            }
        }
        cmd.current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_INDEX_VERSION", "2")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(args);
        if let Some(i) = index {
            cmd.env("GIT_INDEX_FILE", i);
        }
        if let Some(t) = trace {
            cmd.env("GIT_TRACE2_EVENT", t);
        }
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn exercise_format(format: &str, hash_len: usize) {
        let fixture = tempfile::tempdir().unwrap();
        let parent = fixture.path().canonicalize().unwrap();
        let repo = parent.join("repo");
        fs::create_dir(&repo).unwrap();
        git(
            &repo,
            &[
                "init",
                "-q",
                "--template=",
                &format!("--object-format={format}"),
            ],
            None,
            None,
        );
        let tracked = repo.join("code");
        fs::write(&tracked, "original\n").unwrap();
        fs::set_permissions(&tracked, fs::Permissions::from_mode(0o755)).unwrap();
        let mtime = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        File::open(&tracked)
            .unwrap()
            .set_times(FileTimes::new().set_modified(mtime))
            .unwrap();
        fs::write(
            repo.join(OsStr::from_bytes(b"file-\n\t")),
            b"unusual pathname",
        )
        .unwrap();
        symlink("code", repo.join("link")).unwrap();
        git(&repo, &["add", "--all"], None, None);
        git(&repo, &["commit", "-qm", "seed"], None, None);
        let original = git(&repo, &["write-tree"], None, None);
        let template = parent.join("template");
        git(&repo, &["read-tree", "HEAD"], Some(&template), None);

        let raw = fs::read(&template).unwrap();
        let index = parent.join("index");
        prime(&raw, &repo, &index, hash_len).unwrap();
        let existing = fs::read(&index).unwrap();
        assert!(install(b"replacement", &index).is_err());
        assert_eq!(fs::read(&index).unwrap(), existing);
        assert!(!parent.join("index.lock").exists());
        let trace = parent.join("trace");
        assert!(
            git(
                &repo,
                &["status", "--porcelain", "--untracked-files=all"],
                Some(&index),
                Some(&trace)
            )
            .is_empty()
        );
        assert!(fs::read_to_string(trace).unwrap().lines().any(|l| {
            let e: serde_json::Value = serde_json::from_str(l).unwrap();
            e["key"] == "refresh/sum_scan" && e["value"] == "0"
        }));
        assert_eq!(git(&repo, &["write-tree"], Some(&index), None), original);

        // A new private index, used first AFTER an edit with the seed's mtime.
        let edited = parent.join("edited");
        prime(&raw, &repo, &edited, hash_len).unwrap();
        fs::write(&tracked, "modified\n").unwrap();
        File::open(&tracked)
            .unwrap()
            .set_times(FileTimes::new().set_modified(mtime))
            .unwrap();
        assert_eq!(
            git(
                &repo,
                &["status", "--porcelain", "--untracked-files=all"],
                Some(&edited),
                None
            ),
            b" M code\n"
        );
        git(&repo, &["add", "code"], Some(&edited), None);
        assert_eq!(
            git(&repo, &["show", ":code"], Some(&edited), None),
            b"modified\n"
        );
        assert_eq!(git(&repo, &["show", ":code"], None, None), b"original\n");

        let mut broken = raw.clone();
        broken[0] ^= 1;
        let prior_index = fs::read(&index).unwrap();
        assert!(prime(&broken, &repo, &index, hash_len).is_err());
        assert_eq!(fs::read(&index).unwrap(), prior_index);
        let mut stage = raw.clone();
        stage[12 + 40 + hash_len] |= 0x80; // CE_VALID (assume-unchanged).
        let end = stage.len() - hash_len;
        let sum = checksum(&stage[..end], hash_len).unwrap();
        stage[end..].copy_from_slice(&sum);
        assert!(filled(&stage, &repo, hash_len).is_err());
        assert_eq!(fs::read(&template).unwrap(), raw);
        let collision = repo.join(OsStr::from_bytes(b"file-\n\t"));
        fs::remove_file(&collision).unwrap();
        fs::hard_link(&tracked, collision).unwrap();
        assert!(
            filled(&raw, &repo, hash_len)
                .unwrap_err()
                .to_string()
                .contains("alias")
        );
    }

    #[test]
    fn sha1_native_index_and_immediate_edits() {
        exercise_format("sha1", 20);
    }

    #[test]
    fn sha256_native_index_and_immediate_edits() {
        exercise_format("sha256", 32);
    }

    #[test]
    fn rejects_path_traversal_and_truncation() {
        for name in [b"" as &[u8], b"../x", b"/absolute", b".git/index", b"a/./b"] {
            assert!(!valid_name(name));
        }
        assert!(valid_name(b"nested/git-objects"));
        for n in 0..40 {
            assert!(filled(&vec![0; n], Path::new("."), 20).is_err());
        }
    }
}
