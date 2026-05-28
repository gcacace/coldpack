#![allow(dead_code)]

use anyhow::{Context, Result};
use chrono::Datelike;
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use zip::write::FileOptions;
use zip::ZipWriter;

use crate::scanner::FileChange;

pub struct ArchiveResult {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub file_count: u32,
}

#[derive(Debug, Clone)]
pub struct ArchivePlan {
    pub groups: Vec<ArchiveGroup>,
}

#[derive(Debug, Clone)]
pub struct ArchiveGroup {
    pub label: String,
    pub files: Vec<(String, PathBuf, u64)>,
    pub total_size: u64,
}

impl ArchivePlan {
    pub fn total_files(&self) -> usize {
        self.groups.iter().map(|g| g.files.len()).sum()
    }

    pub fn total_size(&self) -> u64 {
        self.groups.iter().map(|g| g.total_size).sum()
    }
}

pub fn plan_archives(changes: &[FileChange], max_zip_bytes: u64) -> ArchivePlan {
    // Collect archivable files with their mtime
    let mut by_month: BTreeMap<String, Vec<(String, PathBuf, u64)>> = BTreeMap::new();

    for change in changes {
        let (logical_path, disk_path, size, mtime) = match change {
            FileChange::New {
                logical_path,
                disk_path,
                size,
                mtime,
                ..
            } => (logical_path.clone(), disk_path.clone(), *size, *mtime),
            FileChange::Modified {
                logical_path,
                disk_path,
                size,
                mtime,
                ..
            } => (logical_path.clone(), disk_path.clone(), *size, *mtime),
            _ => continue,
        };

        let month_key = format!("{:04}-{:02}", mtime.year(), mtime.month());
        by_month
            .entry(month_key)
            .or_default()
            .push((logical_path, disk_path, size));
    }

    // Split each month group by max size
    let mut groups = Vec::new();

    for (month, files) in by_month {
        let mut current_files: Vec<(String, PathBuf, u64)> = Vec::new();
        let mut current_size: u64 = 0;
        let mut part = 1u32;

        for file in files {
            let file_size = file.2;

            // If adding this file would exceed the cap and we already have files in the group
            if current_size + file_size > max_zip_bytes && !current_files.is_empty() {
                let label = if part == 1 {
                    month.clone()
                } else {
                    format!("{}-part{}", month, part)
                };
                groups.push(ArchiveGroup {
                    label,
                    total_size: current_size,
                    files: std::mem::take(&mut current_files),
                });
                part += 1;
                current_size = 0;
            }

            current_size += file_size;
            current_files.push(file);
        }

        // Flush remaining files
        if !current_files.is_empty() {
            let label = if part == 1 {
                month.clone()
            } else {
                format!("{}-part{}", month, part)
            };
            groups.push(ArchiveGroup {
                label,
                total_size: current_size,
                files: current_files,
            });
        }
    }

    ArchivePlan { groups }
}

pub fn create_archive_from_group(
    output_path: &Path,
    group: &ArchiveGroup,
    compression: zip::CompressionMethod,
    mut on_progress: impl FnMut(u32, u32),
) -> Result<ArchiveResult> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create archive directory: {}", parent.display()))?;
    }

    let file = File::create(output_path)
        .with_context(|| format!("Failed to create archive: {}", output_path.display()))?;
    let mut zip = ZipWriter::new(file);

    let total = group.files.len() as u32;
    let options = FileOptions::<()>::default()
        .compression_method(compression);

    for (i, (logical_path, disk_path, _size)) in group.files.iter().enumerate() {
        on_progress(i as u32 + 1, total);

        zip.start_file(logical_path.to_string(), options)
            .with_context(|| format!("Failed to add to archive: {}", logical_path))?;

        let mut source = File::open(disk_path)
            .with_context(|| format!("Failed to open source file: {}", disk_path.display()))?;

        io::copy(&mut source, &mut zip)
            .with_context(|| format!("Failed to write file to archive: {}", logical_path))?;
    }

    zip.finish().with_context(|| "Failed to finalize archive")?;

    let archive_size = std::fs::metadata(output_path)
        .with_context(|| "Failed to read archive size")?
        .len();

    Ok(ArchiveResult {
        path: output_path.to_path_buf(),
        size_bytes: archive_size,
        file_count: total,
    })
}

pub fn create_archive(
    output_path: &Path,
    changes: &[FileChange],
    compression: zip::CompressionMethod,
    mut on_progress: impl FnMut(u32, u32),
) -> Result<Option<ArchiveResult>> {
    let files_to_archive: Vec<(&str, &Path)> = changes
        .iter()
        .filter_map(|c| match c {
            FileChange::New {
                logical_path,
                disk_path,
                ..
            } => Some((logical_path.as_str(), disk_path.as_path())),
            FileChange::Modified {
                logical_path,
                disk_path,
                ..
            } => Some((logical_path.as_str(), disk_path.as_path())),
            _ => None,
        })
        .collect();

    if files_to_archive.is_empty() {
        return Ok(None);
    }

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create archive directory: {}", parent.display()))?;
    }

    let file = File::create(output_path)
        .with_context(|| format!("Failed to create archive: {}", output_path.display()))?;
    let mut zip = ZipWriter::new(file);

    let total = files_to_archive.len() as u32;
    let options = FileOptions::<()>::default()
        .compression_method(compression);

    for (i, (logical_path, disk_path)) in files_to_archive.iter().enumerate() {
        on_progress(i as u32 + 1, total);

        zip.start_file(logical_path.to_string(), options)
            .with_context(|| format!("Failed to add to archive: {}", logical_path))?;

        let mut source = File::open(disk_path)
            .with_context(|| format!("Failed to open source file: {}", disk_path.display()))?;

        io::copy(&mut source, &mut zip)
            .with_context(|| format!("Failed to write file to archive: {}", logical_path))?;
    }

    zip.finish().with_context(|| "Failed to finalize archive")?;

    let archive_size = std::fs::metadata(output_path)
        .with_context(|| "Failed to read archive size")?
        .len();

    Ok(Some(ArchiveResult {
        path: output_path.to_path_buf(),
        size_bytes: archive_size,
        file_count: total,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::fs;
    use std::io::Read;
    use tempfile::TempDir;
    use zip::ZipArchive;

    fn make_new_change(dir: &Path, name: &str, content: &[u8]) -> FileChange {
        let disk_path = dir.join(name);
        if let Some(parent) = disk_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&disk_path, content).unwrap();

        FileChange::New {
            logical_path: format!("marco/{}", name),
            disk_path,
            size: content.len() as u64,
            mtime: Utc::now(),
            fingerprint: "fp".to_string(),
        }
    }

    #[test]
    fn test_create_archive_empty_changes() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("test.zip");

        let result = create_archive(&output, &[], zip::CompressionMethod::Stored, |_, _| {}).unwrap();
        assert!(result.is_none());
        assert!(!output.exists());
    }

    #[test]
    fn test_create_archive_only_moves_and_deletes() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("test.zip");

        let changes = vec![
            FileChange::Moved {
                logical_path: "new/path.jpg".to_string(),
                old_path: "old/path.jpg".to_string(),
                fingerprint: "fp".to_string(),
            },
            FileChange::Deleted {
                logical_path: "gone.jpg".to_string(),
            },
        ];

        let result = create_archive(&output, &changes, zip::CompressionMethod::Stored, |_, _| {}).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_create_archive_single_file() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("test.zip");
        let content = b"hello world photo data";

        let changes = vec![make_new_change(dir.path(), "photo.jpg", content)];

        let result = create_archive(&output, &changes, zip::CompressionMethod::Stored, |_, _| {}).unwrap().unwrap();
        assert_eq!(result.file_count, 1);
        assert!(result.size_bytes > 0);
        assert!(output.exists());

        // Verify zip contents
        let file = File::open(&output).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        assert_eq!(archive.len(), 1);

        let mut entry = archive.by_name("marco/photo.jpg").unwrap();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, content);
    }

    #[test]
    fn test_create_archive_preserves_directory_structure() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("test.zip");

        let changes = vec![
            make_new_change(dir.path(), "2026/05/photo1.jpg", b"photo 1"),
            make_new_change(dir.path(), "2026/05/photo2.jpg", b"photo 2"),
            make_new_change(dir.path(), "2026/04/photo3.jpg", b"photo 3"),
        ];

        let result = create_archive(&output, &changes, zip::CompressionMethod::Stored, |_, _| {}).unwrap().unwrap();
        assert_eq!(result.file_count, 3);

        let file = File::open(&output).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        assert_eq!(archive.len(), 3);

        // Verify all paths are preserved
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"marco/2026/05/photo1.jpg".to_string()));
        assert!(names.contains(&"marco/2026/05/photo2.jpg".to_string()));
        assert!(names.contains(&"marco/2026/04/photo3.jpg".to_string()));
    }

    #[test]
    fn test_create_archive_mixed_changes() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("test.zip");

        let disk_path = dir.path().join("modified.jpg");
        fs::write(&disk_path, b"modified content").unwrap();

        let changes = vec![
            make_new_change(dir.path(), "new.jpg", b"new file"),
            FileChange::Modified {
                logical_path: "marco/modified.jpg".to_string(),
                disk_path,
                size: 16,
                mtime: Utc::now(),
                fingerprint: "fp2".to_string(),
                previous_archive_id: "old-archive".to_string(),
            },
            FileChange::Moved {
                logical_path: "moved.jpg".to_string(),
                old_path: "old.jpg".to_string(),
                fingerprint: "fp3".to_string(),
            },
            FileChange::Deleted {
                logical_path: "deleted.jpg".to_string(),
            },
        ];

        let result = create_archive(&output, &changes, zip::CompressionMethod::Stored, |_, _| {}).unwrap().unwrap();
        // Only New + Modified go into the zip (2 files)
        assert_eq!(result.file_count, 2);

        let file = File::open(&output).unwrap();
        let archive = ZipArchive::new(file).unwrap();
        assert_eq!(archive.len(), 2);
    }

    #[test]
    fn test_create_archive_progress_callback() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("test.zip");

        let changes = vec![
            make_new_change(dir.path(), "a.jpg", b"aaa"),
            make_new_change(dir.path(), "b.jpg", b"bbb"),
            make_new_change(dir.path(), "c.jpg", b"ccc"),
        ];

        let mut progress_calls = Vec::new();
        create_archive(&output, &changes, zip::CompressionMethod::Stored, |current, total| {
            progress_calls.push((current, total));
        })
        .unwrap();

        assert_eq!(progress_calls, vec![(1, 3), (2, 3), (3, 3)]);
    }

    #[test]
    fn test_create_archive_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("nested").join("dir").join("test.zip");

        let changes = vec![make_new_change(dir.path(), "photo.jpg", b"data")];

        let result = create_archive(&output, &changes, zip::CompressionMethod::Stored, |_, _| {}).unwrap().unwrap();
        assert!(result.path.exists());
    }

    #[test]
    fn test_plan_archives_groups_by_month() {
        use chrono::TimeZone;
        let changes = vec![
            FileChange::New {
                logical_path: "a/jan.jpg".to_string(),
                disk_path: PathBuf::from("/tmp/jan.jpg"),
                size: 1000,
                mtime: Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap(),
                fingerprint: "fp1".to_string(),
            },
            FileChange::New {
                logical_path: "a/jan2.jpg".to_string(),
                disk_path: PathBuf::from("/tmp/jan2.jpg"),
                size: 2000,
                mtime: Utc.with_ymd_and_hms(2024, 1, 20, 0, 0, 0).unwrap(),
                fingerprint: "fp2".to_string(),
            },
            FileChange::New {
                logical_path: "a/feb.jpg".to_string(),
                disk_path: PathBuf::from("/tmp/feb.jpg"),
                size: 3000,
                mtime: Utc.with_ymd_and_hms(2024, 2, 10, 0, 0, 0).unwrap(),
                fingerprint: "fp3".to_string(),
            },
        ];

        let plan = plan_archives(&changes, 10 * 1024 * 1024 * 1024);
        assert_eq!(plan.groups.len(), 2);
        assert_eq!(plan.groups[0].label, "2024-01");
        assert_eq!(plan.groups[0].files.len(), 2);
        assert_eq!(plan.groups[1].label, "2024-02");
        assert_eq!(plan.groups[1].files.len(), 1);
    }

    #[test]
    fn test_plan_archives_splits_by_size() {
        use chrono::TimeZone;
        let changes = vec![
            FileChange::New {
                logical_path: "a.jpg".to_string(),
                disk_path: PathBuf::from("/tmp/a.jpg"),
                size: 600,
                mtime: Utc.with_ymd_and_hms(2024, 3, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp1".to_string(),
            },
            FileChange::New {
                logical_path: "b.jpg".to_string(),
                disk_path: PathBuf::from("/tmp/b.jpg"),
                size: 600,
                mtime: Utc.with_ymd_and_hms(2024, 3, 2, 0, 0, 0).unwrap(),
                fingerprint: "fp2".to_string(),
            },
            FileChange::New {
                logical_path: "c.jpg".to_string(),
                disk_path: PathBuf::from("/tmp/c.jpg"),
                size: 600,
                mtime: Utc.with_ymd_and_hms(2024, 3, 3, 0, 0, 0).unwrap(),
                fingerprint: "fp3".to_string(),
            },
        ];

        // Cap at 1100 bytes: first two fit (600+600=1200 > 1100 after adding second), so first alone, second+third? no 600+600=1200>1100
        // Actually with 1100 cap: file1 (600) alone won't trigger split. file2 would make 1200>1100, so split.
        // Then file2 (600) alone, file3 would make 1200>1100, split again. Result: 3 groups of 1 each.
        // Let's use a cap of 1500 to get 2 groups: [600, 600] and [600]
        let plan = plan_archives(&changes, 1500);
        assert_eq!(plan.groups.len(), 2);
        assert_eq!(plan.groups[0].label, "2024-03");
        assert_eq!(plan.groups[0].files.len(), 2); // 600+600=1200 < 1500
        assert_eq!(plan.groups[1].label, "2024-03-part2");
        assert_eq!(plan.groups[1].files.len(), 1); // 600+600+600=1800 > 1500, so third file goes to new group
    }

    #[test]
    fn test_plan_archives_single_large_file() {
        use chrono::TimeZone;
        let changes = vec![
            FileChange::New {
                logical_path: "huge_video.mp4".to_string(),
                disk_path: PathBuf::from("/tmp/huge.mp4"),
                size: 20_000_000_000, // 20 GB
                mtime: Utc.with_ymd_and_hms(2024, 5, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp1".to_string(),
            },
        ];

        // Cap at 10 GB: single file exceeds cap, gets its own zip
        let plan = plan_archives(&changes, 10 * 1024 * 1024 * 1024);
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].files.len(), 1);
        assert_eq!(plan.groups[0].total_size, 20_000_000_000);
    }

    #[test]
    fn test_plan_archives_skips_non_archivable() {
        use chrono::TimeZone;
        let changes = vec![
            FileChange::New {
                logical_path: "new.jpg".to_string(),
                disk_path: PathBuf::from("/tmp/new.jpg"),
                size: 1000,
                mtime: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp1".to_string(),
            },
            FileChange::Moved {
                logical_path: "moved.jpg".to_string(),
                old_path: "old.jpg".to_string(),
                fingerprint: "fp2".to_string(),
            },
            FileChange::Deleted {
                logical_path: "deleted.jpg".to_string(),
            },
        ];

        let plan = plan_archives(&changes, 10 * 1024 * 1024 * 1024);
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.total_files(), 1); // Only the New file
    }

    #[test]
    fn test_plan_archives_empty() {
        let changes: Vec<FileChange> = vec![];
        let plan = plan_archives(&changes, 10 * 1024 * 1024 * 1024);
        assert_eq!(plan.groups.len(), 0);
    }
}
