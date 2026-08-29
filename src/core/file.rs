use std::fs;
use std::path::Path;

use crate::core::error::UError;

/// Ensure that the specified directory exists.
pub fn ensure_dir(path: impl AsRef<Path>) -> Result<(), UError> {
    let path = path.as_ref();
    fs::create_dir_all(path)
        .map_err(|source| UError::FileError { path: path.to_path_buf(), source })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn test_ensure_dir_single() -> Result<(), UError> {
        let root = PathBuf::from("test").join("fs_single_test");
        let path = root.join("subdir");

        ensure_dir(&path)?;
        assert!(path.exists() && path.is_dir(), "directory created successfully");

        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn test_ensure_dir_exists() -> Result<(), UError> {
        let root = PathBuf::from("test").join("fs_exists_test");
        let path = root.join("a").join("b");

        // Create the directory first
        fs::create_dir_all(&path)?;
        // Call ensure_dir on the existing directory
        ensure_dir(&path)?;
        assert!(path.exists() && path.is_dir());

        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn test_ensure_dir_recursive() -> Result<(), UError> {
        let root = PathBuf::from("test").join("fs_recursive_test");
        let deep_path = root.join("x").join("y").join("z");

        let _ = fs::remove_dir_all(&deep_path);
        ensure_dir(&deep_path)?;
        assert!(deep_path.exists() && deep_path.is_dir());

        fs::remove_dir_all(&root)?;
        Ok(())
    }
}
