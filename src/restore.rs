//! Copying and deleting files to make one side of a file/directory comparison match the
//! other.

use crate::compare::Targets;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, FileTimes, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};

/// Prefix of temporary files created next to a destination before replacing it
const TEMP_PREFIX: &str = ".ftdv-restore-";

/// Which side is made to match the other
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreDirection {
    RightToLeft,
    LeftToRight,
}

impl RestoreDirection {
    pub fn arrow(self) -> char {
        match self {
            RestoreDirection::RightToLeft => '←',
            RestoreDirection::LeftToRight => '→',
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            RestoreDirection::RightToLeft => "right to left",
            RestoreDirection::LeftToRight => "left to right",
        }
    }

    /// Order a left and right value as (source, destination)
    fn source_and_dest<T>(self, left: T, right: T) -> (T, T) {
        match self {
            RestoreDirection::RightToLeft => (right, left),
            RestoreDirection::LeftToRight => (left, right),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreAction {
    /// Replace the destination with the source
    Copy,
    /// Delete the destination, which is missing from the source side
    Delete,
}

/// What will be done to a marked file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreMark {
    pub direction: RestoreDirection,
    pub action: RestoreAction,
}

impl RestoreMark {
    pub fn description(self) -> String {
        match self.action {
            RestoreAction::Copy => format!("copy {}", self.direction.description()),
            RestoreAction::Delete => {
                let (_, dest) = self.direction.source_and_dest("left", "right");
                format!("delete from {dest}")
            }
        }
    }
}

/// Making one compared file match the other side
pub struct RestoreOp {
    pub path: String,
    pub direction: RestoreDirection,
    pub action: RestoreAction,
    source: PathBuf,
    dest: PathBuf,
}

impl RestoreOp {
    /// Plan making a file's destination match its source: copy a source regular file or
    /// symlink, or delete a destination file whose source is missing. Returns `None` when
    /// neither applies, e.g. both are missing or one is a directory.
    pub fn plan(targets: &Targets, path: &str, direction: RestoreDirection) -> Option<Self> {
        let (left, right) = targets.pair(path);
        let (source, dest) = direction.source_and_dest(left, right);
        let action = match fs::symlink_metadata(&source) {
            Ok(metadata) if metadata.is_file() || metadata.is_symlink() => RestoreAction::Copy,
            Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::symlink_metadata(&dest) {
                Ok(metadata) if !metadata.is_dir() => RestoreAction::Delete,
                _ => return None,
            },
            _ => return None,
        };
        Some(Self {
            path: path.to_string(),
            direction,
            action,
            source,
            dest,
        })
    }

    pub fn mark(&self) -> RestoreMark {
        RestoreMark {
            direction: self.direction,
            action: self.action,
        }
    }

    pub fn execute(&self) -> io::Result<()> {
        match self.action {
            RestoreAction::Copy => self.copy(),
            RestoreAction::Delete => fs::remove_file(&self.dest),
        }
    }

    /// Replace the destination with a copy of the source, reflinking regular files where
    /// possible and preserving ownership (where permitted), permissions and
    /// access/modification times.
    ///
    /// The copy is made under a temporary name and renamed into place, so a failure never
    /// leaves a partial file. Missing parent directories are created.
    fn copy(&self) -> io::Result<()> {
        let metadata = fs::symlink_metadata(&self.source)?;
        let dest_dir = self.dest.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent")
        })?;
        fs::create_dir_all(dest_dir)?;

        let temp_builder = || {
            let mut builder = tempfile::Builder::new();
            builder.prefix(TEMP_PREFIX);
            builder
        };

        if metadata.is_symlink() {
            let link_target = fs::read_link(&self.source)?;
            let temp = temp_builder().make_in(dest_dir, |path| {
                std::os::unix::fs::symlink(&link_target, path)
            })?;
            preserve_ownership(&metadata, |uid, gid| {
                std::os::unix::fs::lchown(temp.path(), uid, gid)
            })?;
            set_symlink_times(temp.path(), &metadata)?;
            temp.persist(&self.dest).map_err(|e| e.error)?;
        } else if metadata.is_file() {
            let mut source = File::open(&self.source)?;
            let metadata = source.metadata()?;
            let mut temp = temp_builder().tempfile_in(dest_dir)?;
            if !reflink(&source, temp.as_file()) {
                io::copy(&mut source, temp.as_file_mut())?;
            }
            // Before setting permissions, since changing ownership clears setuid/setgid bits
            preserve_ownership(&metadata, |uid, gid| {
                std::os::unix::fs::fchown(temp.as_file(), uid, gid)
            })?;
            temp.as_file().set_permissions(metadata.permissions())?;
            // Linux doesn't allow setting ctime or birth time, so only these can be preserved
            temp.as_file().set_times(
                FileTimes::new()
                    .set_accessed(metadata.accessed()?)
                    .set_modified(metadata.modified()?),
            )?;
            temp.persist(&self.dest).map_err(|e| e.error)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "source is not a regular file or symlink",
            ));
        }
        Ok(())
    }
}

/// Give a copy the source's owner and group, like `cp -p`. Only root can give files away,
/// so if that isn't permitted, fall back to just the group, and failing that leave the
/// copy owned by the current user.
fn preserve_ownership(
    source: &Metadata,
    chown: impl Fn(Option<u32>, Option<u32>) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    // EPERM, or EINVAL for IDs the filesystem can't represent
    let not_permitted = |e: &io::Error| {
        matches!(
            e.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput
        )
    };
    match chown(Some(source.uid()), Some(source.gid())) {
        Err(e) if not_permitted(&e) => match chown(None, Some(source.gid())) {
            Err(e) if not_permitted(&e) => Ok(()),
            result => result,
        },
        result => result,
    }
}

/// Clone the contents of one file into another, sharing storage on filesystems that
/// support it (e.g. btrfs and XFS)
#[cfg(target_os = "linux")]
fn reflink(source: &File, dest: &File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: both descriptors are open for the duration of the call
    unsafe { libc::ioctl(dest.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) == 0 }
}

#[cfg(not(target_os = "linux"))]
fn reflink(_source: &File, _dest: &File) -> bool {
    false
}

/// Set a symlink's own access and modification times, which std can't do
fn set_symlink_times(path: &Path, metadata: &Metadata) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

    let path = CString::new(path.as_os_str().as_bytes())?;
    let times = [
        libc::timespec {
            tv_sec: metadata.atime() as _,
            tv_nsec: metadata.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: metadata.mtime() as _,
            tv_nsec: metadata.mtime_nsec() as _,
        },
    ];
    // SAFETY: path is NUL-terminated and times holds the two entries utimensat reads
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn is_missing(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// Operations to run, and the directories their deletions might leave empty
pub struct RestorePlan {
    ops: Vec<RestoreOp>,
    /// Deepest first, so a directory is considered after its subdirectories
    cleanup_dirs: Vec<CleanupDir>,
}

/// A directory to remove if it's left empty, unless the source side has it
struct CleanupDir {
    path: String,
    source: PathBuf,
    dest: PathBuf,
}

impl CleanupDir {
    fn is_on_source_side(&self) -> bool {
        !is_missing(&self.source)
    }
}

impl RestorePlan {
    pub fn new(targets: &Targets, ops: Vec<RestoreOp>) -> Self {
        let mut cleanup_dirs = BTreeMap::new();
        for op in ops.iter().filter(|op| op.action == RestoreAction::Delete) {
            let dirs = Path::new(&op.path)
                .ancestors()
                .skip(1)
                .filter(|dir| !dir.as_os_str().is_empty());
            for dir in dirs {
                let path = dir.to_string_lossy().to_string();
                let (left, right) = targets.pair(&path);
                let (source, dest) = op.direction.source_and_dest(left, right);
                cleanup_dirs
                    .entry(dest.clone())
                    .or_insert(CleanupDir { path, source, dest });
            }
        }
        let mut cleanup_dirs: Vec<CleanupDir> = cleanup_dirs.into_values().collect();
        cleanup_dirs.sort_by_key(|dir| Reverse(dir.dest.components().count()));
        Self { ops, cleanup_dirs }
    }

    /// Shell commands equivalent to executing the plan
    pub fn preview(&self) -> Vec<String> {
        let mut created_dirs = HashSet::new();
        let mut removed = HashSet::new();
        let mut lines = Vec::new();
        for op in &self.ops {
            match op.action {
                RestoreAction::Copy => {
                    if let Some(dir) = op.dest.parent() {
                        if !dir.exists() && created_dirs.insert(dir) {
                            lines.push(format!("mkdir -p {}", shell_quote(dir)));
                        }
                    }
                    lines.push(format!(
                        "cp --reflink=auto --no-dereference --preserve=mode,ownership,timestamps \
                         --remove-destination -- {} {}",
                        shell_quote(&op.source),
                        shell_quote(&op.dest)
                    ));
                }
                RestoreAction::Delete => {
                    lines.push(format!("rm -- {}", shell_quote(&op.dest)));
                    removed.insert(op.dest.clone());
                }
            }
        }

        // Directories whose contents will all have been deleted
        for dir in &self.cleanup_dirs {
            if dir.is_on_source_side() {
                continue;
            }
            let Ok(mut entries) = fs::read_dir(&dir.dest) else {
                continue;
            };
            if entries.all(|entry| entry.is_ok_and(|entry| removed.contains(&entry.path()))) {
                lines.push(format!("rmdir -- {}", shell_quote(&dir.dest)));
                removed.insert(dir.dest.clone());
            }
        }
        lines
    }
}

fn shell_quote(path: &Path) -> String {
    let path = path.to_string_lossy();
    let is_safe = |c: char| c.is_ascii_alphanumeric() || "/._-+,:@%=".contains(c);
    if !path.is_empty() && path.chars().all(is_safe) {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

/// Restore operations running on a background thread
pub struct RestoreJob {
    total: usize,
    completed: Arc<AtomicUsize>,
    cancelled: Arc<AtomicBool>,
    handle: JoinHandle<JobResults>,
}

struct JobResults {
    ops: Vec<(String, RestoreAction, io::Result<()>)>,
    removed_dirs: usize,
    dir_failures: Vec<(String, String)>,
}

pub struct RestoreOutcome {
    /// Files that now match the other side
    pub completed: Vec<String>,
    pub copied: usize,
    pub deleted: usize,
    pub removed_dirs: usize,
    pub failed: Vec<(String, String)>,
    /// Operations not attempted because the job was cancelled
    pub skipped: usize,
}

impl RestoreJob {
    pub fn start(plan: RestorePlan) -> Self {
        let total = plan.ops.len();
        let completed = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let handle = thread::spawn({
            let completed = completed.clone();
            let cancelled = cancelled.clone();
            move || {
                let mut results = JobResults {
                    ops: Vec::with_capacity(plan.ops.len()),
                    removed_dirs: 0,
                    dir_failures: Vec::new(),
                };
                for op in plan.ops {
                    if cancelled.load(Ordering::Relaxed) {
                        break;
                    }
                    let result = op.execute();
                    results.ops.push((op.path, op.action, result));
                    completed.fetch_add(1, Ordering::Relaxed);
                }

                // Remove directories that deletions left empty, if the source side lacks them
                for dir in plan.cleanup_dirs {
                    if dir.is_on_source_side() {
                        continue;
                    }
                    match fs::remove_dir(&dir.dest) {
                        Ok(()) => results.removed_dirs += 1,
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                            ) => {}
                        Err(e) => results
                            .dir_failures
                            .push((format!("{}/", dir.path), e.to_string())),
                    }
                }
                results
            }
        });
        Self {
            total,
            completed,
            cancelled,
            handle,
        }
    }

    /// Number of operations completed, and the total
    pub fn progress(&self) -> (usize, usize) {
        (self.completed.load(Ordering::Relaxed), self.total)
    }

    /// Stop after the operation in progress
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Wait for the job to finish and collect its results
    pub fn finish(self) -> RestoreOutcome {
        let mut outcome = RestoreOutcome {
            completed: Vec::new(),
            copied: 0,
            deleted: 0,
            removed_dirs: 0,
            failed: Vec::new(),
            skipped: 0,
        };
        let results = self.handle.join().unwrap_or_else(|_| {
            outcome
                .failed
                .push(("(all)".to_string(), "restore thread panicked".to_string()));
            JobResults {
                ops: Vec::new(),
                removed_dirs: 0,
                dir_failures: Vec::new(),
            }
        });
        outcome.skipped = self.total - results.ops.len();
        outcome.removed_dirs = results.removed_dirs;
        for (path, action, result) in results.ops {
            match result {
                Ok(()) => {
                    match action {
                        RestoreAction::Copy => outcome.copied += 1,
                        RestoreAction::Delete => outcome.deleted += 1,
                    }
                    outcome.completed.push(path);
                }
                Err(e) => outcome.failed.push((path, e.to_string())),
            }
        }
        outcome.failed.extend(results.dir_failures);
        outcome
    }
}

impl RestoreOutcome {
    pub fn summary(&self) -> Vec<String> {
        let total = self.completed.len() + self.failed.len() + self.skipped;
        let mut lines = vec![format!(
            "Copied {} and deleted {} of {total} files.",
            self.copied, self.deleted
        )];
        if self.removed_dirs > 0 {
            lines.push(format!("Removed {} empty directories.", self.removed_dirs));
        }
        if self.skipped > 0 {
            lines.push(format!("Cancelled before {} files.", self.skipped));
        }
        if !self.failed.is_empty() {
            lines.push(String::new());
            lines.push(format!("{} failed:", self.failed.len()));
            lines.extend(
                self.failed
                    .iter()
                    .map(|(path, error)| format!("  {path}: {error}")),
            );
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::time::{Duration, UNIX_EPOCH};

    struct Dirs {
        left: tempfile::TempDir,
        right: tempfile::TempDir,
        targets: Targets,
    }

    fn dirs() -> Dirs {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        let targets = Targets::resolve(
            &left.path().to_string_lossy(),
            &right.path().to_string_lossy(),
        )
        .unwrap();
        Dirs {
            left,
            right,
            targets,
        }
    }

    fn plan(dirs: &Dirs, path: &str, direction: RestoreDirection) -> RestoreOp {
        RestoreOp::plan(&dirs.targets, path, direction).unwrap()
    }

    /// Check a copy's metadata against its source's from before copying. Must be called
    /// before reading the copy, since reads can update access times.
    fn assert_same_metadata(source: &Metadata, dest: &Path) {
        let dest = fs::symlink_metadata(dest).unwrap();
        assert_eq!((dest.uid(), dest.gid()), (source.uid(), source.gid()));
        assert_eq!(dest.mode(), source.mode());
        assert_eq!(
            (dest.mtime(), dest.mtime_nsec()),
            (source.mtime(), source.mtime_nsec())
        );
        assert_eq!(
            (dest.atime(), dest.atime_nsec()),
            (source.atime(), source.atime_nsec())
        );
    }

    #[test]
    fn test_copy_file_into_missing_directory() {
        let dirs = dirs();
        let source = dirs.right.path().join("a/b/file");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "contents").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_accessed(UNIX_EPOCH + Duration::new(1_000, 123))
                    .set_modified(UNIX_EPOCH + Duration::new(2_000, 456)),
            )
            .unwrap();

        let source_metadata = fs::metadata(&source).unwrap();
        let op = plan(&dirs, "a/b/file", RestoreDirection::RightToLeft);
        assert_eq!(op.action, RestoreAction::Copy);
        op.execute().unwrap();

        let dest = dirs.left.path().join("a/b/file");
        assert_same_metadata(&source_metadata, &dest);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "contents");
    }

    #[test]
    fn test_copy_replaces_existing_file() {
        let dirs = dirs();
        fs::write(dirs.left.path().join("f"), "new").unwrap();
        fs::write(dirs.right.path().join("f"), "old contents").unwrap();
        let source_metadata = fs::metadata(dirs.left.path().join("f")).unwrap();

        plan(&dirs, "f", RestoreDirection::LeftToRight)
            .execute()
            .unwrap();

        assert_same_metadata(&source_metadata, &dirs.right.path().join("f"));
        assert_eq!(
            fs::read_to_string(dirs.right.path().join("f")).unwrap(),
            "new"
        );
        // No temporary files left behind
        assert_eq!(fs::read_dir(dirs.right.path()).unwrap().count(), 1);
    }

    #[test]
    fn test_copy_symlink() {
        let dirs = dirs();
        let source = dirs.left.path().join("link");
        std::os::unix::fs::symlink("some/target", &source).unwrap();
        let source_metadata = fs::symlink_metadata(&source).unwrap();

        plan(&dirs, "link", RestoreDirection::LeftToRight)
            .execute()
            .unwrap();

        let dest = dirs.right.path().join("link");
        assert_same_metadata(&source_metadata, &dest);
        assert_eq!(fs::read_link(&dest).unwrap(), Path::new("some/target"));
    }

    #[test]
    fn test_copy_does_not_replace_directory() {
        let dirs = dirs();
        fs::write(dirs.left.path().join("x"), "file").unwrap();
        fs::create_dir(dirs.right.path().join("x")).unwrap();
        fs::write(dirs.right.path().join("x/inner"), "keep").unwrap();

        let result = plan(&dirs, "x", RestoreDirection::LeftToRight).execute();

        assert!(result.is_err());
        assert!(dirs.right.path().join("x/inner").exists());
        assert_eq!(fs::read_dir(dirs.right.path()).unwrap().count(), 1);
    }

    #[test]
    fn test_preserve_ownership_falls_back_to_group() {
        let source = fs::metadata(tempfile::tempdir().unwrap().path()).unwrap();
        let denied = || Err(io::Error::from(io::ErrorKind::PermissionDenied));

        // Owner and group not permitted, so only the group is changed
        let calls = std::cell::RefCell::new(Vec::new());
        preserve_ownership(&source, |uid, gid| {
            calls.borrow_mut().push((uid, gid));
            if uid.is_some() { denied() } else { Ok(()) }
        })
        .unwrap();
        assert_eq!(
            calls.into_inner(),
            [
                (Some(source.uid()), Some(source.gid())),
                (None, Some(source.gid()))
            ]
        );

        // Neither permitted: not an error
        preserve_ownership(&source, |_, _| denied()).unwrap();

        // Other errors are reported
        let error = preserve_ownership(&source, |_, _| {
            Err(io::Error::from(io::ErrorKind::NotFound))
        });
        assert_eq!(error.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_plan_actions() {
        let dirs = dirs();
        fs::write(dirs.left.path().join("only-left"), "x").unwrap();
        fs::create_dir(dirs.left.path().join("dir")).unwrap();
        let action =
            |path, direction| RestoreOp::plan(&dirs.targets, path, direction).map(|op| op.action);

        assert_eq!(
            action("only-left", RestoreDirection::LeftToRight),
            Some(RestoreAction::Copy)
        );
        assert_eq!(
            action("only-left", RestoreDirection::RightToLeft),
            Some(RestoreAction::Delete)
        );
        assert_eq!(action("nowhere", RestoreDirection::LeftToRight), None);
        // Directories are neither copied nor deleted
        assert_eq!(action("dir", RestoreDirection::LeftToRight), None);
        assert_eq!(action("dir", RestoreDirection::RightToLeft), None);
    }

    #[test]
    fn test_delete_removes_emptied_directories() {
        let dirs = dirs();
        let left = dirs.left.path();
        for file in ["gone/deep/f1", "gone/deep/f2", "gone/unmarked", "shared/f3"] {
            fs::create_dir_all(left.join(file).parent().unwrap()).unwrap();
            fs::write(left.join(file), "x").unwrap();
        }
        fs::create_dir(dirs.right.path().join("shared")).unwrap();

        let ops = ["gone/deep/f1", "gone/deep/f2", "shared/f3"]
            .map(|path| plan(&dirs, path, RestoreDirection::RightToLeft))
            .into_iter()
            .collect();
        let restore_plan = RestorePlan::new(&dirs.targets, ops);
        let left = fs::canonicalize(left)
            .unwrap()
            .to_string_lossy()
            .to_string();

        assert_eq!(
            restore_plan.preview(),
            [
                format!("rm -- {left}/gone/deep/f1"),
                format!("rm -- {left}/gone/deep/f2"),
                format!("rm -- {left}/shared/f3"),
                format!("rmdir -- {left}/gone/deep"),
            ]
        );

        let outcome = run(restore_plan);
        assert_eq!(outcome.deleted, 3);
        assert_eq!(outcome.removed_dirs, 1);
        assert!(outcome.failed.is_empty());
        let left = Path::new(&left);
        // Still has an unmarked file
        assert!(left.join("gone/unmarked").exists());
        assert!(!left.join("gone/deep").exists());
        // Empty, but exists on the right
        assert!(left.join("shared").exists());
    }

    fn run(plan: RestorePlan) -> RestoreOutcome {
        let job = RestoreJob::start(plan);
        while !job.is_finished() {
            thread::sleep(Duration::from_millis(1));
        }
        let (completed, total) = job.progress();
        assert_eq!(completed, total);
        job.finish()
    }

    #[test]
    fn test_job_reports_results() {
        let dirs = dirs();
        fs::write(dirs.left.path().join("ok"), "x").unwrap();
        fs::write(dirs.left.path().join("blocked"), "x").unwrap();
        fs::create_dir(dirs.right.path().join("blocked")).unwrap();
        fs::write(dirs.right.path().join("extra"), "x").unwrap();
        let ops = [
            plan(&dirs, "blocked", RestoreDirection::LeftToRight),
            plan(&dirs, "extra", RestoreDirection::LeftToRight),
            plan(&dirs, "ok", RestoreDirection::LeftToRight),
        ];

        let outcome = run(RestorePlan::new(&dirs.targets, ops.into()));

        assert_eq!(outcome.completed, ["extra", "ok"]);
        assert_eq!((outcome.copied, outcome.deleted), (1, 1));
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, "blocked");
        assert_eq!(outcome.skipped, 0);
        assert!(!dirs.right.path().join("extra").exists());
    }

    #[test]
    fn test_preview_copies() {
        let dirs = dirs();
        fs::create_dir(dirs.left.path().join("new dir")).unwrap();
        let paths = ["new dir/it's", "new dir/other", "top"];
        for path in paths {
            fs::write(dirs.left.path().join(path), "x").unwrap();
        }
        let ops = paths.map(|path| plan(&dirs, path, RestoreDirection::LeftToRight));
        let [left, right] = [&dirs.left, &dirs.right].map(|dir| {
            fs::canonicalize(dir.path())
                .unwrap()
                .to_string_lossy()
                .to_string()
        });
        let cp = "cp --reflink=auto --no-dereference --preserve=mode,ownership,timestamps \
                  --remove-destination --";

        assert_eq!(
            RestorePlan::new(&dirs.targets, ops.into()).preview(),
            [
                format!("mkdir -p '{right}/new dir'"),
                format!("{cp} '{left}/new dir/it'\\''s' '{right}/new dir/it'\\''s'"),
                format!("{cp} '{left}/new dir/other' '{right}/new dir/other'"),
                format!("{cp} {left}/top {right}/top"),
            ]
        );
    }
}
