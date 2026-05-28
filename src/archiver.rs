#![allow(dead_code)]

use anyhow::{Context, Result};
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

pub fn create_archive(
    output_path: &Path,
    changes: &[FileChange],
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
        .compression_method(zip::CompressionMethod::Deflated);

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

        let result = create_archive(&output, &[], |_, _| {}).unwrap();
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

        let result = create_archive(&output, &changes, |_, _| {}).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_create_archive_single_file() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("test.zip");
        let content = b"hello world photo data";

        let changes = vec![make_new_change(dir.path(), "photo.jpg", content)];

        let result = create_archive(&output, &changes, |_, _| {}).unwrap().unwrap();
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

        let result = create_archive(&output, &changes, |_, _| {}).unwrap().unwrap();
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

        let result = create_archive(&output, &changes, |_, _| {}).unwrap().unwrap();
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
        create_archive(&output, &changes, |current, total| {
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

        let result = create_archive(&output, &changes, |_, _| {}).unwrap().unwrap();
        assert!(result.path.exists());
    }
}
