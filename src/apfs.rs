//! Clone files or entire directory trees with one exclusive Darwin clonefile call.
use std::{io, path::Path};

#[cfg(target_os = "macos")]
fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL byte in path"))
}

#[cfg(target_os = "macos")]
pub fn clone_path(source: &Path, destination: &Path) -> io::Result<()> {
    let source = c_path(source)?;
    let destination = c_path(destination)?;
    // Both strings are owned for the duration of the call. The kernel refuses
    // an existing destination and reports unsupported or cross-volume clones.
    if unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Bind the cache root to its prepared identity before asking APFS to clone it.
/// A rename or symlink swap in the source pathname cannot redirect the syscall.
#[cfg(target_os = "macos")]
pub(crate) fn clone_seed(
    source: &Path,
    destination: &Path,
    expected_dev: u64,
    expected_ino: u64,
) -> io::Result<()> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(source)
        .map_err(|error| match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP) => io::Error::new(
                io::ErrorKind::InvalidData,
                "cache tree is missing or indirect",
            ),
            _ => error,
        })?;
    let stat = source.metadata()?;
    if (stat.dev(), stat.ino()) != (expected_dev, expected_ino) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache tree was replaced",
        ));
    }
    let dest = c_path(destination)?;
    // The verified descriptor and destination CString live across the syscall.
    if unsafe { libc::fclonefileat(source.as_raw_fd(), libc::AT_FDCWD, dest.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
pub fn clone_path(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn clone_seed(_: &Path, _: &Path, _: u64, _: u64) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn cloned_file_is_independently_writable_and_exclusive() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("source");
        let dest = parent.path().join("copy");
        fs::write(&source, b"before").unwrap();
        clone_path(&source, &dest).unwrap();
        fs::write(&dest, b"after").unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"before");
        assert_eq!(
            clone_path(&source, &dest).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&dest).unwrap(), b"after");
    }

    #[test]
    fn seed_clone_requires_the_original_directory() {
        use std::os::unix::fs::{MetadataExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("tree");
        let dest = temp.path().join("copy");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"expected").unwrap();
        let stat = source.metadata().unwrap();
        let clone = || clone_seed(&source, &dest, stat.dev(), stat.ino());
        clone().unwrap();
        assert_eq!(fs::read(dest.join("file")).unwrap(), b"expected");
        fs::remove_dir_all(&dest).unwrap();
        let saved = temp.path().join("saved");
        fs::rename(&source, &saved).unwrap();
        symlink(&saved, &source).unwrap();
        assert_eq!(clone().unwrap_err().kind(), io::ErrorKind::InvalidData);
        fs::remove_file(&source).unwrap();
        fs::create_dir(&source).unwrap();
        assert_eq!(clone().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(!dest.exists());
    }
}
