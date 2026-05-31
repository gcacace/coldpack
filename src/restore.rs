use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::browse::{self, BrowseFilter};
use crate::manifest::{HistoryEvent, Manifest};

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

#[derive(Debug, Clone, PartialEq)]
pub struct ExtractOptions {
    pub include_deleted: bool,
    pub include_versions: VersionMode,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VersionMode {
    None,
    Latest,
    All,
}

#[cfg(test)]
use crate::manifest::FileEntry;
#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
enum ExtractionDecision {
    Current {
        dest_path: String,
    },
    Version {
        dest_path: String,
        date_prefix: Option<String>,
    },
    Skip,
}

/// Determines what path a file had at a given point in time by walking
/// Moved events in reverse. Each Moved { from, at } means "before `at`,
/// the path was `from`".
#[cfg(test)]
fn path_at_time(file: &FileEntry, target_time: DateTime<Utc>) -> &str {
    let mut current_path = file.path.as_str();
    for event in file.history.iter().rev() {
        if let HistoryEvent::Moved { from, at } = event {
            if target_time <= *at {
                current_path = from.as_str();
            }
        }
    }
    current_path
}

pub fn restores_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("restores")
}

#[cfg(test)]
fn build_extraction_plan(
    manifest: &Manifest,
    archive_id: &str,
    options: &ExtractOptions,
) -> HashMap<String, ExtractionDecision> {
    let archive_time = manifest
        .archives
        .iter()
        .find(|a| a.id == archive_id)
        .map(|a| a.created_at);

    let archive_date = archive_time.map(|t| t.format("%Y-%m-%d").to_string());

    let mut plan: HashMap<String, ExtractionDecision> = HashMap::new();

    for file in &manifest.files {
        let is_deleted = file
            .history
            .last()
            .is_some_and(|h| matches!(h, HistoryEvent::Deleted { .. }));

        // Case A: This archive has the LATEST version of this file
        if file.archive_id == archive_id {
            let path_in_archive = match archive_time {
                Some(t) => path_at_time(file, t).to_string(),
                None => file.path.clone(),
            };

            let decision = if is_deleted {
                if options.include_deleted {
                    ExtractionDecision::Version {
                        dest_path: file.path.clone(),
                        date_prefix: archive_date.clone(),
                    }
                } else {
                    ExtractionDecision::Skip
                }
            } else {
                ExtractionDecision::Current {
                    dest_path: file.path.clone(),
                }
            };

            plan.insert(path_in_archive, decision);
        }

        // Case B: This archive has an OLDER version (from HistoryEvent::Added)
        for (event_idx, event) in file.history.iter().enumerate() {
            if let HistoryEvent::Added {
                archive_id: added_aid,
                ..
            } = event
            {
                if added_aid != archive_id {
                    continue;
                }

                let should_include = match options.include_versions {
                    VersionMode::None => false,
                    VersionMode::All => true,
                    VersionMode::Latest => {
                        // Only include the most recent previous version (last Added event)
                        let is_last_added = !file.history[event_idx + 1..]
                            .iter()
                            .any(|h| matches!(h, HistoryEvent::Added { .. }));
                        is_last_added
                    }
                };

                let path_in_archive = match archive_time {
                    Some(t) => path_at_time(file, t).to_string(),
                    None => file.path.clone(),
                };

                let decision = if should_include {
                    ExtractionDecision::Version {
                        dest_path: file.path.clone(),
                        date_prefix: archive_date.clone(),
                    }
                } else {
                    ExtractionDecision::Skip
                };

                // Only insert if not already claimed by Case A (latest version wins)
                plan.entry(path_in_archive).or_insert(decision);
            }
        }
    }

    plan
}

pub fn determine_archives_needed(
    manifest: &Manifest,
    all: bool,
    path_pattern: Option<&str>,
    archive_id: Option<&str>,
    options: &ExtractOptions,
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
    let mut archive_ids: HashSet<&str> = files.iter().map(|f| f.archive_id.as_str()).collect();

    // When include_versions is enabled, also collect archives from Added history events
    if options.include_versions != VersionMode::None {
        for file in &files {
            for event in &file.history {
                if let HistoryEvent::Added {
                    archive_id: aid, ..
                } = event
                {
                    archive_ids.insert(aid.as_str());
                }
            }
        }
    }

    // When include_deleted is enabled, also include archives for deleted files
    if options.include_deleted && !all {
        for file in &manifest.files {
            let is_deleted = file
                .history
                .last()
                .is_some_and(|h| matches!(h, HistoryEvent::Deleted { .. }));
            if is_deleted {
                archive_ids.insert(file.archive_id.as_str());
            }
        }
    }

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

#[cfg(test)]
pub fn update_restore_job(path: &Path, job: &RestoreJob) -> Result<()> {
    let content =
        serde_json::to_string_pretty(job).with_context(|| "Failed to serialize restore job")?;
    std::fs::write(path, content)
        .with_context(|| format!("Failed to update restore job: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
pub fn extract_archive(
    archive_path: &Path,
    output_dir: &Path,
    manifest: &Manifest,
    archive_id: &str,
    options: &ExtractOptions,
) -> Result<u32> {
    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("Failed to open archive: {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(file);

    let plan = build_extraction_plan(manifest, archive_id, options);
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

        let dest = match plan.get(&entry_path) {
            Some(ExtractionDecision::Current { dest_path }) => output_dir.join(dest_path),
            Some(ExtractionDecision::Version {
                dest_path,
                date_prefix,
            }) => {
                let versions_dir = output_dir.join("__versions");
                match date_prefix {
                    Some(prefix) => versions_dir.join(prefix).join(dest_path),
                    None => versions_dir.join(dest_path),
                }
            }
            Some(ExtractionDecision::Skip) => continue,
            None => {
                // Entry not in manifest — fallback to raw tar path
                output_dir.join(&entry_path)
            }
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

    fn default_options() -> ExtractOptions {
        ExtractOptions {
            include_deleted: false,
            include_versions: VersionMode::None,
        }
    }

    fn full_options() -> ExtractOptions {
        ExtractOptions {
            include_deleted: true,
            include_versions: VersionMode::All,
        }
    }

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
        let result =
            determine_archives_needed(&manifest, true, None, None, &default_options()).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_determine_archives_by_id() {
        let manifest = test_manifest();
        let result =
            determine_archives_needed(&manifest, false, None, Some("backup-2"), &default_options())
                .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "backup-2");
        assert_eq!(result[0].1, "archives/backup-2.tar");
    }

    #[test]
    fn test_determine_archives_by_id_not_found() {
        let manifest = test_manifest();
        let result = determine_archives_needed(
            &manifest,
            false,
            None,
            Some("nonexistent"),
            &default_options(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_determine_archives_by_path() {
        let manifest = test_manifest();
        let result =
            determine_archives_needed(&manifest, false, Some("marco/**"), None, &default_options())
                .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "backup-1");
    }

    #[test]
    fn test_determine_archives_by_path_multiple() {
        let manifest = test_manifest();
        let result =
            determine_archives_needed(&manifest, false, Some("**/*.jpg"), None, &default_options())
                .unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_determine_archives_no_criteria_fails() {
        let manifest = test_manifest();
        let result = determine_archives_needed(&manifest, false, None, None, &default_options());
        assert!(result.is_err());
    }

    #[test]
    fn test_determine_archives_includes_historical_when_versions_enabled() {
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![
                Archive {
                    id: "A1".to_string(),
                    s3_key: "archives/A1.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
                Archive {
                    id: "A2".to_string(),
                    s3_key: "archives/A2.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
            ],
            files: vec![FileEntry {
                path: "photo.jpg".to_string(),
                size: 100,
                mtime: Utc::now(),
                fingerprint: "fp".to_string(),
                archive_id: "A2".to_string(),
                history: vec![HistoryEvent::Added {
                    archive_id: "A1".to_string(),
                    mtime: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                    size: 90,
                }],
            }],
        };

        // Without versions: only A2 needed
        let result =
            determine_archives_needed(&manifest, true, None, None, &default_options()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "A2");

        // With versions: both A1 and A2 needed
        let opts = ExtractOptions {
            include_deleted: false,
            include_versions: VersionMode::Latest,
        };
        let result = determine_archives_needed(&manifest, true, None, None, &opts).unwrap();
        assert_eq!(result.len(), 2);
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

    // --- Extract tests: basic behavior ---

    #[test]
    fn test_extract_archive_basic() {
        let dir = TempDir::new().unwrap();
        let tar_path = create_test_tar(
            dir.path(),
            &[
                ("marco/photo1.jpg", b"photo 1 data"),
                ("marco/2026/05/photo2.jpg", b"photo 2 data"),
            ],
        );

        let output_dir = dir.path().join("output");
        let manifest = Manifest::new();

        let count = extract_archive(
            &tar_path,
            &output_dir,
            &manifest,
            "any-id",
            &default_options(),
        )
        .unwrap();
        assert_eq!(count, 2);
        assert!(output_dir.join("marco/photo1.jpg").exists());
        assert!(output_dir.join("marco/2026/05/photo2.jpg").exists());

        let content = std::fs::read_to_string(output_dir.join("marco/photo1.jpg")).unwrap();
        assert_eq!(content, "photo 1 data");
    }

    #[test]
    fn test_extract_preserves_structure() {
        let dir = TempDir::new().unwrap();
        let tar_path = create_test_tar(
            dir.path(),
            &[
                ("marco/2026/01/a.jpg", b"a"),
                ("laura/2026/02/b.jpg", b"b"),
                ("common/trip/c.jpg", b"c"),
            ],
        );

        let output_dir = dir.path().join("restored");
        let manifest = Manifest::new();

        let count = extract_archive(
            &tar_path,
            &output_dir,
            &manifest,
            "any-id",
            &default_options(),
        )
        .unwrap();
        assert_eq!(count, 3);
        assert!(output_dir.join("marco/2026/01/a.jpg").exists());
        assert!(output_dir.join("laura/2026/02/b.jpg").exists());
        assert!(output_dir.join("common/trip/c.jpg").exists());
    }

    // --- Extract tests: moved files ---

    #[test]
    fn test_extract_moved_file_goes_to_new_path() {
        let dir = TempDir::new().unwrap();
        // Archive has file under old path
        let tar_path = create_test_tar(dir.path(), &[("marco/photo.jpg", b"photo data")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![Archive {
                id: "A1".to_string(),
                s3_key: "archives/A1.tar".to_string(),
                size_bytes: 100,
                created_at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                file_count: 1,
            }],
            files: vec![FileEntry {
                path: "common/photo.jpg".to_string(), // current path (after move)
                size: 10,
                mtime: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp".to_string(),
                archive_id: "A1".to_string(),
                history: vec![HistoryEvent::Moved {
                    from: "marco/photo.jpg".to_string(),
                    at: Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap(),
                }],
            }],
        };

        let count =
            extract_archive(&tar_path, &output_dir, &manifest, "A1", &default_options()).unwrap();
        assert_eq!(count, 1);
        // File should be at the NEW path, not old
        assert!(output_dir.join("common/photo.jpg").exists());
        assert!(!output_dir.join("marco/photo.jpg").exists());
    }

    #[test]
    fn test_extract_multiple_moves_resolves_correctly() {
        let dir = TempDir::new().unwrap();
        // Archive has file under original path
        let tar_path = create_test_tar(dir.path(), &[("a/photo.jpg", b"data")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![Archive {
                id: "A1".to_string(),
                s3_key: "archives/A1.tar".to_string(),
                size_bytes: 100,
                created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                file_count: 1,
            }],
            files: vec![FileEntry {
                path: "c/photo.jpg".to_string(), // final path after A->B->C
                size: 10,
                mtime: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp".to_string(),
                archive_id: "A1".to_string(),
                history: vec![
                    HistoryEvent::Moved {
                        from: "a/photo.jpg".to_string(),
                        at: Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap(),
                    },
                    HistoryEvent::Moved {
                        from: "b/photo.jpg".to_string(),
                        at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                    },
                ],
            }],
        };

        let count =
            extract_archive(&tar_path, &output_dir, &manifest, "A1", &default_options()).unwrap();
        assert_eq!(count, 1);
        assert!(output_dir.join("c/photo.jpg").exists());
    }

    // --- Extract tests: deleted files ---

    #[test]
    fn test_extract_deleted_file_skipped_by_default() {
        let dir = TempDir::new().unwrap();
        let tar_path = create_test_tar(dir.path(), &[("marco/deleted.jpg", b"old content")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![Archive {
                id: "A1".to_string(),
                s3_key: "archives/A1.tar".to_string(),
                size_bytes: 100,
                created_at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                file_count: 1,
            }],
            files: vec![FileEntry {
                path: "marco/deleted.jpg".to_string(),
                size: 11,
                mtime: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp".to_string(),
                archive_id: "A1".to_string(),
                history: vec![HistoryEvent::Deleted {
                    at: Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap(),
                }],
            }],
        };

        let count =
            extract_archive(&tar_path, &output_dir, &manifest, "A1", &default_options()).unwrap();
        assert_eq!(count, 0);
        assert!(!output_dir.join("marco/deleted.jpg").exists());
    }

    #[test]
    fn test_extract_deleted_file_goes_to_versions_when_included() {
        let dir = TempDir::new().unwrap();
        let tar_path = create_test_tar(dir.path(), &[("marco/deleted.jpg", b"old content")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![Archive {
                id: "A1".to_string(),
                s3_key: "archives/A1.tar".to_string(),
                size_bytes: 100,
                created_at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                file_count: 1,
            }],
            files: vec![FileEntry {
                path: "marco/deleted.jpg".to_string(),
                size: 11,
                mtime: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp".to_string(),
                archive_id: "A1".to_string(),
                history: vec![HistoryEvent::Deleted {
                    at: Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap(),
                }],
            }],
        };

        let opts = ExtractOptions {
            include_deleted: true,
            include_versions: VersionMode::None,
        };
        let count = extract_archive(&tar_path, &output_dir, &manifest, "A1", &opts).unwrap();
        assert_eq!(count, 1);
        assert!(output_dir
            .join("__versions/2026-03-01/marco/deleted.jpg")
            .exists());
        assert!(!output_dir.join("marco/deleted.jpg").exists());
    }

    // --- Extract tests: current file (no move, no delete) ---

    #[test]
    fn test_extract_current_file_goes_to_normal_path() {
        let dir = TempDir::new().unwrap();
        let tar_path = create_test_tar(dir.path(), &[("marco/current.jpg", b"current content")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![Archive {
                id: "A1".to_string(),
                s3_key: "archives/A1.tar".to_string(),
                size_bytes: 100,
                created_at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                file_count: 1,
            }],
            files: vec![FileEntry {
                path: "marco/current.jpg".to_string(),
                size: 15,
                mtime: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp".to_string(),
                archive_id: "A1".to_string(),
                history: vec![],
            }],
        };

        let count =
            extract_archive(&tar_path, &output_dir, &manifest, "A1", &default_options()).unwrap();
        assert_eq!(count, 1);
        assert!(output_dir.join("marco/current.jpg").exists());
        assert!(!output_dir.join("__versions/marco/current.jpg").exists());
    }

    // --- Extract tests: modified files (old versions) ---

    #[test]
    fn test_extract_old_version_skipped_by_default() {
        let dir = TempDir::new().unwrap();
        // A1 has the old version
        let tar_path = create_test_tar(dir.path(), &[("photo.jpg", b"old version")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![
                Archive {
                    id: "A1".to_string(),
                    s3_key: "archives/A1.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
                Archive {
                    id: "A2".to_string(),
                    s3_key: "archives/A2.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
            ],
            files: vec![FileEntry {
                path: "photo.jpg".to_string(),
                size: 200,
                mtime: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp-new".to_string(),
                archive_id: "A2".to_string(), // latest is in A2
                history: vec![HistoryEvent::Added {
                    archive_id: "A1".to_string(), // old version in A1
                    mtime: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                    size: 100,
                }],
            }],
        };

        // Extracting A1 with default options should skip the old version
        let count =
            extract_archive(&tar_path, &output_dir, &manifest, "A1", &default_options()).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_extract_old_version_goes_to_versions_when_included() {
        let dir = TempDir::new().unwrap();
        let tar_path = create_test_tar(dir.path(), &[("photo.jpg", b"old version")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![
                Archive {
                    id: "A1".to_string(),
                    s3_key: "archives/A1.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
                Archive {
                    id: "A2".to_string(),
                    s3_key: "archives/A2.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
            ],
            files: vec![FileEntry {
                path: "photo.jpg".to_string(),
                size: 200,
                mtime: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp-new".to_string(),
                archive_id: "A2".to_string(),
                history: vec![HistoryEvent::Added {
                    archive_id: "A1".to_string(),
                    mtime: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                    size: 100,
                }],
            }],
        };

        let opts = ExtractOptions {
            include_deleted: false,
            include_versions: VersionMode::All,
        };
        let count = extract_archive(&tar_path, &output_dir, &manifest, "A1", &opts).unwrap();
        assert_eq!(count, 1);
        assert!(output_dir.join("__versions/2026-03-01/photo.jpg").exists());
        assert!(!output_dir.join("photo.jpg").exists());
    }

    #[test]
    fn test_extract_latest_version_goes_to_current_path() {
        let dir = TempDir::new().unwrap();
        // A2 has the latest version
        let tar_path = create_test_tar(dir.path(), &[("photo.jpg", b"new version")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![
                Archive {
                    id: "A1".to_string(),
                    s3_key: "archives/A1.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
                Archive {
                    id: "A2".to_string(),
                    s3_key: "archives/A2.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
            ],
            files: vec![FileEntry {
                path: "photo.jpg".to_string(),
                size: 200,
                mtime: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp-new".to_string(),
                archive_id: "A2".to_string(),
                history: vec![HistoryEvent::Added {
                    archive_id: "A1".to_string(),
                    mtime: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                    size: 100,
                }],
            }],
        };

        // Extracting A2 should put it at the current path
        let count =
            extract_archive(&tar_path, &output_dir, &manifest, "A2", &full_options()).unwrap();
        assert_eq!(count, 1);
        assert!(output_dir.join("photo.jpg").exists());
    }

    // --- Extract tests: moved + modified ---

    #[test]
    fn test_extract_moved_then_modified_old_version() {
        // Timeline: file at "marco/photo.jpg" in A1, moved to "common/photo.jpg", then modified (new content in A3)
        // Extracting A1 with include_versions should put it at __versions/2026-01-01/common/photo.jpg
        let dir = TempDir::new().unwrap();
        let tar_path = create_test_tar(dir.path(), &[("marco/photo.jpg", b"original")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![
                Archive {
                    id: "A1".to_string(),
                    s3_key: "archives/A1.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
                Archive {
                    id: "A3".to_string(),
                    s3_key: "archives/A3.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
            ],
            files: vec![FileEntry {
                path: "common/photo.jpg".to_string(),
                size: 200,
                mtime: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp-new".to_string(),
                archive_id: "A3".to_string(),
                history: vec![
                    HistoryEvent::Moved {
                        from: "marco/photo.jpg".to_string(),
                        at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                    },
                    HistoryEvent::Added {
                        archive_id: "A1".to_string(),
                        mtime: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                        size: 100,
                    },
                ],
            }],
        };

        let opts = ExtractOptions {
            include_deleted: false,
            include_versions: VersionMode::All,
        };
        let count = extract_archive(&tar_path, &output_dir, &manifest, "A1", &opts).unwrap();
        assert_eq!(count, 1);
        // Old version goes to __versions with current dest_path (common/photo.jpg)
        assert!(output_dir
            .join("__versions/2026-01-01/common/photo.jpg")
            .exists());
    }

    // --- Extract tests: moved file that's the latest version ---

    #[test]
    fn test_extract_moved_file_latest_version_uses_current_path() {
        // File was moved but not modified: archive has old path, should extract to new path
        let dir = TempDir::new().unwrap();
        let tar_path = create_test_tar(dir.path(), &[("old/photo.jpg", b"content")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![Archive {
                id: "A1".to_string(),
                s3_key: "archives/A1.tar".to_string(),
                size_bytes: 100,
                created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                file_count: 1,
            }],
            files: vec![FileEntry {
                path: "new/photo.jpg".to_string(),
                size: 7,
                mtime: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp".to_string(),
                archive_id: "A1".to_string(),
                history: vec![HistoryEvent::Moved {
                    from: "old/photo.jpg".to_string(),
                    at: Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap(),
                }],
            }],
        };

        let count =
            extract_archive(&tar_path, &output_dir, &manifest, "A1", &full_options()).unwrap();
        assert_eq!(count, 1);
        assert!(output_dir.join("new/photo.jpg").exists());
        assert!(!output_dir.join("old/photo.jpg").exists());
    }

    // --- Extract tests: entry not in manifest (fallback) ---

    #[test]
    fn test_extract_unknown_entry_uses_raw_path() {
        let dir = TempDir::new().unwrap();
        let tar_path = create_test_tar(dir.path(), &[("unknown/file.jpg", b"mystery")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest::new(); // empty manifest

        let count =
            extract_archive(&tar_path, &output_dir, &manifest, "A1", &full_options()).unwrap();
        assert_eq!(count, 1);
        assert!(output_dir.join("unknown/file.jpg").exists());
    }

    // --- Extract tests: include_versions=latest only includes most recent previous ---

    #[test]
    fn test_extract_versions_latest_only_includes_most_recent() {
        let dir = TempDir::new().unwrap();
        // A1 has the oldest version, A2 has the middle version
        let tar_a1 = create_test_tar(dir.path(), &[("photo.jpg", b"v1")]);

        let output_dir = dir.path().join("output");
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![
                Archive {
                    id: "A1".to_string(),
                    s3_key: "archives/A1.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
                Archive {
                    id: "A2".to_string(),
                    s3_key: "archives/A2.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
                Archive {
                    id: "A3".to_string(),
                    s3_key: "archives/A3.tar".to_string(),
                    size_bytes: 100,
                    created_at: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                    file_count: 1,
                },
            ],
            files: vec![FileEntry {
                path: "photo.jpg".to_string(),
                size: 300,
                mtime: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
                fingerprint: "fp-v3".to_string(),
                archive_id: "A3".to_string(),
                history: vec![
                    HistoryEvent::Added {
                        archive_id: "A1".to_string(),
                        mtime: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                        size: 100,
                    },
                    HistoryEvent::Added {
                        archive_id: "A2".to_string(),
                        mtime: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
                        size: 200,
                    },
                ],
            }],
        };

        // With VersionMode::Latest, only A2 (the most recent previous) should be included
        let opts = ExtractOptions {
            include_deleted: false,
            include_versions: VersionMode::Latest,
        };

        // Extracting A1 should SKIP (it's not the most recent previous version)
        let count = extract_archive(&tar_a1, &output_dir, &manifest, "A1", &opts).unwrap();
        assert_eq!(count, 0);

        // Extracting A2 should include it (it IS the most recent previous)
        let tar_a2_dir = TempDir::new().unwrap();
        let tar_a2 = create_test_tar(tar_a2_dir.path(), &[("photo.jpg", b"v2")]);
        let output_dir2 = dir.path().join("output2");
        let count = extract_archive(&tar_a2, &output_dir2, &manifest, "A2", &opts).unwrap();
        assert_eq!(count, 1);
        assert!(output_dir2.join("__versions/2026-03-01/photo.jpg").exists());
    }

    // --- path_at_time tests ---

    #[test]
    fn test_path_at_time_no_moves() {
        let file = FileEntry {
            path: "current/path.jpg".to_string(),
            size: 100,
            mtime: Utc::now(),
            fingerprint: "fp".to_string(),
            archive_id: "A1".to_string(),
            history: vec![],
        };
        let result = path_at_time(&file, Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
        assert_eq!(result, "current/path.jpg");
    }

    #[test]
    fn test_path_at_time_before_move() {
        let file = FileEntry {
            path: "new/path.jpg".to_string(),
            size: 100,
            mtime: Utc::now(),
            fingerprint: "fp".to_string(),
            archive_id: "A1".to_string(),
            history: vec![HistoryEvent::Moved {
                from: "old/path.jpg".to_string(),
                at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
            }],
        };
        // Before the move
        let result = path_at_time(&file, Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
        assert_eq!(result, "old/path.jpg");
    }

    #[test]
    fn test_path_at_time_after_move() {
        let file = FileEntry {
            path: "new/path.jpg".to_string(),
            size: 100,
            mtime: Utc::now(),
            fingerprint: "fp".to_string(),
            archive_id: "A1".to_string(),
            history: vec![HistoryEvent::Moved {
                from: "old/path.jpg".to_string(),
                at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
            }],
        };
        // After the move
        let result = path_at_time(&file, Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap());
        assert_eq!(result, "new/path.jpg");
    }

    #[test]
    fn test_path_at_time_multiple_moves() {
        let file = FileEntry {
            path: "c/path.jpg".to_string(),
            size: 100,
            mtime: Utc::now(),
            fingerprint: "fp".to_string(),
            archive_id: "A1".to_string(),
            history: vec![
                HistoryEvent::Moved {
                    from: "a/path.jpg".to_string(),
                    at: Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap(),
                },
                HistoryEvent::Moved {
                    from: "b/path.jpg".to_string(),
                    at: Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap(),
                },
            ],
        };
        // Before first move
        let result = path_at_time(&file, Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
        assert_eq!(result, "a/path.jpg");

        // Between moves
        let result = path_at_time(&file, Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap());
        assert_eq!(result, "b/path.jpg");

        // After all moves
        let result = path_at_time(&file, Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap());
        assert_eq!(result, "c/path.jpg");
    }
}
