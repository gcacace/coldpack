use anyhow::{Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart as S3CompletedPart};
use aws_sdk_s3::Client;
use aws_smithy_types::byte_stream::Length;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::Xxh3;

use crate::config::Config;
use crate::util::parse_storage_class;

const PART_SIZE: u64 = 100 * 1024 * 1024; // 100 MB
const HASH_BUF_SIZE: usize = 1024 * 1024; // 1 MB read chunks for hashing

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UploadCheckpoint {
    pub upload_id: String,
    pub s3_key: String,
    pub total_parts: u32,
    pub completed_parts: Vec<CompletedPart>,
    pub local_archive_path: PathBuf,
    #[serde(default)]
    pub archive_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompletedPart {
    pub part_number: u32,
    pub etag: String,
}

impl UploadCheckpoint {
    pub fn new(
        upload_id: String,
        s3_key: String,
        local_archive_path: PathBuf,
        file_size: u64,
        archive_hash: String,
    ) -> Self {
        let total_parts = file_size.div_ceil(PART_SIZE) as u32;
        Self {
            upload_id,
            s3_key,
            total_parts,
            completed_parts: Vec::new(),
            local_archive_path,
            archive_hash,
        }
    }

    pub fn next_part_number(&self) -> Option<u32> {
        let completed: std::collections::HashSet<u32> =
            self.completed_parts.iter().map(|p| p.part_number).collect();
        (1..=self.total_parts).find(|n| !completed.contains(n))
    }

    #[cfg(test)]
    pub fn is_complete(&self) -> bool {
        self.completed_parts.len() as u32 == self.total_parts
    }

    pub fn record_part(&mut self, part_number: u32, etag: String) {
        if !self
            .completed_parts
            .iter()
            .any(|p| p.part_number == part_number)
        {
            self.completed_parts
                .push(CompletedPart { part_number, etag });
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

pub fn find_existing_checkpoint(
    profile_dir: &Path,
    s3_key: &str,
) -> Result<Option<(PathBuf, UploadCheckpoint)>> {
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

pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open file for hashing: {}", path.display()))?;
    let mut hasher = Xxh3::new();
    let mut buf = vec![0u8; HASH_BUF_SIZE];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("Failed to read file for hashing: {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:016x}", hasher.digest()))
}

#[cfg(test)]
pub fn compute_total_parts(file_size: u64) -> u32 {
    file_size.div_ceil(PART_SIZE) as u32
}

pub fn run_cleanup(profile_dir: &Path) -> anyhow::Result<()> {
    let cp_dir = checkpoint_dir(profile_dir);
    if cp_dir.exists() {
        let mut cleaned = 0;
        for entry in std::fs::read_dir(&cp_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                println!("  Removing checkpoint: {}", path.display());
                std::fs::remove_file(&path)?;
                cleaned += 1;
            }
        }
        if cleaned > 0 {
            println!("Cleaned up {} checkpoint file(s).", cleaned);
            println!("Note: In production, this would also abort stale multipart uploads on S3.");
        } else {
            println!("No stale checkpoints found.");
        }
    } else {
        println!("No stale checkpoints found.");
    }
    Ok(())
}

pub async fn upload_archive(
    client: &Client,
    config: &Config,
    profile_dir: &Path,
    s3_key: &str,
    archive_path: &Path,
) -> Result<()> {
    let file_size = std::fs::metadata(archive_path)
        .with_context(|| format!("Archive file not found: {}", archive_path.display()))?
        .len();

    let archive_hash = hash_file(archive_path)?;

    let checkpoint_info = find_existing_checkpoint(profile_dir, s3_key)?;

    let (cp_path, mut checkpoint) = if let Some((path, cp)) = checkpoint_info {
        if cp.archive_hash.is_empty() || cp.archive_hash != archive_hash {
            let reason = if cp.archive_hash.is_empty() {
                "legacy checkpoint without hash"
            } else {
                "archive content changed since last attempt"
            };
            eprintln!("  Aborting stale upload ({})...", reason);
            let _ = client
                .abort_multipart_upload()
                .bucket(&config.storage.bucket)
                .key(s3_key)
                .upload_id(&cp.upload_id)
                .send()
                .await;
            delete_checkpoint(&path)?;
            start_new_upload(
                client,
                config,
                profile_dir,
                s3_key,
                archive_path,
                file_size,
                &archive_hash,
            )
            .await?
        } else {
            eprintln!(
                "  Resuming upload ({} of {} parts already done)",
                cp.completed_parts.len(),
                cp.total_parts
            );
            (path, cp)
        }
    } else {
        start_new_upload(
            client,
            config,
            profile_dir,
            s3_key,
            archive_path,
            file_size,
            &archive_hash,
        )
        .await?
    };

    let upload_bar = ProgressBar::new(file_size);
    upload_bar.set_style(
        ProgressStyle::with_template(
            "  Uploading [{bar:40.cyan/dim}] {bytes}/{total_bytes}  ETA {eta}",
        )
        .unwrap()
        .progress_chars("##-"),
    );

    let already_uploaded: u64 = checkpoint
        .completed_parts
        .iter()
        .map(|p| {
            let (start, end) = checkpoint.part_byte_range(p.part_number, file_size);
            end - start
        })
        .sum();
    upload_bar.set_position(already_uploaded);

    while let Some(part_number) = checkpoint.next_part_number() {
        let (start, end) = checkpoint.part_byte_range(part_number, file_size);
        let length = end - start;

        let body = ByteStream::read_from()
            .path(archive_path)
            .offset(start)
            .length(Length::Exact(length))
            .build()
            .await
            .with_context(|| format!("Failed to read part {} from archive", part_number))?;

        let resp = client
            .upload_part()
            .bucket(&config.storage.bucket)
            .key(s3_key)
            .upload_id(&checkpoint.upload_id)
            .part_number(part_number as i32)
            .content_length(length as i64)
            .body(body)
            .send()
            .await
            .with_context(|| format!("Failed to upload part {}", part_number))?;

        let etag = resp
            .e_tag()
            .ok_or_else(|| anyhow::anyhow!("No ETag returned for part {}", part_number))?
            .to_string();

        checkpoint.record_part(part_number, etag);
        save_checkpoint(&cp_path, &checkpoint)?;
        upload_bar.set_position(already_uploaded + end);
    }

    upload_bar.finish_and_clear();

    let completed_parts: Vec<S3CompletedPart> = {
        let mut parts = checkpoint.completed_parts.clone();
        parts.sort_by_key(|p| p.part_number);
        parts
            .iter()
            .map(|p| {
                S3CompletedPart::builder()
                    .part_number(p.part_number as i32)
                    .e_tag(&p.etag)
                    .build()
            })
            .collect()
    };

    client
        .complete_multipart_upload()
        .bucket(&config.storage.bucket)
        .key(s3_key)
        .upload_id(&checkpoint.upload_id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(completed_parts))
                .build(),
        )
        .send()
        .await
        .with_context(|| "Failed to complete multipart upload")?;

    delete_checkpoint(&cp_path)?;

    Ok(())
}

async fn start_new_upload(
    client: &Client,
    config: &Config,
    profile_dir: &Path,
    s3_key: &str,
    archive_path: &Path,
    file_size: u64,
    archive_hash: &str,
) -> Result<(PathBuf, UploadCheckpoint)> {
    let resp = client
        .create_multipart_upload()
        .bucket(&config.storage.bucket)
        .key(s3_key)
        .storage_class(parse_storage_class(&config.storage.storage_class))
        .send()
        .await
        .with_context(|| "Failed to initiate multipart upload")?;

    let upload_id = resp
        .upload_id()
        .ok_or_else(|| anyhow::anyhow!("No upload ID returned"))?
        .to_string();

    let cp = UploadCheckpoint::new(
        upload_id,
        s3_key.to_string(),
        archive_path.to_path_buf(),
        file_size,
        archive_hash.to_string(),
    );
    let path = checkpoint_dir(profile_dir).join(format!("{}.json", &cp.upload_id));
    save_checkpoint(&path, &cp)?;
    Ok((path, cp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_checkpoint_new() {
        let cp = UploadCheckpoint::new(
            "upload-123".to_string(),
            "archives/backup.tar".to_string(),
            PathBuf::from("/tmp/backup.tar"),
            250 * 1024 * 1024, // 250 MB = 3 parts
            "testhash".to_string(),
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
            "archives/small.tar".to_string(),
            PathBuf::from("/tmp/small.tar"),
            50 * 1024 * 1024, // 50 MB = 1 part
            "testhash".to_string(),
        );
        assert_eq!(cp.total_parts, 1);
    }

    #[test]
    fn test_checkpoint_exact_boundary() {
        let cp = UploadCheckpoint::new(
            "upload-789".to_string(),
            "archives/exact.tar".to_string(),
            PathBuf::from("/tmp/exact.tar"),
            200 * 1024 * 1024, // 200 MB = exactly 2 parts
            "testhash".to_string(),
        );
        assert_eq!(cp.total_parts, 2);
    }

    #[test]
    fn test_record_part_and_progress() {
        let mut cp = UploadCheckpoint::new(
            "upload-1".to_string(),
            "key".to_string(),
            PathBuf::from("/tmp/f.tar"),
            300 * 1024 * 1024,
            "testhash".to_string(),
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
            PathBuf::from("/tmp/f.tar"),
            150 * 1024 * 1024,
            "testhash".to_string(),
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
            PathBuf::from("/tmp/f.tar"),
            file_size,
            "testhash".to_string(),
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
            "archives/test.tar".to_string(),
            PathBuf::from("/tmp/test.tar"),
            200 * 1024 * 1024,
            "testhash".to_string(),
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
            PathBuf::from("/tmp/f.tar"),
            100 * 1024 * 1024,
            "testhash".to_string(),
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
            "archives/backup-2026.tar".to_string(),
            PathBuf::from("/tmp/backup.tar"),
            100 * 1024 * 1024,
            "testhash".to_string(),
        );
        let cp_path = cp_dir.join("upload-xyz.json");
        save_checkpoint(&cp_path, &cp).unwrap();

        let found = find_existing_checkpoint(profile_dir, "archives/backup-2026.tar").unwrap();
        assert!(found.is_some());
        let (found_path, found_cp) = found.unwrap();
        assert_eq!(found_cp.upload_id, "upload-xyz");
        assert_eq!(found_path, cp_path);
    }

    #[test]
    fn test_find_no_matching_checkpoint() {
        let dir = TempDir::new().unwrap();
        let found = find_existing_checkpoint(dir.path(), "archives/no-match.tar").unwrap();
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
