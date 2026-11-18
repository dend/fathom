//! Mapping between object keys and paths on the local disk.

use std::fs;
use std::path::{Path, PathBuf};

/// A local file and the object key it should be uploaded to.
#[derive(Debug, PartialEq, Eq)]
pub struct UploadSource {
    pub path: PathBuf,
    pub key: String,
    pub size: u64,
}

/// Expands the dropped or picked paths into individual files. Folders are
/// walked recursively and keep their structure below `prefix`.
pub fn collect_uploads(paths: &[PathBuf], prefix: &str) -> Vec<UploadSource> {
    let mut found = Vec::new();
    for path in paths {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        walk(path, format!("{prefix}{name}"), &mut found);
    }
    found
}

fn walk(path: &Path, key: String, found: &mut Vec<UploadSource>) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.is_file() {
        found.push(UploadSource {
            path: path.to_path_buf(),
            key,
            size: metadata.len(),
        });
        return;
    }
    // Symlinked directories are skipped so that link cycles cannot recurse forever.
    let is_symlink = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if !metadata.is_dir() || is_symlink {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for child in children {
        if let Some(name) = child.file_name().map(|n| n.to_string_lossy().into_owned()) {
            walk(&child, format!("{key}/{name}"), found);
        }
    }
}

/// Makes one key segment safe to use as a file or folder name.
pub fn sanitize_component(component: &str) -> String {
    let mut name: String = component
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            '<' | '>' | ':' | '"' | '|' | '?' | '*' if cfg!(windows) => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    if cfg!(windows) {
        // Windows silently strips trailing dots and spaces, and reserves device names.
        name.truncate(name.trim_end_matches(['.', ' ']).len());
        let stem = name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.ends_with(|c: char| c.is_ascii_digit()));
        if reserved {
            name.insert(0, '_');
        }
    }
    if name.is_empty() || name == "." || name == ".." {
        name = "_".to_string();
    }
    name
}

/// Resolves a `/`-separated relative key below `base`. The result can never
/// escape `base`, whatever the key contains.
pub fn path_for_key(base: &Path, relative_key: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    for component in relative_key.split('/').filter(|c| !c.is_empty()) {
        path.push(sanitize_component(component));
    }
    path
}

/// Returns `path`, or a variant such as `name (2).ext` if it already exists.
pub fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){extension}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fathom-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn keys_cannot_escape_the_download_folder() {
        let base = Path::new("downloads");
        assert_eq!(
            path_for_key(base, "../../etc/passwd"),
            base.join("_").join("_").join("etc").join("passwd")
        );
        assert_eq!(
            path_for_key(base, "a//b/./c"),
            base.join("a").join("b").join("_").join("c")
        );
        assert_eq!(path_for_key(base, "a\\..\\b"), base.join("a_.._b"));
    }

    #[test]
    #[cfg(windows)]
    fn sanitizes_windows_names() {
        assert_eq!(sanitize_component("a:b?.txt"), "a_b_.txt");
        assert_eq!(sanitize_component("trailing. "), "trailing");
        assert_eq!(sanitize_component("NUL.txt"), "_NUL.txt");
        assert_eq!(sanitize_component("com1"), "_com1");
        assert_eq!(sanitize_component("comet"), "comet");
    }

    #[test]
    fn collects_files_recursively() {
        let dir = temp_dir("collect");
        fs::create_dir_all(dir.join("album/nested")).unwrap();
        fs::write(dir.join("album/a.txt"), b"12345").unwrap();
        fs::write(dir.join("album/nested/b.txt"), b"1").unwrap();
        fs::write(dir.join("single.bin"), b"").unwrap();

        let found = collect_uploads(&[dir.join("album"), dir.join("single.bin")], "dest/");
        let keys: Vec<(&str, u64)> = found.iter().map(|f| (f.key.as_str(), f.size)).collect();
        assert_eq!(
            keys,
            [
                ("dest/album/a.txt", 5),
                ("dest/album/nested/b.txt", 1),
                ("dest/single.bin", 0)
            ]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn avoids_overwriting_existing_files() {
        let dir = temp_dir("unique");
        let target = dir.join("report.pdf");
        assert_eq!(unique_path(target.clone()), target);
        fs::write(&target, b"x").unwrap();
        assert_eq!(unique_path(target.clone()), dir.join("report (2).pdf"));
        fs::write(dir.join("report (2).pdf"), b"x").unwrap();
        assert_eq!(unique_path(target), dir.join("report (3).pdf"));
        let _ = fs::remove_dir_all(dir);
    }
}
