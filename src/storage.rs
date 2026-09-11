use serde::Serialize;
use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const ENTRY_LIMIT: u64 = 10_000;
const TIME_LIMIT: Duration = Duration::from_secs(2);
const DEPTH_LIMIT: usize = 64;
const ERROR_LIMIT: usize = 32;

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub host_available_bytes: Option<u64>,
    pub vm_directory: DirectoryUsage,
    pub backing_images: Vec<ImageUsage>,
    pub shared_lima_cache: Option<DirectoryUsage>,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DirectoryUsage {
    pub path: PathBuf,
    pub exists: Option<bool>,
    pub root_uid: Option<u32>,
    pub root_gid: Option<u32>,
    pub complete: bool,
    pub file_logical_bytes: u64,
    pub file_allocated_bytes: u64,
    pub files: u64,
    pub entries_seen: u64,
    pub hardlinks_skipped: u64,
    pub symlinks_skipped: u64,
    pub other_entries_skipped: u64,
    pub entry_limit: u64,
    pub time_limit_ms: u64,
    pub errors: Vec<String>,
    pub additional_errors: u64,
}

#[derive(Debug, Serialize)]
pub struct ImageUsage {
    pub path: PathBuf,
    pub exists: Option<bool>,
    pub logical_bytes: Option<u64>,
    pub allocated_bytes: Option<u64>,
    pub hard_link_count: Option<u64>,
    pub error: Option<String>,
}

/// Read-only accounting. Callers must validate the state ownership marker first.
/// Allocated bytes are stat block counts, not uniquely owned APFS storage.
pub fn inspect(state: &Path) -> Report {
    let cache = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Caches/lima"));
    inspect_paths(state, cache.as_deref())
}

fn inspect_paths(state: &Path, cache: Option<&Path>) -> Report {
    let vm = state.join("lima/luhmen");
    let mut errors = Vec::new();
    let host_available_bytes = available_space(state)
        .map_err(|error| {
            errors.push(format!("host available space: {error}"));
        })
        .ok();
    let shared_lima_cache = cache.map(|path| scan(path, ENTRY_LIMIT, TIME_LIMIT));
    if cache.is_none() {
        errors.push("HOME is not set; shared Lima cache location is unknown".into());
    }
    Report {
        schema_version: 1,
        host_available_bytes,
        vm_directory: scan(&vm, ENTRY_LIMIT, TIME_LIMIT),
        backing_images: ["disk", "image", "basedisk", "diffdisk"]
            .into_iter()
            .map(|name| inspect_image(&vm.join(name)))
            .collect(),
        shared_lima_cache,
        errors,
    }
}

fn inspect_image(path: &Path) -> ImageUsage {
    let mut image = ImageUsage {
        path: path.into(),
        exists: None,
        logical_bytes: None,
        allocated_bytes: None,
        hard_link_count: None,
        error: None,
    };
    match checked_metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            image.exists = Some(true);
            image.logical_bytes = Some(metadata.len());
            image.allocated_bytes = metadata.blocks().checked_mul(512);
            image.hard_link_count = Some(metadata.nlink());
            if image.allocated_bytes.is_none() {
                image.error = Some("allocated byte count exceeds u64".into());
            }
        }
        Ok(_) => {
            image.exists = Some(true);
            image.error = Some("backing image is not a regular file".into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => image.exists = Some(false),
        Err(error) => image.error = Some(error.to_string()),
    }
    image
}

fn available_space(path: &Path) -> io::Result<u64> {
    for ancestor in path.ancestors() {
        match checked_metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() => return fs2::available_space(ancestor),
            Ok(_) => return Err(io::Error::other("state ancestor is not a directory")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other("no existing state directory ancestor"))
}

// Check every component, since symlink_metadata alone follows intermediate links.
// This is diagnostic accounting, not an atomic filesystem snapshot. Recheck before
// each descent to reject links introduced between directory entries.
fn checked_metadata(path: &Path) -> io::Result<Metadata> {
    if !path.is_absolute() {
        return Err(io::Error::other("storage path must be absolute"));
    }
    let mut current = PathBuf::new();
    let mut last = None;
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(io::Error::other("storage path cannot contain '..'"));
        }
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::other(format!(
                "refusing symlink in storage path: {}",
                current.display()
            )));
        }
        last = Some(metadata);
    }
    last.ok_or_else(|| io::Error::other("empty storage path"))
}

fn scan(path: &Path, entry_limit: u64, time_limit: Duration) -> DirectoryUsage {
    let mut walk = Walk {
        result: DirectoryUsage {
            path: path.into(),
            exists: None,
            root_uid: None,
            root_gid: None,
            complete: true,
            file_logical_bytes: 0,
            file_allocated_bytes: 0,
            files: 0,
            entries_seen: 0,
            hardlinks_skipped: 0,
            symlinks_skipped: 0,
            other_entries_skipped: 0,
            entry_limit,
            time_limit_ms: time_limit.as_millis().try_into().unwrap_or(u64::MAX),
            errors: Vec::new(),
            additional_errors: 0,
        },
        seen: HashSet::new(),
        start: Instant::now(),
        time_limit,
        stopped: false,
    };
    match checked_metadata(path) {
        Ok(metadata) => {
            walk.result.exists = Some(true);
            walk.result.root_uid = Some(metadata.uid());
            walk.result.root_gid = Some(metadata.gid());
            if metadata.is_dir() {
                walk.directory(path, metadata.dev(), 0);
            } else {
                walk.error(path, "storage root is not a directory");
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => walk.result.exists = Some(false),
        Err(error) => walk.error(path, &error.to_string()),
    }
    walk.result
}

struct Walk {
    result: DirectoryUsage,
    seen: HashSet<(u64, u64)>,
    start: Instant,
    time_limit: Duration,
    stopped: bool,
}

impl Walk {
    fn error(&mut self, path: &Path, message: &str) {
        self.result.complete = false;
        if self.result.errors.len() < ERROR_LIMIT {
            self.result
                .errors
                .push(format!("{}: {message}", path.display()));
        } else {
            self.result.additional_errors += 1;
        }
    }

    fn within_budget(&mut self, path: &Path) -> bool {
        if self.stopped {
            return false;
        }
        let reason = if self.result.entries_seen >= self.result.entry_limit {
            Some("entry limit reached; totals are partial")
        } else if self.start.elapsed() >= self.time_limit {
            Some("scan time limit reached; totals are partial")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.error(path, reason);
            self.stopped = true;
        }
        !self.stopped
    }

    fn directory(&mut self, path: &Path, device: u64, depth: usize) {
        if !self.within_budget(path) {
            return;
        }
        if depth > DEPTH_LIMIT {
            self.error(path, "directory depth limit reached; totals are partial");
            return;
        }
        match checked_metadata(path) {
            Ok(metadata) if metadata.is_dir() && metadata.dev() == device => {
                if !self.seen.insert((metadata.dev(), metadata.ino())) {
                    self.error(path, "directory already visited; totals are partial");
                    return;
                }
            }
            Ok(_) => {
                self.error(
                    path,
                    "directory changed or crosses a filesystem; totals are partial",
                );
                return;
            }
            Err(error) => {
                self.error(path, &error.to_string());
                return;
            }
        }
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                self.error(path, &error.to_string());
                return;
            }
        };
        for entry in entries {
            if !self.within_budget(path) {
                break;
            }
            self.result.entries_seen += 1;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.error(path, &error.to_string());
                    continue;
                }
            };
            let entry_path = entry.path();
            let metadata = match fs::symlink_metadata(&entry_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.error(&entry_path, &error.to_string());
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                self.result.symlinks_skipped += 1;
            } else if metadata.dev() != device {
                self.error(
                    &entry_path,
                    "different filesystem skipped; totals are partial",
                );
            } else if metadata.is_dir() {
                self.directory(&entry_path, device, depth + 1);
            } else if metadata.is_file() {
                if !self.seen.insert((metadata.dev(), metadata.ino())) {
                    self.result.hardlinks_skipped += 1;
                    continue;
                }
                let logical = self.result.file_logical_bytes.checked_add(metadata.len());
                let allocated = metadata
                    .blocks()
                    .checked_mul(512)
                    .and_then(|bytes| self.result.file_allocated_bytes.checked_add(bytes));
                if let (Some(logical), Some(allocated)) = (logical, allocated) {
                    self.result.file_logical_bytes = logical;
                    self.result.file_allocated_bytes = allocated;
                    self.result.files += 1;
                } else {
                    self.error(&entry_path, "byte count exceeds u64; totals are partial");
                }
            } else {
                self.result.other_entries_skipped += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn sparse_images_report_length_and_actual_blocks_without_reading_contents() {
        let temp = root();
        let path = fs::canonicalize(temp.path()).unwrap();
        let vm = path.join("lima/luhmen");
        fs::create_dir_all(&vm).unwrap();
        let image = vm.join("disk");
        let mut file = File::create(&image).unwrap();
        file.set_len(64 * 1024 * 1024).unwrap();
        file.seek(SeekFrom::Start(32 * 1024 * 1024)).unwrap();
        file.write_all(b"persistent sentinel").unwrap();
        file.sync_all().unwrap();
        let metadata = file.metadata().unwrap();
        let report = inspect_paths(&path, Some(&path.join("absent-cache")));
        assert_eq!(
            report.backing_images[0].logical_bytes,
            Some(64 * 1024 * 1024)
        );
        assert_eq!(
            report.backing_images[0].allocated_bytes,
            Some(metadata.blocks() * 512)
        );
        assert_eq!(
            report.vm_directory.file_allocated_bytes,
            metadata.blocks() * 512
        );
        assert_eq!(report.vm_directory.file_logical_bytes, metadata.len());
        assert!(report.vm_directory.complete);
        assert!(report.host_available_bytes.is_some());
        assert_eq!(report.backing_images[1].exists, Some(false));
    }

    #[test]
    fn hardlinks_are_counted_once_and_symlink_targets_are_excluded() {
        let temp = root();
        let path = fs::canonicalize(temp.path()).unwrap();
        let cache = path.join("cache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("file"), b"cache bytes").unwrap();
        fs::hard_link(cache.join("file"), cache.join("same-file")).unwrap();
        fs::write(path.join("outside"), [7; 2048]).unwrap();
        symlink(path.join("outside"), cache.join("file-link")).unwrap();
        symlink(&path, cache.join("directory-link")).unwrap();
        let result = scan(&cache, ENTRY_LIMIT, TIME_LIMIT);
        assert!(result.complete);
        assert_eq!(result.file_logical_bytes, 11);
        assert_eq!(result.files, 1);
        assert_eq!(result.hardlinks_skipped, 1);
        assert_eq!(result.symlinks_skipped, 2);
        assert_eq!(
            result.file_allocated_bytes,
            fs::metadata(cache.join("file")).unwrap().blocks() * 512
        );
    }

    #[test]
    fn linked_roots_and_ancestors_are_refused() {
        let temp = root();
        let path = fs::canonicalize(temp.path()).unwrap();
        fs::create_dir_all(path.join("real/cache")).unwrap();
        fs::write(path.join("real/cache/keep"), b"keep").unwrap();
        symlink(path.join("real"), path.join("linked")).unwrap();
        for candidate in [path.join("linked"), path.join("linked/cache")] {
            let result = scan(&candidate, ENTRY_LIMIT, TIME_LIMIT);
            assert!(!result.complete);
            assert_eq!(result.file_logical_bytes, 0);
            assert!(result.errors[0].contains("symlink"));
        }
        let image = inspect_image(&path.join("linked/cache/keep"));
        assert!(image.logical_bytes.is_none());
        assert!(image.error.unwrap().contains("symlink"));
    }

    #[test]
    fn entry_and_time_limits_report_partial_totals() {
        let temp = root();
        let path = fs::canonicalize(temp.path()).unwrap();
        for name in ["one", "two", "three"] {
            fs::write(path.join(name), b"abc").unwrap();
        }
        let result = scan(&path, 2, TIME_LIMIT);
        assert!(!result.complete);
        assert_eq!(result.entries_seen, 2);
        assert_eq!(result.file_logical_bytes, 6);
        assert!(result.errors[0].contains("entry limit"));
        let timed = scan(&path, ENTRY_LIMIT, Duration::ZERO);
        assert!(!timed.complete);
        assert_eq!(timed.entries_seen, 0);
        assert!(timed.errors[0].contains("time limit"));
    }

    #[test]
    fn absent_directory_is_distinct_from_unreadable_directory() {
        let temp = root();
        let path = fs::canonicalize(temp.path()).unwrap();
        let absent = scan(&path.join("absent"), ENTRY_LIMIT, TIME_LIMIT);
        assert_eq!(absent.exists, Some(false));
        assert!(absent.complete);
        let denied = path.join("denied");
        fs::create_dir(&denied).unwrap();
        fs::write(denied.join("keep"), b"keep").unwrap();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::read_dir(&denied).is_ok();
        let result = scan(&denied, ENTRY_LIMIT, TIME_LIMIT);
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
        if !readable {
            assert_eq!(result.exists, Some(true));
            assert!(!result.complete);
            assert!(!result.errors.is_empty());
            assert_eq!(result.file_logical_bytes, 0);
        }
    }
}
