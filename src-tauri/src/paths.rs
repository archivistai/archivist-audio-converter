//! Output path selection and temp-file lifetime.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Marker in temp filenames so the startup sweep can recognise our own orphans.
const TEMP_MARKER: &str = ".archivist-stage-";

/// Builds a non-colliding output path next to the source file with the new extension.
///
/// `suffix` is appended to the stem before the extension (`""` reproduces the app's original
/// naming). Cleaned output gets a suffix so it never silently shadows a plain conversion of
/// the same source.
pub fn build_output_path(input: &Path, suffix: &str, target_ext: &str) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| Path::new("."));
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "converted".to_string());

    let mut candidate = parent.join(format!("{stem}{suffix}.{target_ext}"));
    let mut counter = 1;
    while candidate.exists() {
        candidate = parent.join(format!("{stem}{suffix} ({counter}).{target_ext}"));
        counter += 1;
    }
    candidate
}

/// A temp file that deletes itself when dropped, covering the success, `?`-error and cancel
/// paths without repeating cleanup at each exit.
///
/// `Drop` does not run if the process is killed outright, hence [`sweep_orphans`].
pub struct TempFile {
    path: Option<PathBuf>,
}

impl TempFile {
    /// Places the intermediate beside the eventual output rather than in the system temp
    /// dir: it is the same volume the user already expects a large file on, so a multi-GB
    /// intermediate cannot fill a small system volume.
    pub fn beside(output: &Path, ext: &str) -> Self {
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        let stem = output
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "stage".into());
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            path: Some(parent.join(format!("{stem}{TEMP_MARKER}{unique}.{ext}"))),
        }
    }

    pub fn path(&self) -> &Path {
        self.path.as_deref().unwrap_or_else(|| Path::new(""))
    }

    /// Gives up ownership so the file survives the drop. Used when a staged file has been
    /// renamed into its final place.
    pub fn keep(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(p) = &self.path {
            let _ = fs::remove_file(p);
        }
    }
}

/// Removes stale intermediates left behind by a hard kill (where `Drop` never ran).
///
/// Only touches files carrying our marker and older than an hour, so a job running in
/// another window is never disturbed.
pub fn sweep_orphans(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let cutoff = Duration::from_secs(60 * 60);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains(TEMP_MARKER) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map(|age| age > cutoff)
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_is_appended_before_the_extension() {
        let got = build_output_path(Path::new("/tmp/does-not-exist-xyz/session.wav"), " - clean", "mp3");
        assert_eq!(got, PathBuf::from("/tmp/does-not-exist-xyz/session - clean.mp3"));
    }

    #[test]
    fn empty_suffix_matches_the_original_naming() {
        let got = build_output_path(Path::new("/tmp/does-not-exist-xyz/session.wav"), "", "mp3");
        assert_eq!(got, PathBuf::from("/tmp/does-not-exist-xyz/session.mp3"));
    }

    #[test]
    fn collisions_get_a_counter() {
        let dir = std::env::temp_dir().join(format!("aac-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("take.wav");
        fs::write(&src, b"x").unwrap();
        fs::write(dir.join("take - clean.mp3"), b"x").unwrap();

        let got = build_output_path(&src, " - clean", "mp3");
        assert_eq!(got, dir.join("take - clean (1).mp3"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn temp_file_removes_itself_on_drop() {
        let dir = std::env::temp_dir().join(format!("aac-tmp-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.mp3");

        let path = {
            let t = TempFile::beside(&out, "flac");
            fs::write(t.path(), b"data").unwrap();
            assert!(t.path().exists());
            t.path().to_path_buf()
        };
        assert!(!path.exists(), "temp file should be gone after drop");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn kept_temp_file_survives_drop() {
        let dir = std::env::temp_dir().join(format!("aac-keep-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.mp3");

        let path = {
            let mut t = TempFile::beside(&out, "flac");
            fs::write(t.path(), b"data").unwrap();
            let p = t.path().to_path_buf();
            t.keep();
            p
        };
        assert!(path.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sweep_leaves_unrelated_and_fresh_files_alone() {
        let dir = std::env::temp_dir().join(format!("aac-sweep-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let keeper = dir.join("important.wav");
        let fresh_stage = dir.join(format!("out{TEMP_MARKER}123.flac"));
        fs::write(&keeper, b"x").unwrap();
        fs::write(&fresh_stage, b"x").unwrap();

        sweep_orphans(&dir);

        assert!(keeper.exists(), "unrelated files must not be touched");
        assert!(fresh_stage.exists(), "a recent stage file may belong to a live job");
        fs::remove_dir_all(&dir).unwrap();
    }
}
