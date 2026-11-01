//! Seal the actual directory tree written by Git, including directory membership.
//!
//! Compare the original seed *after* clonefile completes and before adopting
//! the clone. A user can restore an old mtime, but Darwin's change time cannot
//! be set by an application (setattrlist(2)). A replacement has a different
//! inode. Access times are deliberately excluded: normal reads may change them.
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet, fs, os::unix::ffi::OsStrExt, os::unix::fs::MetadataExt, path::Path,
};
use walkdir::WalkDir;

/// The signedness and little-endian encoding are part of this seal's format.
/// Names are length-prefixed native bytes, so newline and non-UTF-8 names work.
pub(crate) fn fingerprint(tree: &Path) -> Result<String> {
    fingerprint_paths(tree, None)
}

/// For an incremental checkout also require an exact match to Git's inventory,
/// including directory spelling on case- and normalization-insensitive volumes.
pub(crate) fn fingerprint_paths(tree: &Path, git_paths: Option<&[u8]>) -> Result<String> {
    let root = fs::symlink_metadata(tree)?;
    ensure!(root.is_dir(), "cache tree is not a directory");
    let mut expected = git_paths.map(|paths| {
        paths
            .split(|&c| c == 0)
            .filter(|p| !p.is_empty())
            .collect::<HashSet<_>>()
    });
    let mut hash = Sha256::new();
    hash.update(b"worktree-fast/checkout-tree/v1\0");
    for entry in WalkDir::new(tree)
        .follow_root_links(false)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry?;
        let m = entry.metadata()?;
        ensure!(
            m.dev() == root.dev() && (m.is_dir() || m.is_file() || m.is_symlink()),
            "unexpected object in the checkout cache"
        );
        let name = entry.path().strip_prefix(tree)?.as_os_str().as_bytes();
        if !name.is_empty()
            && let Some(names) = &mut expected
        {
            ensure!(names.remove(name), "checkout path differs from Git's tree");
        }
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name);
        hash.update(m.dev().to_le_bytes());
        hash.update(m.ino().to_le_bytes());
        hash.update(m.mode().to_le_bytes());
        hash.update(m.size().to_le_bytes());
        hash.update(m.mtime().to_le_bytes());
        hash.update(m.mtime_nsec().to_le_bytes());
        hash.update(m.ctime().to_le_bytes());
        hash.update(m.ctime_nsec().to_le_bytes());
    }
    ensure!(
        expected.is_none_or(|names| names.is_empty()),
        "missing checkout paths"
    );
    Ok(hex::encode(hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::symlink;

    #[test]
    fn changes_in_bytes_names_and_symlink_targets_break_the_seal() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("tree");
        fs::create_dir_all(tree.join("dir")).unwrap();
        let file = tree.join("dir/a");
        fs::write(&file, b"first").unwrap();
        symlink("dir/a", tree.join("link")).unwrap();
        let initial = fingerprint(&tree).unwrap();
        let original_time = file.metadata().unwrap().modified().unwrap();
        fs::write(&file, b"other").unwrap();
        File::open(&file)
            .unwrap()
            .set_modified(original_time)
            .unwrap();
        let edited = fingerprint(&tree).unwrap();
        assert_ne!(initial, edited);
        fs::write(tree.join("dir/.env"), b"private").unwrap();
        let extra = fingerprint(&tree).unwrap();
        assert_ne!(edited, extra);
        fs::remove_file(tree.join("link")).unwrap();
        symlink("dir/.env", tree.join("link")).unwrap();
        assert_ne!(extra, fingerprint(&tree).unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reading_and_cloning_leave_the_source_seal_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("source");
        let sub = tree.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("empty"), b"").unwrap();
        fs::write(sub.join("full"), b"content").unwrap();
        symlink("sub/full", tree.join("link")).unwrap();
        let expected = fingerprint(&tree).unwrap();
        let clone = tmp.path().join("copy");
        crate::apfs::clone_path(&tree, &clone).unwrap();
        fs::read(tree.join("sub/full")).unwrap();
        fs::write(clone.join("sub/full"), b"changed").unwrap();
        assert_eq!(expected, fingerprint(&tree).unwrap());
    }
}
