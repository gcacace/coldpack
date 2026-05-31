use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};

use crate::manifest::{FileEntry, Manifest};

pub struct BrowseFilter {
    pub path_pattern: Option<String>,
    pub after: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
}

pub struct BrowseResult {
    pub entries: Vec<BrowseEntry>,
}

pub struct BrowseEntry {
    pub path: String,
    pub size: u64,
    pub mtime: DateTime<Utc>,
    pub archive_id: String,
}

pub fn parse_date_filter(date_str: &str) -> Result<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .with_context(|| format!("Invalid date '{}': expected YYYY-MM-DD", date_str))?;
    Ok(date.and_hms_opt(0, 0, 0).unwrap().and_utc())
}

pub fn browse(manifest: &Manifest, filter: &BrowseFilter) -> BrowseResult {
    let entries: Vec<BrowseEntry> = manifest
        .files
        .iter()
        .filter(|f| matches_filter(f, filter))
        .map(|f| BrowseEntry {
            path: f.path.clone(),
            size: f.size,
            mtime: f.mtime,
            archive_id: f.archive_id.clone(),
        })
        .collect();

    BrowseResult { entries }
}

fn matches_filter(file: &FileEntry, filter: &BrowseFilter) -> bool {
    // Path glob filter
    if let Some(pattern) = &filter.path_pattern {
        if !matches_glob(&file.path, pattern) {
            return false;
        }
    }

    // Date filters
    if let Some(after) = filter.after {
        if file.mtime <= after {
            return false;
        }
    }

    if let Some(before) = filter.before {
        if file.mtime >= before {
            return false;
        }
    }

    true
}

fn matches_glob(path: &str, pattern: &str) -> bool {
    crate::glob::matches(path, pattern)
}

pub fn format_size(bytes: u64) -> String {
    crate::util::format_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn make_manifest(files: Vec<FileEntry>) -> Manifest {
        Manifest {
            version: 1,
            last_backup: None,
            archives: vec![],
            files,
        }
    }

    fn make_file(path: &str, mtime: DateTime<Utc>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            size: 1000,
            mtime,
            fingerprint: "fp".to_string(),
            archive_id: "a1".to_string(),
            history: vec![],
        }
    }

    #[test]
    fn test_browse_no_filter() {
        let manifest = make_manifest(vec![
            make_file("marco/photo1.jpg", Utc::now()),
            make_file("laura/photo2.jpg", Utc::now()),
        ]);

        let filter = BrowseFilter {
            path_pattern: None,
            after: None,
            before: None,
        };

        let result = browse(&manifest, &filter);
        assert_eq!(result.entries.len(), 2);
    }

    #[test]
    fn test_browse_path_filter_exact() {
        let manifest = make_manifest(vec![
            make_file("marco/2026/05/photo.jpg", Utc::now()),
            make_file("laura/2026/05/photo.jpg", Utc::now()),
        ]);

        let filter = BrowseFilter {
            path_pattern: Some("marco/**".to_string()),
            after: None,
            before: None,
        };

        let result = browse(&manifest, &filter);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].path, "marco/2026/05/photo.jpg");
    }

    #[test]
    fn test_browse_path_filter_extension() {
        let manifest = make_manifest(vec![
            make_file("marco/photo.jpg", Utc::now()),
            make_file("marco/video.mp4", Utc::now()),
            make_file("marco/photo2.jpg", Utc::now()),
        ]);

        let filter = BrowseFilter {
            path_pattern: Some("**/*.jpg".to_string()),
            after: None,
            before: None,
        };

        let result = browse(&manifest, &filter);
        assert_eq!(result.entries.len(), 2);
    }

    #[test]
    fn test_browse_after_filter() {
        let manifest = make_manifest(vec![
            make_file(
                "old.jpg",
                Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            ),
            make_file(
                "new.jpg",
                Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap(),
            ),
        ]);

        let filter = BrowseFilter {
            path_pattern: None,
            after: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            before: None,
        };

        let result = browse(&manifest, &filter);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].path, "new.jpg");
    }

    #[test]
    fn test_browse_before_filter() {
        let manifest = make_manifest(vec![
            make_file(
                "old.jpg",
                Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            ),
            make_file(
                "new.jpg",
                Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap(),
            ),
        ]);

        let filter = BrowseFilter {
            path_pattern: None,
            after: None,
            before: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
        };

        let result = browse(&manifest, &filter);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].path, "old.jpg");
    }

    #[test]
    fn test_browse_combined_filters() {
        let manifest = make_manifest(vec![
            make_file(
                "marco/old.jpg",
                Utc.with_ymd_and_hms(2025, 3, 1, 0, 0, 0).unwrap(),
            ),
            make_file(
                "marco/mid.jpg",
                Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap(),
            ),
            make_file(
                "laura/mid.jpg",
                Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap(),
            ),
            make_file(
                "marco/new.jpg",
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            ),
        ]);

        let filter = BrowseFilter {
            path_pattern: Some("marco/**".to_string()),
            after: Some(Utc.with_ymd_and_hms(2025, 4, 1, 0, 0, 0).unwrap()),
            before: Some(Utc.with_ymd_and_hms(2025, 12, 1, 0, 0, 0).unwrap()),
        };

        let result = browse(&manifest, &filter);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].path, "marco/mid.jpg");
    }

    #[test]
    fn test_parse_date_filter_valid() {
        let dt = parse_date_filter("2026-05-15").unwrap();
        assert_eq!(dt, Utc.with_ymd_and_hms(2026, 5, 15, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_parse_date_filter_invalid() {
        let result = parse_date_filter("not-a-date");
        assert!(result.is_err());
    }

    #[test]
    fn test_glob_double_star() {
        assert!(matches_glob("marco/2026/05/photo.jpg", "**/*.jpg"));
        assert!(matches_glob("a/b/c/d.jpg", "**/*.jpg"));
        assert!(!matches_glob("photo.mp4", "**/*.jpg"));
    }

    #[test]
    fn test_glob_single_star() {
        assert!(matches_glob("marco/photo.jpg", "marco/*.jpg"));
        assert!(!matches_glob("marco/sub/photo.jpg", "marco/*.jpg"));
    }

    #[test]
    fn test_glob_prefix() {
        assert!(matches_glob("marco/2026/05/photo.jpg", "marco/**"));
        assert!(!matches_glob("laura/photo.jpg", "marco/**"));
    }

    #[test]
    fn test_glob_question_mark() {
        assert!(matches_glob("photo1.jpg", "photo?.jpg"));
        assert!(!matches_glob("photo12.jpg", "photo?.jpg"));
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1500), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }
}
