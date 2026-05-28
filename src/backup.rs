#![allow(dead_code)]

use anyhow::{Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, StorageClass};
use aws_sdk_s3::Client;
use aws_smithy_types::byte_stream::Length;
use chrono::{DateTime, Utc};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use crate::archiver;
use crate::config::Config;
use crate::manifest::{
    self, Archive, FileEntry, HistoryEvent, Manifest,
};
use crate::scanner::{self, FileChange, ScanResult};
use crate::uploader::{self, UploadCheckpoint};

pub struct BackupOptions {
    pub dry_run: bool,
    pub cutoff_override: Option<String>,
}

pub struct BackupReport {
    pub scan_stats: scanner::ScanStats,
    pub archive_size: Option<u64>,
    pub archive_file_count: u32,
    pub manifest_updated: bool,
}

pub async fn run_backup(config: &Config, profile_dir: &Path, options: &BackupOptions) -> Result<BackupReport> {
    // 1. Resolve cutoff
    let cutoff_str = options
        .cutoff_override
        .as_deref()
        .unwrap_or(&config.backup.filter.cutoff);
    let cutoff = manifest::resolve_cutoff(cutoff_str)?;

    // 2. Load manifest (local cache or S3)
    let manifest = load_or_create_manifest(config, profile_dir).await?;

    // 3. Scan with progress spinner
    let scan_spinner = ProgressBar::new_spinner();
    scan_spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    scan_spinner.set_message("Scanning sources...");
    scan_spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let scan_result = scanner::scan(
        &config.backup.sources,
        &manifest,
        cutoff,
        &config.backup.filter.exclude,
        |stats| {
            scan_spinner.set_message(format!(
                "Scanning... {} files ({} excluded, {} skipped by cutoff)",
                stats.total_files_scanned, stats.skipped_by_exclude, stats.skipped_by_cutoff
            ));
        },
    )?;

    scan_spinner.finish_with_message(format!(
        "Scanned {} files ({} new, {} modified, {} moved, {} excluded)",
        scan_result.stats.total_files_scanned,
        scan_result.stats.new,
        scan_result.stats.modified,
        scan_result.stats.moved,
        scan_result.stats.skipped_by_exclude,
    ));

    if options.dry_run {
        return Ok(BackupReport {
            scan_stats: scan_result.stats,
            archive_size: None,
            archive_file_count: 0,
            manifest_updated: false,
        });
    }

    // 4. Create archive (if there are files to upload)
    let now = Utc::now();
    let archive_id = format!("backup-{}", now.to_rfc3339());
    let s3_key = format!(
        "{}backup-{}.zip",
        config.storage.archive_prefix,
        now.format("%Y-%m-%dT%H%M%S")
    );

    let has_uploadable_files = scan_result.changes.iter().any(|c| {
        matches!(c, FileChange::New { .. } | FileChange::Modified { .. })
    });

    let mut archive_size = None;
    let mut archive_file_count = 0u32;

    if has_uploadable_files {
        let tmp_dir = std::env::temp_dir().join("coldpack");
        std::fs::create_dir_all(&tmp_dir)?;
        let zip_path = tmp_dir.join(format!("backup-{}.zip", now.format("%Y-%m-%dT%H%M%S")));

        // Count files to archive for progress bar
        let files_to_zip = scan_result.changes.iter().filter(|c| {
            matches!(c, FileChange::New { .. } | FileChange::Modified { .. })
        }).count() as u64;

        let archive_bar = ProgressBar::new(files_to_zip);
        archive_bar.set_style(
            ProgressStyle::with_template("  Archiving [{bar:40.cyan/dim}] {pos}/{len} files  ETA {eta}")
                .unwrap()
                .progress_chars("##-"),
        );

        // Create zip
        let archive_result = archiver::create_archive(&zip_path, &scan_result.changes, |current, _total| {
            archive_bar.set_position(current as u64);
        })?;
        archive_bar.finish_and_clear();

        if let Some(result) = archive_result {
            archive_file_count = result.file_count;
            archive_size = Some(result.size_bytes);

            // Upload to S3
            let s3_client = create_s3_client(config).await?;
            upload_archive(&s3_client, config, profile_dir, &s3_key, &zip_path).await?;

            // Clean up local zip
            let _ = std::fs::remove_file(&zip_path);
        }
    }

    // 5. Update manifest
    eprintln!("  Updating manifest...");
    let updated_manifest = apply_changes_to_manifest(
        manifest,
        &scan_result,
        &archive_id,
        &s3_key,
        archive_size.unwrap_or(0),
        archive_file_count,
        now,
    );

    // 6. Save manifest locally and to S3
    save_manifest(config, profile_dir, &updated_manifest).await?;
    eprintln!("  Manifest saved ({} files tracked).", updated_manifest.files.len());

    Ok(BackupReport {
        scan_stats: scan_result.stats,
        archive_size,
        archive_file_count,
        manifest_updated: true,
    })
}

fn parse_storage_class(s: &str) -> StorageClass {
    match s {
        "STANDARD" => StorageClass::Standard,
        "STANDARD_IA" => StorageClass::StandardIa,
        "GLACIER_IR" => StorageClass::GlacierIr,
        "GLACIER" => StorageClass::Glacier,
        _ => StorageClass::DeepArchive,
    }
}

async fn create_s3_client(config: &Config) -> Result<Client> {
    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(config.storage.region.clone()))
        .load()
        .await;
    Ok(Client::new(&aws_config))
}

async fn load_or_create_manifest(config: &Config, profile_dir: &Path) -> Result<Manifest> {
    let local_path = manifest::manifest_local_path(profile_dir);

    // Try loading local manifest
    if local_path.exists() {
        match manifest::load_from_file(&local_path) {
            Ok(m) => return Ok(m),
            Err(_) => {
                eprintln!("Local manifest corrupted, will download from S3...");
            }
        }
    }

    // Try downloading from S3
    match download_manifest_from_s3(config).await {
        Ok(m) => {
            // Cache locally
            let _ = manifest::save_to_file(&m, &local_path);
            Ok(m)
        }
        Err(_) => {
            eprintln!("No existing manifest found, starting fresh.");
            Ok(Manifest::new())
        }
    }
}

async fn download_manifest_from_s3(config: &Config) -> Result<Manifest> {
    let client = create_s3_client(config).await?;
    let key = format!("{}manifest.json", config.storage.manifest_prefix);

    let resp = client
        .get_object()
        .bucket(&config.storage.bucket)
        .key(&key)
        .send()
        .await
        .with_context(|| "Failed to download manifest from S3")?;

    let bytes = resp
        .body
        .collect()
        .await
        .with_context(|| "Failed to read manifest body")?
        .into_bytes();

    let manifest: Manifest =
        serde_json::from_slice(&bytes).with_context(|| "Failed to parse manifest from S3")?;
    Ok(manifest)
}

async fn upload_archive(
    client: &Client,
    config: &Config,
    profile_dir: &Path,
    s3_key: &str,
    zip_path: &Path,
) -> Result<()> {
    let file_size = std::fs::metadata(zip_path)?.len();

    // Check for existing checkpoint
    let checkpoint_info = uploader::find_existing_checkpoint(profile_dir, s3_key)?;

    let (cp_path, mut checkpoint) = if let Some((path, cp)) = checkpoint_info {
        eprintln!("  Resuming upload ({} of {} parts already done)", cp.completed_parts.len(), cp.total_parts);
        (path, cp)
    } else {
        // Initiate new multipart upload
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
            zip_path.to_path_buf(),
            file_size,
        );
        let path = uploader::checkpoint_dir(profile_dir).join(format!("{}.json", &cp.upload_id));
        uploader::save_checkpoint(&path, &cp)?;
        (path, cp)
    };

    // Upload parts with progress bar
    let upload_bar = ProgressBar::new(file_size);
    upload_bar.set_style(
        ProgressStyle::with_template(
            "  Uploading [{bar:40.cyan/dim}] {bytes}/{total_bytes}  ETA {eta}"
        )
        .unwrap()
        .progress_chars("##-"),
    );

    // Account for already-completed parts
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
            .path(zip_path)
            .offset(start)
            .length(Length::Exact(length))
            .build()
            .await
            .with_context(|| format!("Failed to read part {} from zip", part_number))?;

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
        uploader::save_checkpoint(&cp_path, &checkpoint)?;
        upload_bar.set_position(already_uploaded + end);
    }

    upload_bar.finish_and_clear();

    // Complete multipart upload
    let completed_parts: Vec<CompletedPart> = {
        let mut parts = checkpoint.completed_parts.clone();
        parts.sort_by_key(|p| p.part_number);
        parts
            .iter()
            .map(|p| {
                CompletedPart::builder()
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

    // Clean up checkpoint
    uploader::delete_checkpoint(&cp_path)?;

    Ok(())
}

async fn save_manifest(config: &Config, profile_dir: &Path, manifest: &Manifest) -> Result<()> {
    // Save locally
    let local_path = manifest::manifest_local_path(profile_dir);
    manifest::save_to_file(manifest, &local_path)?;

    // Upload to S3
    let client = create_s3_client(config).await?;
    let key = format!("{}manifest.json", config.storage.manifest_prefix);
    let content = serde_json::to_string_pretty(manifest)?;

    client
        .put_object()
        .bucket(&config.storage.bucket)
        .key(&key)
        .body(ByteStream::from(content.into_bytes()))
        .content_type("application/json")
        .send()
        .await
        .with_context(|| "Failed to upload manifest to S3")?;

    Ok(())
}

pub fn apply_changes_to_manifest(
    mut manifest: Manifest,
    scan_result: &ScanResult,
    archive_id: &str,
    s3_key: &str,
    archive_size: u64,
    file_count: u32,
    now: DateTime<Utc>,
) -> Manifest {
    // Add archive entry (only if files were uploaded)
    if file_count > 0 {
        manifest.archives.push(Archive {
            id: archive_id.to_string(),
            s3_key: s3_key.to_string(),
            size_bytes: archive_size,
            created_at: now,
            file_count,
        });
    }

    // Build a mutable index of existing files by path
    let mut file_map: std::collections::HashMap<String, usize> = manifest
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.path.clone(), i))
        .collect();

    for change in &scan_result.changes {
        match change {
            FileChange::New {
                logical_path,
                size,
                mtime,
                fingerprint,
                ..
            } => {
                manifest.files.push(FileEntry {
                    path: logical_path.clone(),
                    size: *size,
                    mtime: *mtime,
                    fingerprint: fingerprint.clone(),
                    archive_id: archive_id.to_string(),
                    history: vec![],
                });
                file_map.insert(logical_path.clone(), manifest.files.len() - 1);
            }
            FileChange::Modified {
                logical_path,
                size,
                mtime,
                fingerprint,
                previous_archive_id,
                ..
            } => {
                if let Some(&idx) = file_map.get(logical_path.as_str()) {
                    let entry = &mut manifest.files[idx];
                    // Record old version in history
                    entry.history.push(HistoryEvent::Added {
                        archive_id: entry.archive_id.clone(),
                        mtime: entry.mtime,
                        size: entry.size,
                    });
                    // Update to new version
                    entry.size = *size;
                    entry.mtime = *mtime;
                    entry.fingerprint = fingerprint.clone();
                    entry.archive_id = archive_id.to_string();
                } else {
                    // Shouldn't happen, but handle gracefully
                    manifest.files.push(FileEntry {
                        path: logical_path.clone(),
                        size: *size,
                        mtime: *mtime,
                        fingerprint: fingerprint.clone(),
                        archive_id: archive_id.to_string(),
                        history: vec![HistoryEvent::Added {
                            archive_id: previous_archive_id.clone(),
                            mtime: *mtime,
                            size: *size,
                        }],
                    });
                }
            }
            FileChange::Moved {
                logical_path,
                old_path,
                fingerprint,
            } => {
                if let Some(&idx) = file_map.get(old_path.as_str()) {
                    let entry = &mut manifest.files[idx];
                    entry.history.push(HistoryEvent::Moved {
                        from: old_path.clone(),
                        at: now,
                    });
                    entry.path = logical_path.clone();
                    entry.fingerprint = fingerprint.clone();
                    // Update the index
                    file_map.remove(old_path.as_str());
                    file_map.insert(logical_path.clone(), idx);
                }
            }
            FileChange::Deleted { logical_path } => {
                if let Some(&idx) = file_map.get(logical_path.as_str()) {
                    let entry = &mut manifest.files[idx];
                    entry.history.push(HistoryEvent::Deleted { at: now });
                }
            }
        }
    }

    manifest.last_backup = Some(now);
    manifest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::ScanStats;
    use chrono::TimeZone;

    fn make_scan_result(changes: Vec<FileChange>) -> ScanResult {
        ScanResult {
            stats: ScanStats::default(),
            changes,
        }
    }

    #[test]
    fn test_apply_new_files_to_manifest() {
        let manifest = Manifest::new();
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 10, 0, 0).unwrap();

        let scan = make_scan_result(vec![
            FileChange::New {
                logical_path: "marco/photo1.jpg".to_string(),
                disk_path: PathBuf::from("/tmp/photo1.jpg"),
                size: 5000,
                mtime: Utc.with_ymd_and_hms(2026, 5, 15, 0, 0, 0).unwrap(),
                fingerprint: "fp1".to_string(),
            },
            FileChange::New {
                logical_path: "laura/photo2.jpg".to_string(),
                disk_path: PathBuf::from("/tmp/photo2.jpg"),
                size: 3000,
                mtime: Utc.with_ymd_and_hms(2026, 5, 20, 0, 0, 0).unwrap(),
                fingerprint: "fp2".to_string(),
            },
        ]);

        let result = apply_changes_to_manifest(
            manifest,
            &scan,
            "backup-1",
            "archives/backup-1.zip",
            8000,
            2,
            now,
        );

        assert_eq!(result.archives.len(), 1);
        assert_eq!(result.archives[0].file_count, 2);
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].path, "marco/photo1.jpg");
        assert_eq!(result.files[0].archive_id, "backup-1");
        assert_eq!(result.files[1].path, "laura/photo2.jpg");
        assert_eq!(result.last_backup, Some(now));
    }

    #[test]
    fn test_apply_modified_file() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 10, 0, 0).unwrap();
        let manifest = Manifest {
            version: 1,
            last_backup: Some(Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap()),
            archives: vec![Archive {
                id: "backup-old".to_string(),
                s3_key: "archives/old.zip".to_string(),
                size_bytes: 5000,
                created_at: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                file_count: 1,
            }],
            files: vec![FileEntry {
                path: "marco/photo.jpg".to_string(),
                size: 5000,
                mtime: Utc.with_ymd_and_hms(2026, 4, 15, 0, 0, 0).unwrap(),
                fingerprint: "old-fp".to_string(),
                archive_id: "backup-old".to_string(),
                history: vec![],
            }],
        };

        let scan = make_scan_result(vec![FileChange::Modified {
            logical_path: "marco/photo.jpg".to_string(),
            disk_path: PathBuf::from("/tmp/photo.jpg"),
            size: 6000,
            mtime: Utc.with_ymd_and_hms(2026, 5, 20, 0, 0, 0).unwrap(),
            fingerprint: "new-fp".to_string(),
            previous_archive_id: "backup-old".to_string(),
        }]);

        let result = apply_changes_to_manifest(
            manifest,
            &scan,
            "backup-2",
            "archives/backup-2.zip",
            6000,
            1,
            now,
        );

        assert_eq!(result.files.len(), 1);
        let file = &result.files[0];
        assert_eq!(file.size, 6000);
        assert_eq!(file.fingerprint, "new-fp");
        assert_eq!(file.archive_id, "backup-2");
        assert_eq!(file.history.len(), 1);
        assert!(matches!(
            &file.history[0],
            HistoryEvent::Added { archive_id, size, .. }
            if archive_id == "backup-old" && *size == 5000
        ));
    }

    #[test]
    fn test_apply_moved_file() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 10, 0, 0).unwrap();
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "marco/photo.jpg".to_string(),
                size: 5000,
                mtime: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp1".to_string(),
                archive_id: "backup-1".to_string(),
                history: vec![],
            }],
        };

        let scan = make_scan_result(vec![FileChange::Moved {
            logical_path: "common/photo.jpg".to_string(),
            old_path: "marco/photo.jpg".to_string(),
            fingerprint: "fp1".to_string(),
        }]);

        let result = apply_changes_to_manifest(
            manifest, &scan, "backup-2", "archives/backup-2.zip", 0, 0, now,
        );

        // No new archive since no files uploaded
        assert_eq!(result.archives.len(), 0);
        assert_eq!(result.files.len(), 1);
        let file = &result.files[0];
        assert_eq!(file.path, "common/photo.jpg");
        assert_eq!(file.archive_id, "backup-1"); // Still points to old archive
        assert_eq!(file.history.len(), 1);
        assert!(matches!(
            &file.history[0],
            HistoryEvent::Moved { from, .. } if from == "marco/photo.jpg"
        ));
    }

    #[test]
    fn test_apply_deleted_file() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 10, 0, 0).unwrap();
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "marco/deleted.jpg".to_string(),
                size: 1000,
                mtime: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp-del".to_string(),
                archive_id: "backup-1".to_string(),
                history: vec![],
            }],
        };

        let scan = make_scan_result(vec![FileChange::Deleted {
            logical_path: "marco/deleted.jpg".to_string(),
        }]);

        let result = apply_changes_to_manifest(
            manifest, &scan, "backup-2", "archives/backup-2.zip", 0, 0, now,
        );

        assert_eq!(result.files.len(), 1);
        let file = &result.files[0];
        assert_eq!(file.history.len(), 1);
        assert!(matches!(&file.history[0], HistoryEvent::Deleted { at } if *at == now));
    }

    #[test]
    fn test_apply_mixed_changes() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 10, 0, 0).unwrap();
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![
                FileEntry {
                    path: "marco/existing.jpg".to_string(),
                    size: 1000,
                    mtime: Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap(),
                    fingerprint: "fp-existing".to_string(),
                    archive_id: "backup-1".to_string(),
                    history: vec![],
                },
                FileEntry {
                    path: "marco/to-move.jpg".to_string(),
                    size: 2000,
                    mtime: Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap(),
                    fingerprint: "fp-move".to_string(),
                    archive_id: "backup-1".to_string(),
                    history: vec![],
                },
                FileEntry {
                    path: "marco/to-delete.jpg".to_string(),
                    size: 3000,
                    mtime: Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap(),
                    fingerprint: "fp-delete".to_string(),
                    archive_id: "backup-1".to_string(),
                    history: vec![],
                },
            ],
        };

        let scan = make_scan_result(vec![
            FileChange::New {
                logical_path: "laura/new.jpg".to_string(),
                disk_path: PathBuf::from("/tmp/new.jpg"),
                size: 4000,
                mtime: now,
                fingerprint: "fp-new".to_string(),
            },
            FileChange::Modified {
                logical_path: "marco/existing.jpg".to_string(),
                disk_path: PathBuf::from("/tmp/existing.jpg"),
                size: 1500,
                mtime: now,
                fingerprint: "fp-existing-v2".to_string(),
                previous_archive_id: "backup-1".to_string(),
            },
            FileChange::Moved {
                logical_path: "common/to-move.jpg".to_string(),
                old_path: "marco/to-move.jpg".to_string(),
                fingerprint: "fp-move".to_string(),
            },
            FileChange::Deleted {
                logical_path: "marco/to-delete.jpg".to_string(),
            },
        ]);

        let result = apply_changes_to_manifest(
            manifest,
            &scan,
            "backup-2",
            "archives/backup-2.zip",
            5500,
            2,
            now,
        );

        assert_eq!(result.archives.len(), 1);
        assert_eq!(result.files.len(), 4); // 3 original + 1 new

        // New file
        let new_file = result.files.iter().find(|f| f.path == "laura/new.jpg").unwrap();
        assert_eq!(new_file.archive_id, "backup-2");

        // Modified
        let modified = result.files.iter().find(|f| f.path == "marco/existing.jpg").unwrap();
        assert_eq!(modified.size, 1500);
        assert_eq!(modified.archive_id, "backup-2");
        assert_eq!(modified.history.len(), 1);

        // Moved
        let moved = result.files.iter().find(|f| f.path == "common/to-move.jpg").unwrap();
        assert_eq!(moved.archive_id, "backup-1"); // content still in old archive
        assert_eq!(moved.history.len(), 1);

        // Deleted
        let deleted = result.files.iter().find(|f| f.path == "marco/to-delete.jpg").unwrap();
        assert_eq!(deleted.history.len(), 1);
        assert!(matches!(&deleted.history[0], HistoryEvent::Deleted { .. }));
    }

    #[test]
    fn test_no_archive_when_only_moves_and_deletes() {
        let now = Utc::now();
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "a/file.jpg".to_string(),
                size: 100,
                mtime: now,
                fingerprint: "fp".to_string(),
                archive_id: "old".to_string(),
                history: vec![],
            }],
        };

        let scan = make_scan_result(vec![FileChange::Moved {
            logical_path: "b/file.jpg".to_string(),
            old_path: "a/file.jpg".to_string(),
            fingerprint: "fp".to_string(),
        }]);

        let result = apply_changes_to_manifest(
            manifest, &scan, "backup-2", "archives/backup-2.zip", 0, 0, now,
        );

        // file_count is 0, so no archive added
        assert_eq!(result.archives.len(), 0);
    }
}
