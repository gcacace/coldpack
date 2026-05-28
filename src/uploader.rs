#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const PART_SIZE: u64 = 100 * 1024 * 1024; // 100 MB

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UploadCheckpoint {
    pub upload_id: String,
    pub s3_key: String,
    pub total_parts: u32,
    pub completed_parts: Vec<CompletedPart>,
    pub local_zip_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompletedPart {
    pub part_number: u32,
    pub etag: String,
}

impl UploadCheckpoint {
    pub fn new(upload_id: String, s3_key: String, local_zip_path: PathBuf, file_size: u64) -> Self {
        let total_parts = file_size.div_ceil(PART_SIZE) as u32;
        Self {
            upload_id,
            s3_key,
            total_parts,
            completed_parts: Vec::new(),
            local_zip_path,
        }
    }

    pub fn next_part_number(&self) -> Option<u32> {
        let completed: std::collections::HashSet<u32> =
            self.completed_parts.iter().map(|p| p.part_number).collect();
        (1..=self.total_parts).find(|n| !completed.contains(n))
    }

    pub fn is_complete(&self) -> bool {
        self.completed_parts.len() as u32 == self.total_parts
    }

    pub fn record_part(&mut self, part_number: u32, etag: String) {
        if !self.completed_parts.iter().any(|p| p.part_number == part_number) {
            self.completed_parts.push(CompletedPart { part_number, etag });
        }
    }

    pub fn part_byte_range(&self, part_number: u32, file_size: u64) -> (u64, u64) {
        let start = (part_number as u64 - 1) * PART_SIZE;
        let end = (start + PART_SIZE).min(file_size);
        (start, end)
    }
}

pub fn checkpoint_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("uploads")
}

pub fn checkpoint_path(profile_dir: &Path, upload_id: &str) -> PathBuf {
    checkpoint_dir(profile_dir).join(format!("{}.json", upload_id))
}

pub fn save_checkpoint(path: &Path, checkpoint: &UploadCheckpoint) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create checkpoint dir: {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(checkpoint)
        .with_context(|| "Failed to serialize checkpoint")?;
    std::fs::write(path, content)
        .with_context(|| format!("Failed to write checkpoint: {}", path.display()))?;
    Ok(())
}

pub fn load_checkpoint(path: &Path) -> Result<UploadCheckpoint> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read checkpoint: {}", path.display()))?;
    let checkpoint: UploadCheckpoint =
        serde_json::from_str(&content).with_context(|| "Failed to parse checkpoint")?;
    Ok(checkpoint)
}

pub fn find_existing_checkpoint(profile_dir: &Path, s3_key: &str) -> Result<Option<(PathBuf, UploadCheckpoint)>> {
    let dir = checkpoint_dir(profile_dir);
    if !dir.exists() {
        return Ok(None);
    }

    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("Failed to read checkpoint dir: {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            if let Ok(cp) = load_checkpoint(&path) {
                if cp.s3_key == s3_key {
                    return Ok(Some((path, cp)));
                }
            }
        }
    }

    Ok(None)
}

pub fn delete_checkpoint(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("Failed to delete checkpoint: {}", path.display()))?;
    }
    Ok(())
}

pub fn compute_total_parts(file_size: u64) -> u32 {
    file_size.div_ceil(PART_SIZE) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_checkpoint_new() {
        let cp = UploadCheckpoint::new(
            "upload-123".to_string(),
            "archives/backup.zip".to_string(),
            PathBuf::from("/tmp/backup.zip"),
            250 * 1024 * 1024, // 250 MB = 3 parts
        );
        assert_eq!(cp.total_parts, 3);
        assert!(cp.completed_parts.is_empty());
        assert_eq!(cp.next_part_number(), Some(1));
        assert!(!cp.is_complete());
    }

    #[test]
    fn test_checkpoint_small_file() {
        let cp = UploadCheckpoint::new(
            "upload-456".to_string(),
            "archives/small.zip".to_string(),
            PathBuf::from("/tmp/small.zip"),
            50 * 1024 * 1024, // 50 MB = 1 part
        );
        assert_eq!(cp.total_parts, 1);
    }

    #[test]
    fn test_checkpoint_exact_boundary() {
        let cp = UploadCheckpoint::new(
            "upload-789".to_string(),
            "archives/exact.zip".to_string(),
            PathBuf::from("/tmp/exact.zip"),
            200 * 1024 * 1024, // 200 MB = exactly 2 parts
        );
        assert_eq!(cp.total_parts, 2);
    }

    #[test]
    fn test_record_part_and_progress() {
        let mut cp = UploadCheckpoint::new(
            "upload-1".to_string(),
            "key".to_string(),
            PathBuf::from("/tmp/f.zip"),
            300 * 1024 * 1024,
        );
        assert_eq!(cp.total_parts, 3);

        cp.record_part(1, "etag-1".to_string());
        assert_eq!(cp.next_part_number(), Some(2));
        assert!(!cp.is_complete());

        cp.record_part(2, "etag-2".to_string());
        assert_eq!(cp.next_part_number(), Some(3));

        cp.record_part(3, "etag-3".to_string());
        assert_eq!(cp.next_part_number(), None);
        assert!(cp.is_complete());
    }

    #[test]
    fn test_record_part_idempotent() {
        let mut cp = UploadCheckpoint::new(
            "upload-1".to_string(),
            "key".to_string(),
            PathBuf::from("/tmp/f.zip"),
            150 * 1024 * 1024,
        );

        cp.record_part(1, "etag-1".to_string());
        cp.record_part(1, "etag-1-duplicate".to_string());
        assert_eq!(cp.completed_parts.len(), 1);
        assert_eq!(cp.completed_parts[0].etag, "etag-1");
    }

    #[test]
    fn test_part_byte_range() {
        let file_size = 250 * 1024 * 1024u64;
        let cp = UploadCheckpoint::new(
            "upload-1".to_string(),
            "key".to_string(),
            PathBuf::from("/tmp/f.zip"),
            file_size,
        );

        let (start, end) = cp.part_byte_range(1, file_size);
        assert_eq!(start, 0);
        assert_eq!(end, 100 * 1024 * 1024);

        let (start, end) = cp.part_byte_range(2, file_size);
        assert_eq!(start, 100 * 1024 * 1024);
        assert_eq!(end, 200 * 1024 * 1024);

        let (start, end) = cp.part_byte_range(3, file_size);
        assert_eq!(start, 200 * 1024 * 1024);
        assert_eq!(end, 250 * 1024 * 1024); // Last part is smaller
    }

    #[test]
    fn test_save_and_load_checkpoint() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("checkpoint.json");

        let mut cp = UploadCheckpoint::new(
            "upload-abc".to_string(),
            "archives/test.zip".to_string(),
            PathBuf::from("/tmp/test.zip"),
            200 * 1024 * 1024,
        );
        cp.record_part(1, "\"etag-with-quotes\"".to_string());

        save_checkpoint(&path, &cp).unwrap();
        let loaded = load_checkpoint(&path).unwrap();
        assert_eq!(cp, loaded);
    }

    #[test]
    fn test_save_checkpoint_creates_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("dir").join("cp.json");

        let cp = UploadCheckpoint::new(
            "id".to_string(),
            "key".to_string(),
            PathBuf::from("/tmp/f.zip"),
            100 * 1024 * 1024,
        );

        save_checkpoint(&path, &cp).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_load_nonexistent_checkpoint() {
        let result = load_checkpoint(Path::new("/nonexistent/cp.json"));
        assert!(result.is_err());
    }

    #[test]
    fn test_delete_checkpoint() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cp.json");
        std::fs::write(&path, "{}").unwrap();

        delete_checkpoint(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn test_delete_nonexistent_checkpoint() {
        let result = delete_checkpoint(Path::new("/nonexistent/cp.json"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_find_existing_checkpoint() {
        let dir = TempDir::new().unwrap();
        let profile_dir = dir.path();

        // Create the checkpoint dir
        let cp_dir = checkpoint_dir(profile_dir);
        std::fs::create_dir_all(&cp_dir).unwrap();

        let cp = UploadCheckpoint::new(
            "upload-xyz".to_string(),
            "archives/backup-2026.zip".to_string(),
            PathBuf::from("/tmp/backup.zip"),
            100 * 1024 * 1024,
        );
        let cp_path = cp_dir.join("upload-xyz.json");
        save_checkpoint(&cp_path, &cp).unwrap();

        let found = find_existing_checkpoint(profile_dir, "archives/backup-2026.zip").unwrap();
        assert!(found.is_some());
        let (found_path, found_cp) = found.unwrap();
        assert_eq!(found_cp.upload_id, "upload-xyz");
        assert_eq!(found_path, cp_path);
    }

    #[test]
    fn test_find_no_matching_checkpoint() {
        let dir = TempDir::new().unwrap();
        let found = find_existing_checkpoint(dir.path(), "archives/no-match.zip").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_compute_total_parts() {
        assert_eq!(compute_total_parts(0), 0);
        assert_eq!(compute_total_parts(1), 1);
        assert_eq!(compute_total_parts(100 * 1024 * 1024), 1);
        assert_eq!(compute_total_parts(100 * 1024 * 1024 + 1), 2);
        assert_eq!(compute_total_parts(200 * 1024 * 1024), 2);
        assert_eq!(compute_total_parts(1500 * 1024 * 1024), 15);
    }
}
