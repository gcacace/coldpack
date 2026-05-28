#![allow(dead_code)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use walkdir::WalkDir;

use crate::config::SourceConfig;
use crate::manifest::{FileEntry, Manifest};

const FINGERPRINT_READ_SIZE: usize = 65536; // 64KB

#[derive(Debug, Clone, PartialEq)]
pub enum FileChange {
    New {
        logical_path: String,
        disk_path: std::path::PathBuf,
        size: u64,
        mtime: DateTime<Utc>,
        fingerprint: String,
    },
    Modified {
        logical_path: String,
        disk_path: std::path::PathBuf,
        size: u64,
        mtime: DateTime<Utc>,
        fingerprint: String,
        previous_archive_id: String,
    },
    Moved {
        logical_path: String,
        old_path: String,
        fingerprint: String,
    },
    Deleted {
        logical_path: String,
    },
}

#[derive(Debug)]
pub struct ScanResult {
    pub changes: Vec<FileChange>,
    pub stats: ScanStats,
}

#[derive(Debug, Default)]
pub struct ScanStats {
    pub total_files_scanned: u64,
    pub skipped_by_cutoff: u64,
    pub skipped_by_exclude: u64,
    pub unchanged: u64,
    pub new: u64,
    pub modified: u64,
    pub moved: u64,
    pub deleted: u64,
}

pub fn compute_fingerprint(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("Failed to open: {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("Failed to read metadata: {}", path.display()))?;
    let size = metadata.len();

    let mut buf = vec![0u8; FINGERPRINT_READ_SIZE.min(size as usize)];
    let bytes_read = file
        .read(&mut buf)
        .with_context(|| format!("Failed to read: {}", path.display()))?;
    buf.truncate(bytes_read);

    let hash = xxhash_rust::xxh3::xxh3_64(&buf);
    Ok(format!("{:016x}-{}", hash, size))
}

fn should_exclude(path: &Path, source_root: &Path, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let relative = path.strip_prefix(source_root).unwrap_or(path);
    for component in relative.components() {
        let name = component.as_os_str().to_string_lossy();
        for pattern in patterns {
            if pattern.contains('*') {
                if glob_match_simple(&name, pattern) {
                    return true;
                }
            } else if name == *pattern {
                return true;
            }
        }
    }
    false
}

fn glob_match_simple(text: &str, pattern: &str) -> bool {
    let t = text.as_bytes();
    let p = pattern.as_bytes();
    let mut ti = 0;
    let mut pi = 0;
    let mut star_ti: Option<usize> = None;
    let mut star_pi: Option<usize> = None;

    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star_ti = Some(ti);
            star_pi = Some(pi + 1);
            pi += 1;
        } else if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if let (Some(sti), Some(spi)) = (star_ti, star_pi) {
            let new_sti = sti + 1;
            star_ti = Some(new_sti);
            ti = new_sti;
            pi = spi;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

pub fn scan(
    sources: &[SourceConfig],
    manifest: &Manifest,
    cutoff: Option<DateTime<Utc>>,
    exclude: &[String],
) -> Result<ScanResult> {
    let mut stats = ScanStats::default();
    let mut changes = Vec::new();

    let file_index: HashMap<&str, &FileEntry> = manifest.file_index();
    let fingerprint_index: HashMap<&str, &FileEntry> = manifest.fingerprint_index();

    // Track which manifest paths we've seen on disk (for deletion detection)
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    for source in sources {
        if !source.path.exists() {
            anyhow::bail!(
                "Source path does not exist: {} (name: '{}')",
                source.path.display(),
                source.name
            );
        }

        for entry in WalkDir::new(&source.path).follow_links(true) {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    eprintln!("  Warning: skipping inaccessible path: {}", err);
                    continue;
                }
            };

            if should_exclude(entry.path(), &source.path, exclude) {
                if entry.file_type().is_file() {
                    stats.skipped_by_exclude += 1;
                }
                continue;
            }

            if !entry.file_type().is_file() {
                continue;
            }

            stats.total_files_scanned += 1;

            let disk_path = entry.path().to_path_buf();
            let relative = disk_path
                .strip_prefix(&source.path)
                .with_context(|| "Failed to compute relative path")?;
            let logical_path = format!(
                "{}/{}",
                source.name,
                relative.to_string_lossy().replace('\\', "/")
            );

            let metadata = entry.metadata().with_context(|| {
                format!("Failed to read metadata: {}", disk_path.display())
            })?;
            let size = metadata.len();
            let mtime: DateTime<Utc> = metadata
                .modified()
                .with_context(|| "Failed to read mtime")?
                .into();

            // Apply cutoff filter
            if let Some(cutoff_dt) = cutoff {
                if mtime >= cutoff_dt {
                    stats.skipped_by_cutoff += 1;
                    continue;
                }
            }

            seen_paths.insert(logical_path.clone());

            // Fast path: check if path exists in manifest with same mtime + size
            if let Some(existing) = file_index.get(logical_path.as_str()) {
                let existing_mtime_secs = existing.mtime.timestamp();
                let current_mtime_secs = mtime.timestamp();

                if existing.size == size && existing_mtime_secs == current_mtime_secs {
                    stats.unchanged += 1;
                    continue;
                }

                // Modified: same path, different content
                let fingerprint = compute_fingerprint(&disk_path)?;
                stats.modified += 1;
                changes.push(FileChange::Modified {
                    logical_path,
                    disk_path,
                    size,
                    mtime,
                    fingerprint,
                    previous_archive_id: existing.archive_id.clone(),
                });
                continue;
            }

            // Path not in manifest — might be new or might be a move
            let fingerprint = compute_fingerprint(&disk_path)?;

            if let Some(existing) = fingerprint_index.get(fingerprint.as_str()) {
                // Same content exists at a different path — this is a move
                stats.moved += 1;
                changes.push(FileChange::Moved {
                    logical_path,
                    old_path: existing.path.clone(),
                    fingerprint,
                });
            } else {
                // Genuinely new file
                stats.new += 1;
                changes.push(FileChange::New {
                    logical_path,
                    disk_path,
                    size,
                    mtime,
                    fingerprint,
                });
            }
        }
    }

    // Deletion detection: manifest entries not seen on disk and not matched by a move
    let moved_from_paths: std::collections::HashSet<String> = changes
        .iter()
        .filter_map(|c| match c {
            FileChange::Moved { old_path, .. } => Some(old_path.clone()),
            _ => None,
        })
        .collect();

    for file_entry in &manifest.files {
        if !seen_paths.contains(&file_entry.path) && !moved_from_paths.contains(&file_entry.path)
        {
            stats.deleted += 1;
            changes.push(FileChange::Deleted {
                logical_path: file_entry.path.clone(),
            });
        }
    }

    Ok(ScanResult { changes, stats })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_source(dir: &TempDir, name: &str) -> SourceConfig {
        SourceConfig {
            name: name.to_string(),
            path: dir.path().to_path_buf(),
        }
    }

    fn create_file(dir: &Path, relative_path: &str, content: &[u8]) -> PathBuf {
        let full_path = dir.join(relative_path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full_path, content).unwrap();
        full_path
    }

    fn empty_manifest() -> Manifest {
        Manifest::new()
    }

    #[test]
    fn test_compute_fingerprint_deterministic() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jpg");
        fs::write(&path, b"hello world this is a test file").unwrap();

        let fp1 = compute_fingerprint(&path).unwrap();
        let fp2 = compute_fingerprint(&path).unwrap();
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_compute_fingerprint_different_content() {
        let dir = TempDir::new().unwrap();
        let path1 = dir.path().join("a.jpg");
        let path2 = dir.path().join("b.jpg");
        fs::write(&path1, b"content A").unwrap();
        fs::write(&path2, b"content B").unwrap();

        let fp1 = compute_fingerprint(&path1).unwrap();
        let fp2 = compute_fingerprint(&path2).unwrap();
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_compute_fingerprint_same_content_different_path() {
        let dir = TempDir::new().unwrap();
        let path1 = dir.path().join("a.jpg");
        let path2 = dir.path().join("b.jpg");
        fs::write(&path1, b"same content").unwrap();
        fs::write(&path2, b"same content").unwrap();

        let fp1 = compute_fingerprint(&path1).unwrap();
        let fp2 = compute_fingerprint(&path2).unwrap();
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_compute_fingerprint_includes_size() {
        let dir = TempDir::new().unwrap();
        // Same first 64KB prefix but different total sizes won't happen naturally with small files,
        // but verify the format includes size
        let path = dir.path().join("test.jpg");
        fs::write(&path, b"hello").unwrap();

        let fp = compute_fingerprint(&path).unwrap();
        assert!(fp.ends_with("-5"), "fingerprint should end with file size");
    }

    #[test]
    fn test_scan_empty_dir_empty_manifest() {
        let dir = TempDir::new().unwrap();
        let sources = vec![make_source(&dir, "photos")];
        let manifest = empty_manifest();

        let result = scan(&sources, &manifest, None, &[]).unwrap();
        assert_eq!(result.stats.total_files_scanned, 0);
        assert!(result.changes.is_empty());
    }

    #[test]
    fn test_scan_new_files() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "2026/05/photo1.jpg", b"photo data 1");
        create_file(dir.path(), "2026/05/photo2.jpg", b"photo data 2");

        let sources = vec![make_source(&dir, "marco")];
        let manifest = empty_manifest();

        let result = scan(&sources, &manifest, None, &[]).unwrap();
        assert_eq!(result.stats.new, 2);
        assert_eq!(result.stats.total_files_scanned, 2);

        let new_paths: Vec<&str> = result
            .changes
            .iter()
            .filter_map(|c| match c {
                FileChange::New { logical_path, .. } => Some(logical_path.as_str()),
                _ => None,
            })
            .collect();
        assert!(new_paths.contains(&"marco/2026/05/photo1.jpg"));
        assert!(new_paths.contains(&"marco/2026/05/photo2.jpg"));
    }

    #[test]
    fn test_scan_unchanged_files() {
        let dir = TempDir::new().unwrap();
        let path = create_file(dir.path(), "photo.jpg", b"photo data");
        let metadata = fs::metadata(&path).unwrap();
        let mtime: DateTime<Utc> = metadata.modified().unwrap().into();

        let sources = vec![make_source(&dir, "marco")];
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "marco/photo.jpg".to_string(),
                size: metadata.len(),
                mtime,
                fingerprint: "doesnt-matter".to_string(),
                archive_id: "a1".to_string(),
                history: vec![],
            }],
        };

        let result = scan(&sources, &manifest, None, &[]).unwrap();
        assert_eq!(result.stats.unchanged, 1);
        assert_eq!(result.stats.new, 0);
        assert!(result.changes.is_empty());
    }

    #[test]
    fn test_scan_modified_file() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "photo.jpg", b"new content that is different");

        let sources = vec![make_source(&dir, "marco")];
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "marco/photo.jpg".to_string(),
                size: 999, // different size
                mtime: Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap(),
                fingerprint: "old-fp".to_string(),
                archive_id: "a1".to_string(),
                history: vec![],
            }],
        };

        let result = scan(&sources, &manifest, None, &[]).unwrap();
        assert_eq!(result.stats.modified, 1);
        assert!(matches!(&result.changes[0], FileChange::Modified { logical_path, .. } if logical_path == "marco/photo.jpg"));
    }

    #[test]
    fn test_scan_moved_file() {
        let dir = TempDir::new().unwrap();
        let content = b"unique photo content for move detection";
        create_file(dir.path(), "new-folder/photo.jpg", content);

        // Compute what the fingerprint will be
        let fp = compute_fingerprint(&dir.path().join("new-folder/photo.jpg")).unwrap();

        let sources = vec![make_source(&dir, "marco")];
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "marco/old-folder/photo.jpg".to_string(),
                size: content.len() as u64,
                mtime: Utc::now(),
                fingerprint: fp.clone(),
                archive_id: "a1".to_string(),
                history: vec![],
            }],
        };

        let result = scan(&sources, &manifest, None, &[]).unwrap();
        assert_eq!(result.stats.moved, 1);
        assert!(matches!(
            &result.changes[0],
            FileChange::Moved { logical_path, old_path, .. }
            if logical_path == "marco/new-folder/photo.jpg" && old_path == "marco/old-folder/photo.jpg"
        ));
    }

    #[test]
    fn test_scan_deleted_file() {
        let dir = TempDir::new().unwrap();
        // Dir is empty — file in manifest no longer exists

        let sources = vec![make_source(&dir, "marco")];
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "marco/deleted-photo.jpg".to_string(),
                size: 100,
                mtime: Utc::now(),
                fingerprint: "some-fp".to_string(),
                archive_id: "a1".to_string(),
                history: vec![],
            }],
        };

        let result = scan(&sources, &manifest, None, &[]).unwrap();
        assert_eq!(result.stats.deleted, 1);
        assert!(matches!(
            &result.changes[0],
            FileChange::Deleted { logical_path } if logical_path == "marco/deleted-photo.jpg"
        ));
    }

    #[test]
    fn test_scan_move_does_not_cause_deletion() {
        let dir = TempDir::new().unwrap();
        let content = b"moved file content";
        create_file(dir.path(), "new-location/photo.jpg", content);

        let fp = compute_fingerprint(&dir.path().join("new-location/photo.jpg")).unwrap();

        let sources = vec![make_source(&dir, "marco")];
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "marco/old-location/photo.jpg".to_string(),
                size: content.len() as u64,
                mtime: Utc::now(),
                fingerprint: fp,
                archive_id: "a1".to_string(),
                history: vec![],
            }],
        };

        let result = scan(&sources, &manifest, None, &[]).unwrap();
        assert_eq!(result.stats.moved, 1);
        assert_eq!(result.stats.deleted, 0);
        // The old path shouldn't show as deleted since it was identified as the source of a move
    }

    #[test]
    fn test_scan_cutoff_filters_recent_files() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "recent.jpg", b"recent photo");
        create_file(dir.path(), "old.jpg", b"old photo");

        // Set old file's mtime to the past
        let old_path = dir.path().join("old.jpg");
        let past_time = filetime::FileTime::from_unix_time(1_600_000_000, 0); // Sept 2020
        filetime::set_file_mtime(&old_path, past_time).unwrap();

        let sources = vec![make_source(&dir, "marco")];
        let manifest = empty_manifest();

        // Cutoff: only files before 2025-01-01
        let cutoff = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let result = scan(&sources, &manifest, Some(cutoff), &[]).unwrap();

        assert_eq!(result.stats.total_files_scanned, 2);
        assert_eq!(result.stats.skipped_by_cutoff, 1);
        assert_eq!(result.stats.new, 1);

        let new_path = match &result.changes[0] {
            FileChange::New { logical_path, .. } => logical_path.as_str(),
            _ => panic!("Expected New change"),
        };
        assert_eq!(new_path, "marco/old.jpg");
    }

    #[test]
    fn test_scan_multiple_sources() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        create_file(dir1.path(), "photo1.jpg", b"marco photo");
        create_file(dir2.path(), "photo2.jpg", b"laura photo");

        let sources = vec![
            SourceConfig {
                name: "marco".to_string(),
                path: dir1.path().to_path_buf(),
            },
            SourceConfig {
                name: "laura".to_string(),
                path: dir2.path().to_path_buf(),
            },
        ];
        let manifest = empty_manifest();

        let result = scan(&sources, &manifest, None, &[]).unwrap();
        assert_eq!(result.stats.new, 2);

        let paths: Vec<&str> = result
            .changes
            .iter()
            .filter_map(|c| match c {
                FileChange::New { logical_path, .. } => Some(logical_path.as_str()),
                _ => None,
            })
            .collect();
        assert!(paths.contains(&"marco/photo1.jpg"));
        assert!(paths.contains(&"laura/photo2.jpg"));
    }

    #[test]
    fn test_scan_nonexistent_source_fails() {
        let sources = vec![SourceConfig {
            name: "missing".to_string(),
            path: PathBuf::from("/nonexistent/path"),
        }];
        let manifest = empty_manifest();

        let result = scan(&sources, &manifest, None, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_scan_move_between_sources() {
        // File moved from "marco" source to "common" source
        let dir_marco = TempDir::new().unwrap();
        let dir_common = TempDir::new().unwrap();
        let content = b"shared family photo content";
        create_file(dir_common.path(), "photo.jpg", content);

        let fp = compute_fingerprint(&dir_common.path().join("photo.jpg")).unwrap();

        let sources = vec![
            SourceConfig {
                name: "marco".to_string(),
                path: dir_marco.path().to_path_buf(),
            },
            SourceConfig {
                name: "common".to_string(),
                path: dir_common.path().to_path_buf(),
            },
        ];

        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "marco/photo.jpg".to_string(),
                size: content.len() as u64,
                mtime: Utc::now(),
                fingerprint: fp,
                archive_id: "a1".to_string(),
                history: vec![],
            }],
        };

        let result = scan(&sources, &manifest, None, &[]).unwrap();
        assert_eq!(result.stats.moved, 1);
        assert_eq!(result.stats.deleted, 0);
        assert!(matches!(
            &result.changes[0],
            FileChange::Moved { logical_path, old_path, .. }
            if logical_path == "common/photo.jpg" && old_path == "marco/photo.jpg"
        ));
    }

    #[test]
    fn test_scan_excludes_directory() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "photo.jpg", b"good photo");
        create_file(dir.path(), "@eaDir/thumb.jpg", b"thumbnail");
        create_file(dir.path(), "sub/@eaDir/meta.json", b"metadata");

        let sources = vec![make_source(&dir, "marco")];
        let manifest = empty_manifest();
        let exclude = vec!["@eaDir".to_string()];

        let result = scan(&sources, &manifest, None, &exclude).unwrap();
        assert_eq!(result.stats.new, 1);
        assert_eq!(result.stats.skipped_by_exclude, 2);

        let paths: Vec<&str> = result
            .changes
            .iter()
            .filter_map(|c| match c {
                FileChange::New { logical_path, .. } => Some(logical_path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(paths, vec!["marco/photo.jpg"]);
    }

    #[test]
    fn test_scan_excludes_glob_pattern() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "photo.jpg", b"photo");
        create_file(dir.path(), "temp.tmp", b"temp");
        create_file(dir.path(), "other.tmp", b"other temp");

        let sources = vec![make_source(&dir, "marco")];
        let manifest = empty_manifest();
        let exclude = vec!["*.tmp".to_string()];

        let result = scan(&sources, &manifest, None, &exclude).unwrap();
        assert_eq!(result.stats.new, 1);
        assert_eq!(result.stats.skipped_by_exclude, 2);
    }

    #[test]
    fn test_scan_excludes_multiple_patterns() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "photo.jpg", b"photo");
        create_file(dir.path(), "@eaDir/thumb.jpg", b"thumb");
        create_file(dir.path(), "#recycle/old.jpg", b"recycled");
        create_file(dir.path(), ".DS_Store", b"ds");

        let sources = vec![make_source(&dir, "marco")];
        let manifest = empty_manifest();
        let exclude = vec![
            "@eaDir".to_string(),
            "#recycle".to_string(),
            ".DS_Store".to_string(),
        ];

        let result = scan(&sources, &manifest, None, &exclude).unwrap();
        assert_eq!(result.stats.new, 1);
        assert_eq!(result.stats.skipped_by_exclude, 3);
    }

    #[test]
    fn test_should_exclude_exact_match() {
        let root = Path::new("/mnt/nas");
        let patterns = vec!["@eaDir".to_string()];
        assert!(should_exclude(Path::new("/mnt/nas/@eaDir/file.jpg"), root, &patterns));
        assert!(should_exclude(Path::new("/mnt/nas/sub/@eaDir/file.jpg"), root, &patterns));
        assert!(!should_exclude(Path::new("/mnt/nas/photo.jpg"), root, &patterns));
    }

    #[test]
    fn test_should_exclude_glob() {
        let root = Path::new("/mnt/nas");
        let patterns = vec!["*.tmp".to_string()];
        assert!(should_exclude(Path::new("/mnt/nas/file.tmp"), root, &patterns));
        assert!(!should_exclude(Path::new("/mnt/nas/file.jpg"), root, &patterns));
    }
}
