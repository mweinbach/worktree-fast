//! Clone files or entire directory trees with one exclusive Darwin clonefile call.
use std::{io, path::Path};

#[cfg(target_os = "macos")]
pub fn clone_path(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    fn c_path(path: &Path) -> io::Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL byte in path"))
    }
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

#[cfg(not(target_os = "macos"))]
pub fn clone_path(_source: &Path, _destination: &Path) -> io::Result<()> {
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
}
