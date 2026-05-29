#![allow(dead_code)]

use anyhow::{Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use chrono::{Datelike, DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::Config;



#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub version: u32,
    pub last_backup: Option<DateTime<Utc>>,
    pub archives: Vec<Archive>,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Archive {
    pub id: String,
    pub s3_key: String,
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
    pub file_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub mtime: DateTime<Utc>,
    pub fingerprint: String,
    pub archive_id: String,
    #[serde(default)]
    pub history: Vec<HistoryEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event")]
pub enum HistoryEvent {
    #[serde(rename = "added")]
    Added {
        archive_id: String,
        mtime: DateTime<Utc>,
        size: u64,
    },
    #[serde(rename = "moved")]
    Moved { from: String, at: DateTime<Utc> },
    #[serde(rename = "deleted")]
    Deleted { at: DateTime<Utc> },
}

impl Manifest {
    pub fn new() -> Self {
        Self {
            version: 1,
            last_backup: None,
            archives: Vec::new(),
            files: Vec::new(),
        }
    }

    pub fn file_index(&self) -> HashMap<&str, &FileEntry> {
        self.files.iter().map(|f| (f.path.as_str(), f)).collect()
    }

    pub fn fingerprint_index(&self) -> HashMap<&str, &FileEntry> {
        self.files
            .iter()
            .map(|f| (f.fingerprint.as_str(), f))
            .collect()
    }
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new()
    }
}

pub fn resolve_cutoff(cutoff_str: &str) -> Result<Option<DateTime<Utc>>> {
    match cutoff_str {
        "none" => Ok(None),
        "start_of_current_month" => {
            let now = Utc::now();
            let first_of_month = now.date_naive().with_day(1).unwrap();
            let dt = first_of_month
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc();
            Ok(Some(dt))
        }
        date_str => {
            let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
                .with_context(|| format!("Invalid cutoff date: {}", date_str))?;
            let dt = date.and_hms_opt(0, 0, 0).unwrap().and_utc();
            Ok(Some(dt))
        }
    }
}

pub fn manifest_local_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("manifest.json")
}

pub fn load_from_file(path: &Path) -> Result<Manifest> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read manifest: {}", path.display()))?;
    let manifest: Manifest =
        serde_json::from_str(&content).with_context(|| "Failed to parse manifest JSON")?;
    Ok(manifest)
}

pub fn save_to_file(manifest: &Manifest, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(manifest)
        .with_context(|| "Failed to serialize manifest")?;
    std::fs::write(&tmp_path, &content)
        .with_context(|| format!("Failed to write manifest to: {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| "Failed to atomically replace manifest file")?;
    Ok(())
}

pub async fn load_or_create(config: &Config, profile_dir: &Path) -> Result<Manifest> {
    let local_path = manifest_local_path(profile_dir);

    if local_path.exists() {
        match load_from_file(&local_path) {
            Ok(m) => return Ok(m),
            Err(_) => {
                eprintln!("Local manifest corrupted, will download from S3...");
            }
        }
    }

    match download_from_s3(config).await {
        Ok(m) => {
            let _ = save_to_file(&m, &local_path);
            Ok(m)
        }
        Err(_) => {
            eprintln!("No existing manifest found, starting fresh.");
            Ok(Manifest::new())
        }
    }
}

pub async fn download_from_s3(config: &Config) -> Result<Manifest> {
    let client = crate::util::create_s3_client(config).await;
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

pub async fn save_to_s3(config: &Config, profile_dir: &Path, manifest: &Manifest) -> Result<()> {
    let local_path = manifest_local_path(profile_dir);
    save_to_file(manifest, &local_path)?;

    let client = crate::util::create_s3_client(config).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone};
    use tempfile::TempDir;

    #[test]
    fn test_manifest_new() {
        let m = Manifest::new();
        assert_eq!(m.version, 1);
        assert!(m.last_backup.is_none());
        assert!(m.archives.is_empty());
        assert!(m.files.is_empty());
    }

    #[test]
    fn test_manifest_serialization_roundtrip() {
        let manifest = Manifest {
            version: 1,
            last_backup: Some(Utc.with_ymd_and_hms(2026, 5, 27, 10, 0, 0).unwrap()),
            archives: vec![Archive {
                id: "backup-2026-05-27T10:00:00Z".to_string(),
                s3_key: "archives/backup-2026-05-27.tar".to_string(),
                size_bytes: 1_500_000_000,
                created_at: Utc.with_ymd_and_hms(2026, 5, 27, 10, 30, 0).unwrap(),
                file_count: 45,
            }],
            files: vec![FileEntry {
                path: "marco/2026/05/IMG_1234.jpg".to_string(),
                size: 5_242_880,
                mtime: Utc.with_ymd_and_hms(2026, 5, 15, 14, 30, 0).unwrap(),
                fingerprint: "a1b2c3d4e5f6".to_string(),
                archive_id: "backup-2026-05-27T10:00:00Z".to_string(),
                history: vec![
                    HistoryEvent::Added {
                        archive_id: "backup-2026-04-27T10:00:00Z".to_string(),
                        mtime: Utc.with_ymd_and_hms(2026, 4, 10, 9, 0, 0).unwrap(),
                        size: 5_200_000,
                    },
                    HistoryEvent::Moved {
                        from: "marco/2026/04/IMG_1234.jpg".to_string(),
                        at: Utc.with_ymd_and_hms(2026, 5, 27, 10, 0, 0).unwrap(),
                    },
                ],
            }],
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let deserialized: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, deserialized);
    }

    #[test]
    fn test_file_index() {
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![
                FileEntry {
                    path: "marco/photo.jpg".to_string(),
                    size: 100,
                    mtime: Utc::now(),
                    fingerprint: "fp1".to_string(),
                    archive_id: "a1".to_string(),
                    history: vec![],
                },
                FileEntry {
                    path: "laura/photo.jpg".to_string(),
                    size: 200,
                    mtime: Utc::now(),
                    fingerprint: "fp2".to_string(),
                    archive_id: "a1".to_string(),
                    history: vec![],
                },
            ],
        };

        let index = manifest.file_index();
        assert_eq!(index.len(), 2);
        assert_eq!(index["marco/photo.jpg"].size, 100);
        assert_eq!(index["laura/photo.jpg"].size, 200);
    }

    #[test]
    fn test_fingerprint_index() {
        let manifest = Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files: vec![FileEntry {
                path: "marco/photo.jpg".to_string(),
                size: 100,
                mtime: Utc::now(),
                fingerprint: "abc123".to_string(),
                archive_id: "a1".to_string(),
                history: vec![],
            }],
        };

        let index = manifest.fingerprint_index();
        assert!(index.contains_key("abc123"));
        assert_eq!(index["abc123"].path, "marco/photo.jpg");
    }

    #[test]
    fn test_resolve_cutoff_none() {
        let result = resolve_cutoff("none").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_cutoff_start_of_current_month() {
        let result = resolve_cutoff("start_of_current_month").unwrap().unwrap();
        let now = Utc::now();
        assert_eq!(result.date_naive().day(), 1);
        assert_eq!(result.date_naive().month(), now.date_naive().month());
        assert_eq!(result.date_naive().year(), now.date_naive().year());
        assert_eq!(result.time(), chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    }

    #[test]
    fn test_resolve_cutoff_explicit_date() {
        let result = resolve_cutoff("2026-05-01").unwrap().unwrap();
        assert_eq!(result, Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_resolve_cutoff_invalid() {
        let result = resolve_cutoff("not-a-date");
        assert!(result.is_err());
    }

    #[test]
    fn test_save_and_load_manifest() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("manifest.json");

        let manifest = Manifest {
            version: 1,
            last_backup: Some(Utc.with_ymd_and_hms(2026, 5, 27, 10, 0, 0).unwrap()),
            archives: vec![],
            files: vec![FileEntry {
                path: "test/file.jpg".to_string(),
                size: 1024,
                mtime: Utc.with_ymd_and_hms(2026, 5, 15, 0, 0, 0).unwrap(),
                fingerprint: "fp123".to_string(),
                archive_id: "backup-1".to_string(),
                history: vec![],
            }],
        };

        save_to_file(&manifest, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();
        assert_eq!(manifest, loaded);
    }

    #[test]
    fn test_save_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("dir").join("manifest.json");

        let manifest = Manifest::new();
        save_to_file(&manifest, &path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_load_nonexistent_file() {
        let result = load_from_file(Path::new("/nonexistent/manifest.json"));
        assert!(result.is_err());
    }

    #[test]
    fn test_load_corrupted_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("manifest.json");
        std::fs::write(&path, "not valid json{{{").unwrap();

        let result = load_from_file(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parse"));
    }

    #[test]
    fn test_history_event_serialization() {
        let events = vec![
            HistoryEvent::Added {
                archive_id: "a1".to_string(),
                mtime: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                size: 500,
            },
            HistoryEvent::Moved {
                from: "old/path.jpg".to_string(),
                at: Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap(),
            },
            HistoryEvent::Deleted {
                at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
            },
        ];

        let json = serde_json::to_string(&events).unwrap();
        let deserialized: Vec<HistoryEvent> = serde_json::from_str(&json).unwrap();
        assert_eq!(events, deserialized);

        // Verify tagged format
        assert!(json.contains("\"event\":\"added\""));
        assert!(json.contains("\"event\":\"moved\""));
        assert!(json.contains("\"event\":\"deleted\""));
    }
}
