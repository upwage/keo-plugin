//! Approved directories: the only folders a task from Keo may run in. Each one is absolute,
//! exists, and is stored canonical (symlinks resolved), with no duplicates.

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DirError {
    #[error("type the full path, like /Users/you/code or ~/code")]
    NotAbsolute,
    #[error("the folder does not exist")]
    Missing,
    #[error("the path is not a folder")]
    NotADir,
}

/// Check and canonicalize a folder the person typed. A leading `~/` (or `~` alone) means the
/// home folder (`HOME`, else `USERPROFILE` on Windows); any other relative path is refused.
pub fn normalize(input: &str) -> Result<PathBuf, DirError> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    normalize_in(input, home.as_deref())
}

fn normalize_in(input: &str, home: Option<&Path>) -> Result<PathBuf, DirError> {
    let trimmed = input.trim();
    let expanded = match (trimmed.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(trimmed),
    };
    let path = expanded.as_path();
    if !path.is_absolute() {
        return Err(DirError::NotAbsolute);
    }
    let canon = path.canonicalize().map_err(|_| DirError::Missing)?;
    if !canon.is_dir() {
        return Err(DirError::NotADir);
    }
    Ok(canon)
}

/// Add a folder; returns false when it was already there.
pub fn add(list: &mut Vec<PathBuf>, dir: PathBuf) -> bool {
    if list.contains(&dir) {
        return false;
    }
    list.push(dir);
    list.sort();
    true
}

/// Remove a folder by its stored path; returns false when it was not there.
pub fn remove(list: &mut Vec<PathBuf>, dir: &Path) -> bool {
    let before = list.len();
    list.retain(|d| d != dir);
    list.len() != before
}

/// Whether `cwd` is one of the approved folders or inside one. `cwd` is canonicalized first, so
/// `..` and symlinks cannot step outside; a path that does not exist is never inside.
pub fn contains(list: &[PathBuf], cwd: &Path) -> bool {
    admit(list, cwd).is_some()
}

/// The canonical form of `cwd` when it is inside an approved folder (see [`contains`]). A
/// launch uses this path, never `cwd` as given: a symlink in `cwd` swapped after the check then
/// cannot move the launch elsewhere.
pub fn admit(list: &[PathBuf], cwd: &Path) -> Option<PathBuf> {
    if !cwd.is_absolute() {
        return None;
    }
    let canon = cwd.canonicalize().ok()?;
    list.iter().any(|d| canon.starts_with(d)).then_some(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let canon = tmp.path().canonicalize().unwrap();
        assert_eq!(normalize(tmp.path().to_str().unwrap()).unwrap(), canon);
        assert_eq!(normalize("relative/path"), Err(DirError::NotAbsolute));
        assert_eq!(
            normalize(tmp.path().join("nope").to_str().unwrap()),
            Err(DirError::Missing)
        );
        let file = tmp.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(normalize(file.to_str().unwrap()), Err(DirError::NotADir));
    }

    #[test]
    fn normalize_expands_a_leading_tilde_to_home() {
        let home = tempfile::tempdir().unwrap();
        let canon = home.path().canonicalize().unwrap();
        std::fs::create_dir(home.path().join("keo-companion-test")).unwrap();
        let h = Some(home.path());
        assert_eq!(
            normalize_in("~/keo-companion-test", h),
            Ok(canon.join("keo-companion-test"))
        );
        assert_eq!(
            normalize_in("  ~/keo-companion-test/ ", h),
            Ok(canon.join("keo-companion-test"))
        );
        assert_eq!(normalize_in("~", h), Ok(canon.clone()));
        assert_eq!(normalize_in("~/nope", h), Err(DirError::Missing));
        // `~user` and other relative paths stay refused, and so does `~/` with no home.
        assert_eq!(normalize_in("~other/x", h), Err(DirError::NotAbsolute));
        assert_eq!(
            normalize_in("keo-companion-test", h),
            Err(DirError::NotAbsolute)
        );
        assert_eq!(
            normalize_in("./keo-companion-test", h),
            Err(DirError::NotAbsolute)
        );
        assert_eq!(
            normalize_in("~/keo-companion-test", None),
            Err(DirError::NotAbsolute)
        );
    }

    #[test]
    fn add_dedupes_and_remove() {
        let mut list = Vec::new();
        assert!(add(&mut list, PathBuf::from("/b")));
        assert!(add(&mut list, PathBuf::from("/a")));
        assert!(!add(&mut list, PathBuf::from("/a")));
        assert_eq!(list, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert!(remove(&mut list, Path::new("/a")));
        assert!(!remove(&mut list, Path::new("/a")));
    }

    #[test]
    fn containment() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let approved = root.join("work");
        let inner = approved.join("repo");
        let sibling = root.join("workshop");
        for d in [&inner, &sibling] {
            std::fs::create_dir_all(d).unwrap();
        }
        let list = vec![approved.clone()];
        assert!(contains(&list, &approved));
        assert!(contains(&list, &inner));
        // A shared name prefix is not containment.
        assert!(!contains(&list, &sibling));
        // `..` cannot climb out.
        assert!(!contains(
            &list,
            &inner.join("..").join("..").join("workshop")
        ));
        assert!(!contains(&list, &approved.join("missing")));
        assert!(!contains(&list, Path::new("work/repo")));
        #[cfg(unix)]
        {
            let link = approved.join("escape");
            std::os::unix::fs::symlink(&sibling, &link).unwrap();
            assert!(!contains(&list, &link));
            // A link that stays inside is admitted as its canonical target, which is what runs.
            let inside = approved.join("to-repo");
            std::os::unix::fs::symlink(&inner, &inside).unwrap();
            assert_eq!(admit(&list, &inside), Some(inner.clone()));
        }
        assert_eq!(
            admit(&list, &inner.join("..").join("repo")),
            Some(inner.clone())
        );
        assert_eq!(admit(&list, &sibling), None);
    }
}
