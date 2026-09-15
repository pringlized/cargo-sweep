//! Fixtures for building fake target dirs with controlled timestamps.

use std::fs::{self, File, FileTimes};
use std::path::Path;
use std::time::{Duration, SystemTime};

pub const DAY: Duration = Duration::from_secs(24 * 60 * 60);

pub fn days_ago(days: u64) -> SystemTime {
    SystemTime::now() - DAY * days as u32
}

/// Writes `path` (creating parents) with the given access and modification ages, in days.
pub fn file(path: &Path, accessed_days_ago: u64, modified_days_ago: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"build output").unwrap();
    set_times(path, accessed_days_ago, modified_days_ago);
}

/// Sets both timestamps of an existing file or directory. Directories need this after their
/// children are created, because creating a child bumps the parent's mtime.
pub fn set_times(path: &Path, accessed_days_ago: u64, modified_days_ago: u64) {
    let times = FileTimes::new()
        .set_accessed(days_ago(accessed_days_ago))
        .set_modified(days_ago(modified_days_ago));
    File::open(path).unwrap().set_times(times).unwrap();
}

/// Ages every directory under `root` (inclusive) to `days` old, deepest first.
pub fn age_dirs(root: &Path, days: u64) {
    for entry in walkdir::WalkDir::new(root).contents_first(true) {
        let entry = entry.unwrap();
        if entry.file_type().is_dir() {
            set_times(entry.path(), days, days);
        }
    }
}
