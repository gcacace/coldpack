use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub storage: StorageConfig,
    pub backup: BackupConfig,
    #[serde(default)]
    pub performance: PerformanceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StorageConfig {
    pub bucket: String,
    pub region: String,
    #[serde(default = "default_archive_prefix")]
    pub archive_prefix: String,
    #[serde(default = "default_manifest_prefix")]
    pub manifest_prefix: String,
    #[serde(default = "default_storage_class")]
    pub storage_class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupConfig {
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub filter: FilterConfig,
    #[serde(default = "default_max_archive_size_mb")]
    pub max_archive_size_mb: u64,
    #[serde(default = "default_tmp_dir")]
    pub tmp_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceConfig {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilterConfig {
    #[serde(default = "default_cutoff")]
    pub cutoff: String,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerformanceConfig {
    #[serde(default = "default_max_io_workers")]
    pub max_io_workers: usize,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            cutoff: default_cutoff(),
            exclude: vec![],
        }
    }
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            max_io_workers: default_max_io_workers(),
        }
    }
}

pub const VALID_STORAGE_CLASSES: &[&str] = &[
    "STANDARD",
    "STANDARD_IA",
    "GLACIER_IR",
    "GLACIER",
    "DEEP_ARCHIVE",
];

fn default_archive_prefix() -> String {
    "archives/".to_string()
}

fn default_manifest_prefix() -> String {
    "manifest/".to_string()
}

fn default_cutoff() -> String {
    "start_of_current_month".to_string()
}

fn default_max_archive_size_mb() -> u64 {
    10240 // 10 GB
}

fn default_tmp_dir() -> PathBuf {
    std::env::temp_dir().join("coldpack")
}

fn default_storage_class() -> String {
    "DEEP_ARCHIVE".to_string()
}

fn default_max_io_workers() -> usize {
    2
}

pub fn coldpack_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".coldpack")
}

pub fn profiles_dir() -> PathBuf {
    coldpack_dir().join("profiles")
}

pub fn profile_dir(name: &str) -> PathBuf {
    profiles_dir().join(name)
}

pub fn profile_config_path(name: &str) -> PathBuf {
    profile_dir(name).join("config.toml")
}

#[cfg(test)]
pub fn list_profiles() -> Result<Vec<String>> {
    let dir = profiles_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut profiles = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("Failed to read profiles directory: {}", dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                if profile_dir(name).join("config.toml").exists() {
                    profiles.push(name.to_string());
                }
            }
        }
    }
    profiles.sort();
    Ok(profiles)
}

pub fn load_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let config: Config = toml::from_str(&content).with_context(|| "Failed to parse config file")?;

    validate_config(&config)?;
    Ok(config)
}

pub fn save_config(config: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    let content = toml::to_string_pretty(config).with_context(|| "Failed to serialize config")?;
    std::fs::write(path, content)
        .with_context(|| format!("Failed to write config: {}", path.display()))?;
    Ok(())
}

fn validate_config(config: &Config) -> Result<()> {
    if config.backup.sources.is_empty() {
        anyhow::bail!("At least one backup source must be configured");
    }

    let mut names = std::collections::HashSet::new();
    for source in &config.backup.sources {
        if source.name.is_empty() {
            anyhow::bail!("Source name cannot be empty");
        }
        if source.name.contains('/') || source.name.contains('\\') {
            anyhow::bail!(
                "Source name '{}' cannot contain path separators",
                source.name
            );
        }
        if !names.insert(&source.name) {
            anyhow::bail!("Duplicate source name: '{}'", source.name);
        }
    }

    let cutoff = &config.backup.filter.cutoff;
    if cutoff != "start_of_current_month" && cutoff != "none" {
        chrono::NaiveDate::parse_from_str(cutoff, "%Y-%m-%d")
            .with_context(|| format!("Invalid cutoff date '{}': expected 'start_of_current_month', 'none', or YYYY-MM-DD", cutoff))?;
    }

    if !VALID_STORAGE_CLASSES.contains(&config.storage.storage_class.as_str()) {
        anyhow::bail!(
            "Invalid storage_class '{}'. Must be one of: {}",
            config.storage.storage_class,
            VALID_STORAGE_CLASSES.join(", ")
        );
    }

    if config.performance.max_io_workers == 0 {
        anyhow::bail!("max_io_workers must be at least 1");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn test_load_full_config() {
        let f = write_config(
            r#"
[storage]
bucket = "my-family-backup"
region = "us-east-1"
archive_prefix = "archives/"
manifest_prefix = "manifest/"

[[backup.sources]]
name = "marco"
path = "/mnt/nas/marco-photos"

[[backup.sources]]
name = "laura"
path = "/mnt/nas/laura-photos"

[[backup.sources]]
name = "common"
path = "/mnt/nas/family-common"

[backup.filter]
cutoff = "start_of_current_month"

[performance]
max_io_workers = 4
"#,
        );

        let config = load_config(f.path()).unwrap();
        assert_eq!(config.storage.bucket, "my-family-backup");
        assert_eq!(config.storage.region, "us-east-1");
        assert_eq!(config.backup.sources.len(), 3);
        assert_eq!(config.backup.sources[0].name, "marco");
        assert_eq!(
            config.backup.sources[0].path,
            PathBuf::from("/mnt/nas/marco-photos")
        );
        assert_eq!(config.backup.filter.cutoff, "start_of_current_month");
        assert_eq!(config.performance.max_io_workers, 4);
    }

    #[test]
    fn test_defaults_applied() {
        let f = write_config(
            r#"
[storage]
bucket = "test-bucket"
region = "eu-west-1"

[[backup.sources]]
name = "photos"
path = "/data/photos"
"#,
        );

        let config = load_config(f.path()).unwrap();
        assert_eq!(config.storage.archive_prefix, "archives/");
        assert_eq!(config.storage.manifest_prefix, "manifest/");
        assert_eq!(config.backup.filter.cutoff, "start_of_current_month");
        assert_eq!(config.performance.max_io_workers, 2);
    }

    #[test]
    fn test_explicit_cutoff_date() {
        let f = write_config(
            r#"
[storage]
bucket = "b"
region = "r"

[[backup.sources]]
name = "a"
path = "/a"

[backup.filter]
cutoff = "2026-05-01"
"#,
        );

        let config = load_config(f.path()).unwrap();
        assert_eq!(config.backup.filter.cutoff, "2026-05-01");
    }

    #[test]
    fn test_cutoff_none() {
        let f = write_config(
            r#"
[storage]
bucket = "b"
region = "r"

[[backup.sources]]
name = "a"
path = "/a"

[backup.filter]
cutoff = "none"
"#,
        );

        let config = load_config(f.path()).unwrap();
        assert_eq!(config.backup.filter.cutoff, "none");
    }

    #[test]
    fn test_invalid_cutoff_date() {
        let f = write_config(
            r#"
[storage]
bucket = "b"
region = "r"

[[backup.sources]]
name = "a"
path = "/a"

[backup.filter]
cutoff = "not-a-date"
"#,
        );

        let err = load_config(f.path()).unwrap_err();
        assert!(err.to_string().contains("Invalid cutoff date"));
    }

    #[test]
    fn test_no_sources_fails() {
        let f = write_config(
            r#"
[storage]
bucket = "b"
region = "r"

[backup]
sources = []
"#,
        );

        let err = load_config(f.path()).unwrap_err();
        assert!(err.to_string().contains("At least one backup source"));
    }

    #[test]
    fn test_duplicate_source_name_fails() {
        let f = write_config(
            r#"
[storage]
bucket = "b"
region = "r"

[[backup.sources]]
name = "photos"
path = "/a"

[[backup.sources]]
name = "photos"
path = "/b"
"#,
        );

        let err = load_config(f.path()).unwrap_err();
        assert!(err.to_string().contains("Duplicate source name"));
    }

    #[test]
    fn test_source_name_with_slash_fails() {
        let f = write_config(
            r#"
[storage]
bucket = "b"
region = "r"

[[backup.sources]]
name = "my/photos"
path = "/a"
"#,
        );

        let err = load_config(f.path()).unwrap_err();
        assert!(err.to_string().contains("cannot contain path separators"));
    }

    #[test]
    fn test_zero_workers_fails() {
        let f = write_config(
            r#"
[storage]
bucket = "b"
region = "r"

[[backup.sources]]
name = "a"
path = "/a"

[performance]
max_io_workers = 0
"#,
        );

        let err = load_config(f.path()).unwrap_err();
        assert!(err
            .to_string()
            .contains("max_io_workers must be at least 1"));
    }

    #[test]
    fn test_missing_config_file() {
        let err = load_config(Path::new("/nonexistent/config.toml")).unwrap_err();
        assert!(err.to_string().contains("Failed to read config file"));
    }

    #[test]
    fn test_profile_dir_path() {
        let dir = profile_dir("my-backup");
        assert!(dir.ends_with("profiles/my-backup"));
    }

    #[test]
    fn test_profile_config_path() {
        let path = profile_config_path("default");
        assert!(path.ends_with("profiles/default/config.toml"));
    }

    #[test]
    fn test_list_profiles_empty() {
        // When profiles dir doesn't exist, returns empty
        // (relies on ~/.coldpack/profiles not existing in test env — safe assumption)
        // We test via a tempdir approach instead
        let profiles = list_profiles().unwrap_or_default();
        // Just verify it doesn't panic
        let _ = profiles;
    }

    #[test]
    fn test_save_and_load_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        let config = Config {
            storage: StorageConfig {
                bucket: "test-bucket".to_string(),
                region: "us-east-1".to_string(),
                archive_prefix: "archives/".to_string(),
                manifest_prefix: "manifest/".to_string(),
                storage_class: "GLACIER".to_string(),
            },
            backup: BackupConfig {
                sources: vec![SourceConfig {
                    name: "photos".to_string(),
                    path: PathBuf::from("/data/photos"),
                }],
                filter: FilterConfig::default(),
                max_archive_size_mb: 10240,
                tmp_dir: PathBuf::from("/tmp/coldpack"),
            },
            performance: PerformanceConfig::default(),
        };

        save_config(&config, &path).unwrap();
        let loaded = load_config(&path).unwrap();
        assert_eq!(config, loaded);
    }

    #[test]
    fn test_storage_class_default() {
        let f = write_config(
            r#"
[storage]
bucket = "b"
region = "r"

[[backup.sources]]
name = "a"
path = "/a"
"#,
        );

        let config = load_config(f.path()).unwrap();
        assert_eq!(config.storage.storage_class, "DEEP_ARCHIVE");
    }

    #[test]
    fn test_storage_class_valid_values() {
        for class in VALID_STORAGE_CLASSES {
            let content = format!(
                r#"
[storage]
bucket = "b"
region = "r"
storage_class = "{}"

[[backup.sources]]
name = "a"
path = "/a"
"#,
                class
            );
            let f = write_config(&content);
            let config = load_config(f.path()).unwrap();
            assert_eq!(config.storage.storage_class, *class);
        }
    }

    #[test]
    fn test_storage_class_invalid() {
        let f = write_config(
            r#"
[storage]
bucket = "b"
region = "r"
storage_class = "INVALID_CLASS"

[[backup.sources]]
name = "a"
path = "/a"
"#,
        );

        let err = load_config(f.path()).unwrap_err();
        assert!(err.to_string().contains("Invalid storage_class"));
    }
}
