//! Do two paths name the same file?
//!
//! Comparing the strings is not enough and the difference is not academic:
//! `E:\film.mkv`, `e:/FILM.MKV` and `E:\.\film.mkv` are one file on Windows,
//! and a rewrite whose output lands on its own input reads a file it is
//! truncating - which leaves neither the original nor a usable result. The
//! same goes for a reference: the master is read once and would be gone.
//!
//! So: ask the file system where both paths really point when both exist,
//! and when one is still to be created, compare the folder it would go into
//! with the other's folder and the two names the way the platform does.

use std::path::{Path, PathBuf};

/// Two names for one file, as the platform sees it.
fn names_equal(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        // Not `eq_ignore_ascii_case`: Windows folds Umlauts too.
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

/// The folder a path sits in, resolved as far as it exists, and the name.
fn dir_and_name(p: &Path) -> Option<(PathBuf, String)> {
    let name = p.file_name()?.to_string_lossy().into_owned();
    let dir = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let dir = dir.canonicalize().unwrap_or(dir);
    Some((dir, name))
}

/// True when both paths name the same file. False when it cannot be told -
/// callers use this to refuse, and refusing on a guess would be worse.
pub fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    // Both there: the file system knows, symlinks and all.
    if let (Ok(ca), Ok(cb)) = (a.canonicalize(), b.canonicalize()) {
        return ca == cb;
    }
    match (dir_and_name(a), dir_and_name(b)) {
        (Some((da, na)), Some((db, nb))) => da == db && names_equal(&na, &nb),
        _ => false,
    }
}

/// `same_file` for the string a form holds; empty means "not given".
pub fn same_file_str(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim().trim_matches('"'), b.trim().trim_matches('"'));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    same_file(Path::new(a), Path::new(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_file_spelled_differently_is_the_same_file() {
        let dir = std::env::temp_dir().join("ddvt_samepath");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("Film (2023).mkv");
        std::fs::write(&file, b"x").unwrap();

        assert!(same_file(&file, &file));
        // A folder step that cancels out.
        assert!(same_file(&file, &dir.join(".").join("Film (2023).mkv")));
        if cfg!(windows) {
            // Case, and the other separator.
            let shouted = PathBuf::from(file.display().to_string().to_uppercase());
            assert!(same_file(&file, &shouted), "{} vs {}", file.display(), shouted.display());
            let slashed = PathBuf::from(file.display().to_string().replace('\\', "/"));
            assert!(same_file(&file, &slashed), "{}", slashed.display());
        }
        // A file that does not exist yet, in the same folder, same name.
        let gone = dir.join("not-there.mkv");
        assert!(same_file(&gone, &dir.join("not-there.mkv")));
        if cfg!(windows) {
            assert!(same_file(&gone, &dir.join("NOT-THERE.mkv")));
        }
        // Different names are different files.
        assert!(!same_file(&file, &dir.join("other.mkv")));
        assert!(!same_file(&file, &gone));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn the_string_form_ignores_quotes_and_blanks() {
        assert!(!same_file_str("", "anything.mkv"));
        assert!(!same_file_str("anything.mkv", "   "));
        assert!(same_file_str("  \"a.mkv\" ", "a.mkv"));
    }
}
