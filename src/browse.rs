#![allow(dead_code)]

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
    glob_recursive(path, pattern)
}

fn glob_recursive(path: &str, pattern: &str) -> bool {
    // Split pattern on ** to handle double-star segments
    if let Some(idx) = pattern.find("**") {
        let prefix = &pattern[..idx];
        let suffix = &pattern[idx + 2..];
        // Remove leading / from suffix
        let suffix = suffix.strip_prefix('/').unwrap_or(suffix);

        // prefix must match the start of path (using single-star glob)
        if !prefix.is_empty() {
            // prefix should match a path prefix
            // Try matching prefix against all possible path prefixes
            let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
            if !path.starts_with(prefix) && !glob_simple_prefix(path, prefix) {
                return false;
            }
        }

        if suffix.is_empty() {
            return true;
        }

        // ** can match zero or more path segments
        // Try matching suffix against every possible tail of path
        for i in 0..=path.len() {
            if (i == 0 || i == path.len() || path.as_bytes()[i - 1] == b'/' || prefix.is_empty())
                && glob_simple(&path[i..], suffix)
            {
                return true;
            }
        }
        false
    } else {
        glob_simple(path, pattern)
    }
}

fn glob_simple_prefix(path: &str, pattern: &str) -> bool {
    // Check if pattern matches the beginning of path up to a /
    for i in 0..=path.len() {
        if (i == path.len() || path.as_bytes()[i] == b'/') && glob_simple(&path[..i], pattern) {
            return true;
        }
    }
    false
}

fn glob_simple(text: &str, pattern: &str) -> bool {
    // Simple glob: * matches anything except /, ? matches one char except /
    let t = text.as_bytes();
    let p = pattern.as_bytes();
    let mut ti = 0;
    let mut pi = 0;
    let mut star_ti: Option<usize> = None;
    let mut star_pi: Option<usize> = None;

    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star_ti = Some(ti);
            star_pi = Some(pi + 1);
            pi += 1;
        } else if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) && t[ti] != b'/' {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] && t[ti] == b'/' {
            ti += 1;
            pi += 1;
            // Reset star tracking at path separator for single *
            star_ti = None;
            star_pi = None;
        } else if let (Some(sti), Some(spi)) = (star_ti, star_pi) {
            if t[sti] == b'/' {
                // Single * cannot cross /
                return false;
            }
            let new_sti = sti + 1;
            star_ti = Some(new_sti);
            ti = new_sti;
            pi = spi;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
    }
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
            make_file("old.jpg", Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap()),
            make_file("new.jpg", Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()),
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
            make_file("old.jpg", Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap()),
            make_file("new.jpg", Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()),
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
            make_file("marco/old.jpg", Utc.with_ymd_and_hms(2025, 3, 1, 0, 0, 0).unwrap()),
            make_file("marco/mid.jpg", Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap()),
            make_file("laura/mid.jpg", Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap()),
            make_file("marco/new.jpg", Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
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
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.00 GB");
    }
}
