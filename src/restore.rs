#![allow(dead_code)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::browse::{self, BrowseFilter};
use crate::manifest::Manifest;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RestoreJob {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub request_type: RestoreRequestType,
    pub archives: Vec<RestoreArchive>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RestoreRequestType {
    All,
    Path(String),
    Archive(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RestoreArchive {
    pub archive_id: String,
    pub s3_key: String,
    pub status: RestoreStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RestoreStatus {
    Requested,
    Available,
    Downloaded,
    Failed(String),
}

pub fn restores_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("restores")
}

pub fn determine_archives_needed(
    manifest: &Manifest,
    all: bool,
    path_pattern: Option<&str>,
    archive_id: Option<&str>,
) -> Result<Vec<(String, String)>> {
    if let Some(id) = archive_id {
        let archive = manifest
            .archives
            .iter()
            .find(|a| a.id == id)
            .with_context(|| format!("Archive not found: {}", id))?;
        return Ok(vec![(archive.id.clone(), archive.s3_key.clone())]);
    }

    let files = if all {
        manifest.files.clone()
    } else if let Some(pattern) = path_pattern {
        let filter = BrowseFilter {
            path_pattern: Some(pattern.to_string()),
            after: None,
            before: None,
        };
        let result = browse::browse(manifest, &filter);
        result
            .entries
            .iter()
            .map(|e| {
                manifest
                    .files
                    .iter()
                    .find(|f| f.path == e.path)
                    .unwrap()
                    .clone()
            })
            .collect()
    } else {
        anyhow::bail!("Must specify --all, --path, or --archive");
    };

    // Collect unique archive IDs needed
    let archive_ids: HashSet<&str> = files.iter().map(|f| f.archive_id.as_str()).collect();

    let archives: Vec<(String, String)> = manifest
        .archives
        .iter()
        .filter(|a| archive_ids.contains(a.id.as_str()))
        .map(|a| (a.id.clone(), a.s3_key.clone()))
        .collect();

    Ok(archives)
}

pub fn create_restore_job(
    request_type: RestoreRequestType,
    archives: Vec<(String, String)>,
) -> RestoreJob {
    let now = Utc::now();
    RestoreJob {
        id: format!("restore-{}", now.format("%Y%m%dT%H%M%S")),
        created_at: now,
        request_type,
        archives: archives
            .into_iter()
            .map(|(id, key)| RestoreArchive {
                archive_id: id,
                s3_key: key,
                status: RestoreStatus::Requested,
            })
            .collect(),
    }
}

pub fn save_restore_job(profile_dir: &Path, job: &RestoreJob) -> Result<()> {
    let dir = restores_dir(profile_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create restores dir: {}", dir.display()))?;
    let path = dir.join(format!("{}.json", job.id));
    let content =
        serde_json::to_string_pretty(job).with_context(|| "Failed to serialize restore job")?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write restore job: {}", path.display()))?;
    Ok(())
}

pub fn load_restore_jobs(profile_dir: &Path) -> Result<Vec<(PathBuf, RestoreJob)>> {
    let dir = restores_dir(profile_dir);
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut jobs = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("Failed to read restores dir: {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            let content = std::fs::read_to_string(&path)?;
            if let Ok(job) = serde_json::from_str::<RestoreJob>(&content) {
                jobs.push((path, job));
            }
        }
    }

    Ok(jobs)
}

pub fn update_restore_job(path: &Path, job: &RestoreJob) -> Result<()> {
    let content =
        serde_json::to_string_pretty(job).with_context(|| "Failed to serialize restore job")?;
    std::fs::write(path, content)
        .with_context(|| format!("Failed to update restore job: {}", path.display()))?;
    Ok(())
}

pub fn extract_archive(
    archive_path: &Path,
    output_dir: &Path,
    manifest: &Manifest,
    is_full_restore: bool,
) -> Result<u32> {
    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("Failed to open archive: {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(file);

    let mut extracted = 0u32;

    for entry in archive
        .entries()
        .with_context(|| format!("Failed to read archive: {}", archive_path.display()))?
    {
        let mut entry = entry.with_context(|| "Failed to read tar entry")?;
        let entry_path = entry.path()?.to_string_lossy().to_string();

        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }

        let dest = if is_full_restore {
            let is_latest = manifest
                .files
                .iter()
                .find(|f| f.path == entry_path)
                .map(|f| {
                    f.history
                        .last()
                        .is_none_or(|h| !matches!(h, crate::manifest::HistoryEvent::Deleted { .. }))
                })
                .unwrap_or(true);

            if is_latest {
                output_dir.join(&entry_path)
            } else {
                let versions_dir = output_dir.join("__versions");
                versions_dir.join(&entry_path)
            }
        } else {
            output_dir.join(&entry_path)
        };

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut out_file = std::fs::File::create(&dest)
            .with_context(|| format!("Failed to create: {}", dest.display()))?;
        std::io::copy(&mut entry, &mut out_file)?;
        extracted += 1;
    }

    Ok(extracted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Archive, FileEntry};
    use chrono::TimeZone;
    use tempfile::TempDir;

    // Tests now use TempDir as profile_dir directly instead of Config

    fn test_manifest() -> Manifest {
        Manifest {
            version: 1,
            last_backup: Some(Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap()),
            archives: vec![
                Archive {
                    id: "backup-1".to_string(),
                    s3_key: "archives/backup-1.tar".to_string(),
                    size_bytes: 1000,
                    created_at: Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap(),
                    file_count: 2,
                },
                Archive {
                    id: "backup-2".to_string(),
                    s3_key: "archives/backup-2.tar".to_string(),
                    size_bytes: 2000,
                    created_at: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
            ],
            files: vec![
                FileEntry {
                    path: "marco/photo1.jpg".to_string(),
                    size: 500,
                    mtime: Utc.with_ymd_and_hms(2026, 3, 15, 0, 0, 0).unwrap(),
                    fingerprint: "fp1".to_string(),
                    archive_id: "backup-1".to_string(),
                    history: vec![],
                },
                FileEntry {
                    path: "marco/photo2.jpg".to_string(),
                    size: 600,
                    mtime: Utc.with_ymd_and_hms(2026, 3, 20, 0, 0, 0).unwrap(),
                    fingerprint: "fp2".to_string(),
                    archive_id: "backup-1".to_string(),
                    history: vec![],
                },
                FileEntry {
                    path: "laura/photo3.jpg".to_string(),
                    size: 700,
                    mtime: Utc.with_ymd_and_hms(2026, 4, 10, 0, 0, 0).unwrap(),
                    fingerprint: "fp3".to_string(),
                    archive_id: "backup-2".to_string(),
                    history: vec![],
                },
            ],
        }
    }

    #[test]
    fn test_determine_archives_all() {
        let manifest = test_manifest();
        let result = determine_archives_needed(&manifest, true, None, None).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_determine_archives_by_id() {
        let manifest = test_manifest();
        let result = determine_archives_needed(&manifest, false, None, Some("backup-2")).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "backup-2");
        assert_eq!(result[0].1, "archives/backup-2.tar");
    }

    #[test]
    fn test_determine_archives_by_id_not_found() {
        let manifest = test_manifest();
        let result = determine_archives_needed(&manifest, false, None, Some("nonexistent"));
        assert!(result.is_err());
    }

    #[test]
    fn test_determine_archives_by_path() {
        let manifest = test_manifest();
        let result = determine_archives_needed(&manifest, false, Some("marco/**"), None).unwrap();
        // Both marco files are in backup-1
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "backup-1");
    }

    #[test]
    fn test_determine_archives_by_path_multiple() {
        let manifest = test_manifest();
        let result = determine_archives_needed(&manifest, false, Some("**/*.jpg"), None).unwrap();
        // Files span both archives
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_determine_archives_no_criteria_fails() {
        let manifest = test_manifest();
        let result = determine_archives_needed(&manifest, false, None, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_create_restore_job() {
        let archives = vec![
            ("backup-1".to_string(), "archives/backup-1.tar".to_string()),
            ("backup-2".to_string(), "archives/backup-2.tar".to_string()),
        ];

        let job = create_restore_job(RestoreRequestType::All, archives);
        assert!(job.id.starts_with("restore-"));
        assert_eq!(job.archives.len(), 2);
        assert!(matches!(job.archives[0].status, RestoreStatus::Requested));
        assert!(matches!(job.archives[1].status, RestoreStatus::Requested));
    }

    #[test]
    fn test_save_and_load_restore_job() {
        let dir = TempDir::new().unwrap();
        let profile_dir = dir.path();

        let job = create_restore_job(
            RestoreRequestType::Path("marco/**".to_string()),
            vec![("backup-1".to_string(), "archives/backup-1.tar".to_string())],
        );

        save_restore_job(profile_dir, &job).unwrap();

        let jobs = load_restore_jobs(profile_dir).unwrap();
        assert!(!jobs.is_empty());

        let loaded_job = jobs.iter().find(|(_, j)| j.id == job.id);
        assert!(loaded_job.is_some());
        assert_eq!(loaded_job.unwrap().1, job);
    }

    #[test]
    fn test_restore_job_serialization() {
        let job = RestoreJob {
            id: "restore-20260601T100000".to_string(),
            created_at: Utc.with_ymd_and_hms(2026, 6, 1, 10, 0, 0).unwrap(),
            request_type: RestoreRequestType::All,
            archives: vec![
                RestoreArchive {
                    archive_id: "backup-1".to_string(),
                    s3_key: "archives/backup-1.tar".to_string(),
                    status: RestoreStatus::Requested,
                },
                RestoreArchive {
                    archive_id: "backup-2".to_string(),
                    s3_key: "archives/backup-2.tar".to_string(),
                    status: RestoreStatus::Available,
                },
            ],
        };

        let json = serde_json::to_string_pretty(&job).unwrap();
        let loaded: RestoreJob = serde_json::from_str(&json).unwrap();
        assert_eq!(job, loaded);
    }

    #[test]
    fn test_update_restore_job() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("job.json");

        let mut job = create_restore_job(
            RestoreRequestType::All,
            vec![("backup-1".to_string(), "key".to_string())],
        );
        std::fs::write(&path, serde_json::to_string(&job).unwrap()).unwrap();

        job.archives[0].status = RestoreStatus::Available;
        update_restore_job(&path, &job).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: RestoreJob = serde_json::from_str(&content).unwrap();
        assert!(matches!(
            loaded.archives[0].status,
            RestoreStatus::Available
        ));
    }

    fn create_test_tar(dir: &Path, files: &[(&str, &[u8])]) -> PathBuf {
        use std::io::Write;

        let tar_path = dir.join("test.tar");
        let file = std::fs::File::create(&tar_path).unwrap();
        let mut tar = tar::Builder::new(file);

        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, &content[..]).unwrap();
        }
        tar.finish().unwrap();
        tar_path
    }

    #[test]
    fn test_extract_archive_basic() {
        let dir = TempDir::new().unwrap();
        let zip_path = create_test_tar(
            dir.path(),
            &[
                ("marco/photo1.jpg", b"photo 1 data"),
                ("marco/2026/05/photo2.jpg", b"photo 2 data"),
            ],
        );

        let output_dir = dir.path().join("output");
        let manifest = Manifest::new();

        let count = extract_archive(&zip_path, &output_dir, &manifest, false).unwrap();
        assert_eq!(count, 2);
        assert!(output_dir.join("marco/photo1.jpg").exists());
        assert!(output_dir.join("marco/2026/05/photo2.jpg").exists());

        let content = std::fs::read_to_string(output_dir.join("marco/photo1.jpg")).unwrap();
        assert_eq!(content, "photo 1 data");
    }

    #[test]
    fn test_extract_preserves_structure() {
        let dir = TempDir::new().unwrap();
        let zip_path = create_test_tar(
            dir.path(),
            &[
                ("marco/2026/01/a.jpg", b"a"),
                ("laura/2026/02/b.jpg", b"b"),
                ("common/trip/c.jpg", b"c"),
            ],
        );

        let output_dir = dir.path().join("restored");
        let manifest = Manifest::new();

        let count = extract_archive(&zip_path, &output_dir, &manifest, false).unwrap();
        assert_eq!(count, 3);
        assert!(output_dir.join("marco/2026/01/a.jpg").exists());
        assert!(output_dir.join("laura/2026/02/b.jpg").exists());
        assert!(output_dir.join("common/trip/c.jpg").exists());
    }

    #[test]
    fn test_extract_full_restore_deleted_goes_to_versions() {
        use crate::manifest::HistoryEvent;

        let dir = TempDir::new().unwrap();
        let zip_path = create_test_tar(dir.path(), &[("marco/deleted.jpg", b"old content")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "marco/deleted.jpg".to_string(),
                size: 11,
                mtime: Utc::now(),
                fingerprint: "fp".to_string(),
                archive_id: "a1".to_string(),
                history: vec![HistoryEvent::Deleted { at: Utc::now() }],
            }],
        };

        let count = extract_archive(&zip_path, &output_dir, &manifest, true).unwrap();
        assert_eq!(count, 1);
        // Deleted file goes to __versions/
        assert!(output_dir.join("__versions/marco/deleted.jpg").exists());
        assert!(!output_dir.join("marco/deleted.jpg").exists());
    }

    #[test]
    fn test_extract_full_restore_current_goes_to_normal_path() {
        let dir = TempDir::new().unwrap();
        let zip_path = create_test_tar(dir.path(), &[("marco/current.jpg", b"current content")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "marco/current.jpg".to_string(),
                size: 15,
                mtime: Utc::now(),
                fingerprint: "fp".to_string(),
                archive_id: "a1".to_string(),
                history: vec![],
            }],
        };

        let count = extract_archive(&zip_path, &output_dir, &manifest, true).unwrap();
        assert_eq!(count, 1);
        assert!(output_dir.join("marco/current.jpg").exists());
        assert!(!output_dir.join("__versions/marco/current.jpg").exists());
    }
}
