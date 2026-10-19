//! Comparison of files and directories that aren't git refs.
//!
//! Files are compared by presence, type, size and modification time rather than content, so
//! large trees are cheap to compare and unreadable files don't break the comparison. A content
//! diff is only generated for the file being viewed.

use crate::parser::{FileChange, FileDiff};
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Find the files that differ between two files or directories. With `one_file_system`,
/// directories on a different filesystem than their side's root are treated as empty.
pub fn compare_paths(target1: &str, target2: &str, one_file_system: bool) -> Result<Vec<FileDiff>> {
    let targets = Targets::resolve(target1, target2)?;
    let root_devices = if one_file_system && targets.both_dirs {
        let device = |root: &Path| {
            fs::metadata(root)
                .map(|metadata| Some(metadata.dev()))
                .with_context(|| format!("Cannot access {}", root.display()))
        };
        [device(&targets.left)?, device(&targets.right)?]
    } else {
        [None, None]
    };
    let mut walker = Walker {
        targets: &targets,
        root_devices,
        changes: Vec::new(),
    };

    if targets.both_dirs {
        walker.compare_dir(Path::new(""), &Entry::Dir, &Entry::Dir);
    } else {
        let left = Entry::read(&targets.left);
        let right = Entry::read(&targets.right);
        if left != right {
            walker.report(Path::new(&targets.file_name()), left, right);
        }
    }

    Ok(walker.changes)
}

/// Describe the differences for one file reported by [`compare_paths`], including a content
/// diff when both sides are (or are missing) regular files or symlinks
pub fn file_diff(target1: &str, target2: &str, file_path: &str) -> Result<String> {
    let targets = Targets::resolve(target1, target2)?;
    let (left_path, right_path) = targets.pair(file_path);
    let left = Entry::read_checked(&left_path);
    let right = Entry::read_checked(&right_path);

    let mut output = summary(&left_path, &left, &right_path, &right);
    if left.is_diffable() && right.is_diffable() && !(left.is_missing() && right.is_missing()) {
        output.push('\n');
        match content_diff(&targets, &left_path, &left, &right_path, &right) {
            Ok(diff) if diff.is_empty() => output.push_str("File contents are identical\n"),
            Ok(diff) => output.push_str(&diff),
            Err(e) => output.push_str(&format!("Could not compare contents: {e}\n")),
        }
    }
    Ok(output)
}

/// The two sides of a file/directory comparison
#[derive(Clone)]
pub struct Targets {
    left: PathBuf,
    right: PathBuf,
    both_dirs: bool,
}

impl Targets {
    pub fn resolve(target1: &str, target2: &str) -> Result<Self> {
        let canonicalize = |target: &str| {
            fs::canonicalize(target).with_context(|| format!("Cannot access {target}"))
        };
        let left = canonicalize(target1)?;
        let right = canonicalize(target2)?;
        let both_dirs = left.is_dir() && right.is_dir();
        Ok(Self {
            left,
            right,
            both_dirs,
        })
    }

    /// Paths of both sides of a reported file
    pub fn pair(&self, file_path: &str) -> (PathBuf, PathBuf) {
        if self.both_dirs {
            (self.left.join(file_path), self.right.join(file_path))
        } else {
            (self.left.clone(), self.right.clone())
        }
    }

    /// Directories that reported paths are relative to
    fn roots(&self) -> [&Path; 2] {
        if self.both_dirs {
            [&self.left, &self.right]
        } else {
            [&self.left, &self.right].map(|path| path.parent().unwrap_or(Path::new("/")))
        }
    }

    /// Name to report when comparing two single files
    fn file_name(&self) -> String {
        self.left
            .file_name()
            .or(self.right.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| self.left.to_string_lossy().to_string())
    }
}

/// What was found at one side of a comparison. Errors are kept as their message, so the same
/// error on both sides compares equal.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Missing,
    Dir,
    UnreadableDir(String),
    File {
        kind: FileKind,
        size: u64,
        /// Whole seconds since the epoch, since backups often don't preserve sub-second times
        modified: Option<i64>,
    },
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FileKind {
    Regular,
    Symlink,
    Special,
}

impl Entry {
    /// Read an entry without following symlinks
    fn read(path: &Path) -> Self {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => Entry::Dir,
            Ok(metadata) => Entry::File {
                kind: if metadata.is_file() {
                    FileKind::Regular
                } else if metadata.is_symlink() {
                    FileKind::Symlink
                } else {
                    FileKind::Special
                },
                size: metadata.len(),
                modified: metadata.modified().ok().map(unix_seconds),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Entry::Missing,
            Err(e) => Entry::Error(e.to_string()),
        }
    }

    /// Like [`Entry::read`], but also check that directories can be listed
    fn read_checked(path: &Path) -> Self {
        match Self::read(path) {
            Entry::Dir => match fs::read_dir(path) {
                Ok(_) => Entry::Dir,
                Err(e) => Entry::UnreadableDir(e.to_string()),
            },
            entry => entry,
        }
    }

    fn is_missing(&self) -> bool {
        matches!(self, Entry::Missing)
    }

    /// Whether this side can take part in a content diff
    fn is_diffable(&self) -> bool {
        matches!(
            self,
            Entry::Missing
                | Entry::File {
                    kind: FileKind::Regular | FileKind::Symlink,
                    ..
                }
        )
    }

    fn describe(&self) -> String {
        match self {
            Entry::Missing => "missing".to_string(),
            Entry::Dir => "directory".to_string(),
            Entry::UnreadableDir(e) => format!("directory, contents unreadable: {e}"),
            Entry::File {
                kind,
                size,
                modified,
            } => {
                let kind = match kind {
                    FileKind::Regular => "file",
                    FileKind::Symlink => "symlink",
                    FileKind::Special => "special file",
                };
                let modified = modified.map_or_else(
                    || "modification time unavailable".to_string(),
                    |secs| format!("modified {}", format_timestamp(secs)),
                );
                format!("{kind}, {size} bytes, {modified}")
            }
            Entry::Error(e) => format!("error: {e}"),
        }
    }
}

struct Walker<'a> {
    targets: &'a Targets,
    /// Devices of the left and right roots, when not crossing filesystem boundaries
    root_devices: [Option<u64>; 2],
    changes: Vec<FileDiff>,
}

impl Walker<'_> {
    /// Compare the contents of a directory present on either side
    fn compare_dir(&mut self, rel: &Path, left: &Entry, right: &Entry) {
        let left_path = self.targets.left.join(rel);
        let right_path = self.targets.right.join(rel);
        let [left_device, right_device] = self.root_devices;
        let (left_names, right_names) = match (
            list_dir(&left_path, left, left_device),
            list_dir(&right_path, right, right_device),
        ) {
            (Ok(left_names), Ok(right_names)) => (left_names, right_names),
            (Err(left_error), Err(right_error)) if left_error == right_error => return,
            (left_names, right_names) => {
                // The contents can't be compared, so report the directory itself
                let left = left_names.map_or_else(Entry::UnreadableDir, |_| left.clone());
                let right = right_names.map_or_else(Entry::UnreadableDir, |_| right.clone());
                self.report(rel, left, right);
                return;
            }
        };

        for name in left_names.union(&right_names) {
            let child = rel.join(name);
            let read = |root: &Path, names: &BTreeSet<OsString>| {
                if names.contains(name) {
                    Entry::read(&root.join(&child))
                } else {
                    Entry::Missing
                }
            };
            let left = read(&self.targets.left, &left_names);
            let right = read(&self.targets.right, &right_names);
            self.compare_entry(&child, left, right);
        }
    }

    fn compare_entry(&mut self, rel: &Path, left: Entry, right: Entry) {
        let left_is_dir = left == Entry::Dir;
        let right_is_dir = right == Entry::Dir;
        if !left_is_dir && !right_is_dir {
            if left != right {
                self.report(rel, left, right);
            }
            return;
        }

        // A directory replaced by something else: report that, then the directory's contents
        if left_is_dir != right_is_dir && !left.is_missing() && !right.is_missing() {
            self.report(rel, left.clone(), right.clone());
        }
        self.compare_dir(rel, &left, &right);
    }

    fn report(&mut self, rel: &Path, left: Entry, right: Entry) {
        let (left_path, right_path) = self.targets.pair(&rel.to_string_lossy());
        self.changes.push(FileDiff {
            filename: rel.to_string_lossy().to_string(),
            old_path: None,
            new_path: None,
            content: summary(&left_path, &left, &right_path, &right),
            added_lines: 0,
            removed_lines: 0,
            diff_key: None,
            change: Some(if left.is_missing() {
                FileChange::Added
            } else if right.is_missing() {
                FileChange::Deleted
            } else {
                FileChange::Modified
            }),
        });
    }
}

/// Names in a directory, or none if the entry isn't a directory or is on a different device
/// than `root_device`. Entries that can't be read are skipped.
fn list_dir(
    path: &Path,
    entry: &Entry,
    root_device: Option<u64>,
) -> Result<BTreeSet<OsString>, String> {
    if *entry != Entry::Dir {
        return Ok(BTreeSet::new());
    }
    if let Some(root_device) = root_device {
        // If the metadata can't be read, listing the directory will report the error
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.dev() != root_device) {
            return Ok(BTreeSet::new());
        }
    }
    let entries = fs::read_dir(path).map_err(|e| e.to_string())?;
    Ok(entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .collect())
}

fn summary(left_path: &Path, left: &Entry, right_path: &Path, right: &Entry) -> String {
    format!(
        "Left:  {}\n       {}\nRight: {}\n       {}\n",
        left_path.display(),
        left.describe(),
        right_path.display(),
        right.describe()
    )
}

/// Diff one pair of files with `git diff --no-index`, using `/dev/null` for a missing side
fn content_diff(
    targets: &Targets,
    left_path: &Path,
    left: &Entry,
    right_path: &Path,
    right: &Entry,
) -> Result<String> {
    let dev_null = Path::new("/dev/null");
    let output = Command::new("git")
        .args([
            "diff",
            "--no-index",
            "--no-color",
            "--no-ext-diff",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--",
        ])
        .arg(if left.is_missing() {
            dev_null
        } else {
            left_path
        })
        .arg(if right.is_missing() {
            dev_null
        } else {
            right_path
        })
        .output()
        .context("Failed to execute git diff --no-index")?;

    // Exit code 1 just means the files differ
    if !matches!(output.status.code(), Some(0 | 1)) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("{}", stderr.trim()));
    }

    // Files may be in any encoding
    let diff = String::from_utf8_lossy(&output.stdout);
    Ok(strip_roots(&diff, &targets.roots()))
}

/// `git diff --no-index` shows paths as given (minus the leading `/`), e.g. `a/home/me/file`.
/// Rewrite them relative to the compared roots (`a/file`) in file headers, leaving hunk
/// content untouched.
fn strip_roots(diff: &str, roots: &[&Path]) -> String {
    let mut patterns: Vec<(String, &str)> = roots
        .iter()
        .map(|root| root.to_string_lossy().trim_start_matches('/').to_string())
        .filter(|root| !root.is_empty())
        .flat_map(|root| ["a/", "b/"].map(|side| (format!("{side}{root}/"), side)))
        .collect();
    // Prefer the longest match when one root contains another
    patterns.sort_by_key(|(pattern, _)| std::cmp::Reverse(pattern.len()));

    let mut result = String::with_capacity(diff.len());
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
        } else if line.starts_with("@@") {
            in_hunk = true;
        }

        if in_hunk {
            result.push_str(line);
        } else {
            strip_path_prefixes(line, &patterns, &mut result);
        }
        result.push('\n');
    }
    result
}

/// Replace path prefixes that start a whitespace- or quote-delimited token
fn strip_path_prefixes(line: &str, patterns: &[(String, &str)], out: &mut String) {
    let mut i = 0;
    while i < line.len() {
        let at_token_start = i == 0 || matches!(line.as_bytes()[i - 1], b' ' | b'\t' | b'"');
        if at_token_start {
            if let Some((pattern, side)) = patterns
                .iter()
                .find(|(pattern, _)| line[i..].starts_with(pattern.as_str()))
            {
                out.push_str(side);
                i += pattern.len();
                continue;
            }
        }
        let ch = line[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
}

/// Whole seconds since the epoch, rounding down for times before it
fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs() as i64,
        Err(e) => {
            let before = e.duration();
            -(before.as_secs() as i64) - i64::from(before.subsec_nanos() > 0)
        }
    }
}

/// Format seconds since the epoch as a UTC date and time
fn format_timestamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400);

    // Civil-from-days conversion, from Howard Hinnant's date algorithms
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        time / 3_600,
        time % 3_600 / 60,
        time % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::Duration;

    fn set_modified(path: &Path, secs: u64) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn write(path: &Path, contents: &str, modified: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        set_modified(path, modified);
    }

    fn changed_files(left: &Path, right: &Path) -> Vec<String> {
        let mut files: Vec<_> =
            compare_paths(&left.to_string_lossy(), &right.to_string_lossy(), false)
                .unwrap()
                .into_iter()
                .map(|file| file.filename)
                .collect();
        files.sort();
        files
    }

    #[test]
    fn test_compare_directories() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        let (left, right) = (left.path(), right.path());

        write(&left.join("same"), "same", 1_000);
        write(&right.join("same"), "same", 1_000);
        write(&left.join("empty-new-file"), "", 1_000);
        write(&right.join("sub/added"), "added", 1_000);
        write(&left.join("sub/resized"), "1", 1_000);
        write(&right.join("sub/resized"), "22", 1_000);
        write(&left.join("sub/touched"), "1", 1_000);
        write(&right.join("sub/touched"), "2", 2_000);
        write(&left.join("only-left/deep/file"), "x", 1_000);
        // Same size and time, so considered identical despite different contents
        write(&left.join("sub/same-metadata"), "1", 1_000);
        write(&right.join("sub/same-metadata"), "2", 1_000);

        assert_eq!(
            changed_files(left, right),
            [
                "empty-new-file",
                "only-left/deep/file",
                "sub/added",
                "sub/resized",
                "sub/touched",
            ]
        );
    }

    #[test]
    fn test_compare_reports_change_kinds() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        write(&left.path().join("deleted"), "x", 1_000);
        write(&right.path().join("added"), "x", 1_000);
        write(&left.path().join("modified"), "1", 1_000);
        write(&right.path().join("modified"), "2", 2_000);

        let mut changes: Vec<_> = compare_paths(
            &left.path().to_string_lossy(),
            &right.path().to_string_lossy(),
            false,
        )
        .unwrap()
        .into_iter()
        .map(|file| (file.filename, file.change))
        .collect();
        changes.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(
            changes,
            [
                ("added".to_string(), Some(FileChange::Added)),
                ("deleted".to_string(), Some(FileChange::Deleted)),
                ("modified".to_string(), Some(FileChange::Modified)),
            ]
        );
    }

    #[test]
    fn test_compare_ignores_subsecond_times() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        fs::write(left.path().join("f"), "x").unwrap();
        fs::write(right.path().join("f"), "x").unwrap();
        let time = UNIX_EPOCH + Duration::from_secs(1_000);
        File::options()
            .write(true)
            .open(left.path().join("f"))
            .unwrap()
            .set_modified(time)
            .unwrap();
        File::options()
            .write(true)
            .open(right.path().join("f"))
            .unwrap()
            .set_modified(time + Duration::from_millis(500))
            .unwrap();

        assert!(changed_files(left.path(), right.path()).is_empty());
    }

    #[test]
    fn test_compare_file_replaced_by_directory() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        write(&left.path().join("x"), "file", 1_000);
        write(&right.path().join("x/inner"), "file", 1_000);

        assert_eq!(changed_files(left.path(), right.path()), ["x", "x/inner"]);
    }

    #[test]
    fn test_compare_unreadable_directories() {
        use std::os::unix::fs::PermissionsExt;

        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        let (left, right) = (left.path(), right.path());
        for dir in ["both-locked", "left-locked"] {
            write(&left.join(dir).join("f"), "x", 1_000);
            write(&right.join(dir).join("f"), "x", 1_000);
        }
        write(&left.join("changed"), "1", 1_000);
        write(&right.join("changed"), "2", 2_000);

        let lock = |path: PathBuf| {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            path
        };
        let locked = [
            lock(left.join("both-locked")),
            lock(right.join("both-locked")),
            lock(left.join("left-locked")),
        ];
        // Permissions don't apply to root
        let enforced = fs::read_dir(&locked[0]).is_err();

        let files = changed_files(left, right);
        for path in locked {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        if enforced {
            assert_eq!(files, ["changed", "left-locked"]);
        }
    }

    #[test]
    fn test_list_dir_skips_other_devices() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("f"), "x", 1_000);
        let device = fs::metadata(dir.path()).unwrap().dev();

        let names = |root_device| list_dir(dir.path(), &Entry::Dir, root_device).unwrap();
        assert_eq!(names(None).len(), 1);
        assert_eq!(names(Some(device)).len(), 1);
        assert!(names(Some(device.wrapping_add(1))).is_empty());
    }

    #[test]
    fn test_same_errors_are_identical() {
        let left = Entry::Error("Permission denied (os error 13)".to_string());
        let right = Entry::Error("Permission denied (os error 13)".to_string());
        let other = Entry::Error("Input/output error (os error 5)".to_string());
        assert_eq!(left, right);
        assert_ne!(left, other);
    }

    #[test]
    fn test_compare_single_files() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("one"), "1", 1_000);
        write(&dir.path().join("two"), "2", 2_000);

        assert_eq!(
            changed_files(&dir.path().join("one"), &dir.path().join("two")),
            ["one"]
        );
    }

    #[test]
    fn test_file_diff_shows_metadata_and_content() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        write(&left.path().join("sub/f"), "old\n", 0);
        write(&right.path().join("sub/f"), "new\n", 1_000_000_000);

        let output = file_diff(
            &left.path().to_string_lossy(),
            &right.path().to_string_lossy(),
            "sub/f",
        )
        .unwrap();

        assert!(output.contains("file, 4 bytes, modified 1970-01-01 00:00:00 UTC"));
        assert!(output.contains("file, 4 bytes, modified 2001-09-09 01:46:40 UTC"));
        assert!(output.contains("diff --git a/sub/f b/sub/f\n"));
        assert!(output.contains("-old\n+new\n"));
    }

    #[test]
    fn test_file_diff_for_missing_side() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        write(&left.path().join("gone"), "content\n", 1_000);

        let output = file_diff(
            &left.path().to_string_lossy(),
            &right.path().to_string_lossy(),
            "gone",
        )
        .unwrap();

        assert!(output.contains("       missing\n"));
        assert!(output.contains("+++ /dev/null\n"));
        assert!(output.contains("-content\n"));
    }

    #[test]
    fn test_strip_roots() {
        let diff = "diff --git a/home/me/sub/f b/backup/home/me/sub/f\n\
                    --- a/home/me/sub/f\n\
                    +++ b/backup/home/me/sub/f\n\
                    @@ -1 +1 @@\n\
                    -a/home/me/sub/f\n\
                    +x\n";
        let roots = [Path::new("/home/me"), Path::new("/backup/home/me")];

        assert_eq!(
            strip_roots(diff, &roots),
            "diff --git a/sub/f b/sub/f\n\
             --- a/sub/f\n\
             +++ b/sub/f\n\
             @@ -1 +1 @@\n\
             -a/home/me/sub/f\n\
             +x\n"
        );
    }

    #[test]
    fn test_format_timestamp() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_timestamp(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_timestamp(-1), "1969-12-31 23:59:59 UTC");
    }
}
