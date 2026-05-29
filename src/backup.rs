#![allow(dead_code)]

use anyhow::Result;
use chrono::{DateTime, Utc};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;

use crate::archiver;
use crate::config::Config;
use crate::manifest::{self, Archive, FileEntry, HistoryEvent, Manifest};
use crate::scanner::{self, FileChange};
#[cfg(test)]
use crate::scanner::ScanResult;
use crate::uploader;

pub struct BackupOptions {
    pub dry_run: bool,
    pub cutoff_override: Option<String>,
    pub verbose: bool,
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
    let manifest = manifest::load_or_create(config, profile_dir).await?;

    // 3. Scan with progress spinner
    let scan_spinner = ProgressBar::new_spinner();
    scan_spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    scan_spinner.set_message("Scanning sources...");
    scan_spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let verbose = options.verbose;
    let scan_result = scanner::scan(
        &config.backup.sources,
        &manifest,
        cutoff,
        &config.backup.filter.exclude,
        verbose,
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
        print_dry_run_plan(&scan_result, config);
        return Ok(BackupReport {
            scan_stats: scan_result.stats,
            archive_size: None,
            archive_file_count: 0,
            manifest_updated: false,
        });
    }

    // 4. Plan and create archives (grouped by month, capped by size)
    let now = Utc::now();
    let max_zip_bytes = config.backup.max_archive_size_mb * 1024 * 1024;
    let archive_plan = archiver::plan_archives(&scan_result.changes, max_zip_bytes);

    let mut total_archive_size: u64 = 0;
    let mut total_archive_file_count: u32 = 0;
    let mut manifest = manifest;

    if !archive_plan.groups.is_empty() {
        let tmp_dir = &config.backup.tmp_dir;
        std::fs::create_dir_all(tmp_dir)?;
        let s3_client = crate::util::create_s3_client(config).await;

        let total_files = archive_plan.total_files() as u64;
        let archive_bar = ProgressBar::new(total_files);
        archive_bar.set_style(
            ProgressStyle::with_template(
                "  Archiving [{bar:40.cyan/dim}] {pos}/{len} files  ETA {eta}",
            )
            .unwrap()
            .progress_chars("##-"),
        );

        let mut files_done: u64 = 0;

        for (group_idx, group) in archive_plan.groups.iter().enumerate() {
            let archive_id = format!("backup-{}-run{}", group.label, now.to_rfc3339());
            let s3_key = format!(
                "{}backup-{}-run{}.tar",
                config.storage.archive_prefix,
                group.label,
                now.format("%Y%m%dT%H%M%S")
            );

            let archive_path = tmp_dir.join(format!(
                "backup-{}-run{}.tar",
                group.label,
                now.format("%Y%m%dT%H%M%S")
            ));

            let result = archiver::create_archive_from_group(&archive_path, group, |current, _total| {
                archive_bar.set_position(files_done + current as u64);
            })?;

            files_done += result.file_count as u64;
            total_archive_size += result.size_bytes;
            total_archive_file_count += result.file_count;

            archive_bar.set_position(files_done);

            // Upload
            eprintln!(
                "  Uploading archive {}/{}: {} ({:.1} MB)",
                group_idx + 1,
                archive_plan.groups.len(),
                group.label,
                result.size_bytes as f64 / 1024.0 / 1024.0
            );
            uploader::upload_archive(&s3_client, config, profile_dir, &s3_key, &archive_path).await?;

            // Clean up local zip
            let _ = std::fs::remove_file(&archive_path);

            // Update manifest incrementally after each successful upload
            manifest.archives.push(Archive {
                id: archive_id.clone(),
                s3_key: s3_key.clone(),
                size_bytes: result.size_bytes,
                created_at: now,
                file_count: result.file_count,
            });
            for (logical_path, _, _) in &group.files {
                add_file_to_manifest(&mut manifest, &scan_result.changes, logical_path, &archive_id);
            }
            manifest.last_backup = Some(now);
            manifest::save_to_s3(config, profile_dir, &manifest).await?;
        }

        archive_bar.finish_and_clear();
    }

    // 5. Apply non-archive changes (moves, deletes) and final save
    let has_moves_or_deletes = scan_result.changes.iter().any(|c| {
        matches!(c, FileChange::Moved { .. } | FileChange::Deleted { .. })
    });
    if has_moves_or_deletes || archive_plan.groups.is_empty() {
        apply_moves_and_deletes(&mut manifest, &scan_result.changes, now);
        manifest.last_backup = Some(now);
        manifest::save_to_s3(config, profile_dir, &manifest).await?;
    }

    eprintln!("  Manifest saved ({} files tracked).", manifest.files.len());

    Ok(BackupReport {
        scan_stats: scan_result.stats,
        archive_size: if total_archive_size > 0 { Some(total_archive_size) } else { None },
        archive_file_count: total_archive_file_count,
        manifest_updated: true,
    })
}


fn print_dry_run_plan(scan_result: &scanner::ScanResult, config: &Config) {
    let max_zip_bytes = config.backup.max_archive_size_mb * 1024 * 1024;
    let archive_plan = archiver::plan_archives(&scan_result.changes, max_zip_bytes);

    let moved_files: Vec<_> = scan_result.changes.iter().filter_map(|c| match c {
        FileChange::Moved { logical_path, old_path, .. } => Some((old_path.as_str(), logical_path.as_str())),
        _ => None,
    }).collect();

    let deleted_files: Vec<_> = scan_result.changes.iter().filter_map(|c| match c {
        FileChange::Deleted { logical_path } => Some(logical_path.as_str()),
        _ => None,
    }).collect();

    println!("\nDry run — backup plan:");
    println!("  Storage class: {}", config.storage.storage_class);
    println!("  Max archive size: {} MB", config.backup.max_archive_size_mb);

    if archive_plan.groups.is_empty() {
        println!("  No files to archive.");
    } else {
        println!(
            "  Archives to create: {} ({} files, ~{})",
            archive_plan.groups.len(),
            archive_plan.total_files(),
            format_bytes(archive_plan.total_size()),
        );
        println!();
        for group in &archive_plan.groups {
            println!(
                "    {}: {} ({} files)",
                group.label,
                format_bytes(group.total_size),
                group.files.len()
            );
            for (i, (logical_path, _, size)) in group.files.iter().enumerate() {
                if i >= 20 {
                    println!("      ... and {} more", group.files.len() - 20);
                    break;
                }
                println!("      {} ({})", logical_path, format_bytes(*size));
            }
        }
    }

    if !moved_files.is_empty() {
        println!("\n  Moves to record: {}", moved_files.len());
        for (i, (from, to)) in moved_files.iter().enumerate() {
            if i >= 10 {
                println!("    ... and {} more", moved_files.len() - 10);
                break;
            }
            println!("    {} -> {}", from, to);
        }
    }

    if !deleted_files.is_empty() {
        println!("\n  Deletions to record: {}", deleted_files.len());
        for (i, path) in deleted_files.iter().enumerate() {
            if i >= 10 {
                println!("    ... and {} more", deleted_files.len() - 10);
                break;
            }
            println!("    {}", path);
        }
    }

    println!("\n(dry run — no changes made)");
}

fn format_bytes(bytes: u64) -> String {
    crate::util::format_bytes(bytes)
}

pub fn run_status(profile_dir: &std::path::Path, profile_name: &str) -> Result<()> {
    let local_manifest_path = manifest::manifest_local_path(profile_dir);

    if local_manifest_path.exists() {
        let m = manifest::load_from_file(&local_manifest_path)?;
        println!("Backup Status (profile: '{}'):", profile_name);
        println!(
            "  Last backup: {}",
            m.last_backup
                .map_or("never".to_string(), |t| t.to_rfc3339())
        );
        println!("  Total archives: {}", m.archives.len());
        println!("  Total files tracked: {}", m.files.len());

        let total_size: u64 = m.archives.iter().map(|a| a.size_bytes).sum();
        println!("  Total archive size: {}", format_bytes(total_size));
    } else {
        println!("No backup data found. Run 'coldpack backup' to start.");
    }

    let jobs = crate::restore::load_restore_jobs(profile_dir)?;
    if !jobs.is_empty() {
        println!("\nPending Restores:");
        for (_, job) in &jobs {
            let pending = job
                .archives
                .iter()
                .filter(|a| matches!(a.status, crate::restore::RestoreStatus::Requested))
                .count();
            let available = job
                .archives
                .iter()
                .filter(|a| matches!(a.status, crate::restore::RestoreStatus::Available))
                .count();
            println!("  {} — {} pending, {} available", job.id, pending, available);
        }
    }

    let cp_dir = uploader::checkpoint_dir(profile_dir);
    if cp_dir.exists() {
        let count = std::fs::read_dir(&cp_dir)?.filter(|e| e.is_ok()).count();
        if count > 0 {
            println!(
                "\nStale Uploads: {} checkpoint file(s) found. Run 'coldpack cleanup' to resolve.",
                count
            );
        }
    }

    Ok(())
}


fn add_file_to_manifest(
    manifest: &mut Manifest,
    changes: &[FileChange],
    logical_path: &str,
    archive_id: &str,
) {
    let change = changes.iter().find(|c| match c {
        FileChange::New { logical_path: p, .. } | FileChange::Modified { logical_path: p, .. } => p == logical_path,
        _ => false,
    });

    let Some(change) = change else { return };

    match change {
        FileChange::New { logical_path, size, mtime, fingerprint, .. } => {
            manifest.files.push(FileEntry {
                path: logical_path.clone(),
                size: *size,
                mtime: *mtime,
                fingerprint: fingerprint.clone(),
                archive_id: archive_id.to_string(),
                history: vec![],
            });
        }
        FileChange::Modified { logical_path, size, mtime, fingerprint, .. } => {
            if let Some(entry) = manifest.files.iter_mut().find(|f| f.path == *logical_path) {
                entry.history.push(HistoryEvent::Added {
                    archive_id: entry.archive_id.clone(),
                    mtime: entry.mtime,
                    size: entry.size,
                });
                entry.size = *size;
                entry.mtime = *mtime;
                entry.fingerprint = fingerprint.clone();
                entry.archive_id = archive_id.to_string();
            }
        }
        _ => {}
    }
}

fn apply_moves_and_deletes(
    manifest: &mut Manifest,
    changes: &[FileChange],
    now: DateTime<Utc>,
) {
    for change in changes {
        match change {
            FileChange::Moved { logical_path, old_path, fingerprint } => {
                if let Some(entry) = manifest.files.iter_mut().find(|f| f.path == *old_path) {
                    entry.history.push(HistoryEvent::Moved {
                        from: old_path.clone(),
                        at: now,
                    });
                    entry.path = logical_path.clone();
                    entry.fingerprint = fingerprint.clone();
                }
            }
            FileChange::Deleted { logical_path } => {
                if let Some(entry) = manifest.files.iter_mut().find(|f| f.path == *logical_path) {
                    entry.history.push(HistoryEvent::Deleted { at: now });
                }
            }
            _ => {}
        }
    }
}


#[cfg(test)]
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
    use std::path::PathBuf;

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
            "archives/backup-1.tar",
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
                s3_key: "archives/old.tar".to_string(),
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
            "archives/backup-2.tar",
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
            manifest, &scan, "backup-2", "archives/backup-2.tar", 0, 0, now,
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
            manifest, &scan, "backup-2", "archives/backup-2.tar", 0, 0, now,
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
            "archives/backup-2.tar",
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
            manifest, &scan, "backup-2", "archives/backup-2.tar", 0, 0, now,
        );

        // file_count is 0, so no archive added
        assert_eq!(result.archives.len(), 0);
    }
}
