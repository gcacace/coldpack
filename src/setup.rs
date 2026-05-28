use anyhow::Result;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::config::{
    self, BackupConfig, Config, FilterConfig, PerformanceConfig, SourceConfig, StorageConfig,
};

pub fn run_setup(profile_name: &str) -> Result<()> {
    println!("Welcome to coldpack setup!\n");
    println!("Profile: {}\n", profile_name);

    let config_path = config::profile_config_path(profile_name);
    if config_path.exists() {
        let overwrite = prompt_yes_no(&format!(
            "Profile '{}' already exists. Overwrite?",
            profile_name
        ), false)?;
        if !overwrite {
            println!("Setup cancelled.");
            return Ok(());
        }
    }

    // === AWS Storage ===
    println!("=== AWS Storage ===");
    let bucket = prompt_required("S3 bucket name")?;
    let region = prompt_with_default("AWS region", "us-east-1")?;
    let archive_prefix = prompt_with_default("Archive prefix", "archives/")?;
    let manifest_prefix = prompt_with_default("Manifest prefix", "manifest/")?;

    // === Backup Sources ===
    println!("\n=== Backup Sources ===");
    let mut sources = Vec::new();
    loop {
        println!("Add a source directory.");
        let name = prompt_required("  Label (e.g., \"marco\", \"laura\", \"common\")")?;

        if sources.iter().any(|s: &SourceConfig| s.name == name) {
            eprintln!("  Warning: label '{}' already used. Please choose a different one.", name);
            continue;
        }
        if name.contains('/') || name.contains('\\') {
            eprintln!("  Warning: label cannot contain path separators.");
            continue;
        }

        let path_str = prompt_required("  Path")?;
        let path = PathBuf::from(&path_str);
        if !path.exists() {
            eprintln!("  Warning: path '{}' does not exist (yet). Continuing anyway.", path_str);
        }

        sources.push(SourceConfig { name, path });

        if !prompt_yes_no("Add another source?", false)? {
            break;
        }
    }

    if sources.is_empty() {
        anyhow::bail!("At least one source directory is required.");
    }

    // === Backup Filter ===
    println!("\n=== Backup Filter ===");
    println!("Cutoff strategy:");
    println!("  1. start_of_current_month (recommended — ignores files from current month)");
    println!("  2. Specific date (YYYY-MM-DD)");
    println!("  3. none (backup everything)");
    let cutoff_choice = prompt_with_default("Choice", "1")?;
    let cutoff = match cutoff_choice.as_str() {
        "1" | "" => "start_of_current_month".to_string(),
        "2" => {
            let date = prompt_required("  Enter date (YYYY-MM-DD)")?;
            chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .map_err(|_| anyhow::anyhow!("Invalid date format"))?;
            date
        }
        "3" => "none".to_string(),
        _ => {
            eprintln!("  Invalid choice, using default (start_of_current_month).");
            "start_of_current_month".to_string()
        }
    };

    // === Performance ===
    println!("\n=== Performance ===");
    let workers_str = prompt_with_default("Max I/O workers for fingerprinting", "2")?;
    let max_io_workers: usize = workers_str.parse().unwrap_or(2);
    if max_io_workers == 0 {
        anyhow::bail!("max_io_workers must be at least 1");
    }

    // Build config
    let cfg = Config {
        storage: StorageConfig {
            bucket,
            region,
            archive_prefix,
            manifest_prefix,
        },
        backup: BackupConfig {
            sources,
            filter: FilterConfig { cutoff },
        },
        performance: PerformanceConfig { max_io_workers },
    };

    // Save
    config::save_config(&cfg, &config_path)?;

    println!("\nConfiguration saved to: {}", config_path.display());
    if profile_name == "default" {
        println!("Run 'coldpack backup --dry-run' to verify your setup.");
    } else {
        println!(
            "Run 'coldpack --profile {} backup --dry-run' to verify your setup.",
            profile_name
        );
    }

    Ok(())
}

fn prompt_required(label: &str) -> Result<String> {
    loop {
        print!("{}: ", label);
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
        eprintln!("  This field is required.");
    }
}

fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    print!("{} [{}]: ", label, default);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_string();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed)
    }
}

fn prompt_yes_no(label: &str, default: bool) -> Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{} {}: ", label, hint);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_lowercase();
    Ok(match trimmed.as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        "" => default,
        _ => default,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_config_roundtrip_via_save() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        let cfg = Config {
            storage: StorageConfig {
                bucket: "my-bucket".to_string(),
                region: "eu-west-1".to_string(),
                archive_prefix: "archives/".to_string(),
                manifest_prefix: "manifest/".to_string(),
            },
            backup: BackupConfig {
                sources: vec![
                    SourceConfig {
                        name: "marco".to_string(),
                        path: PathBuf::from("/mnt/nas/marco"),
                    },
                    SourceConfig {
                        name: "laura".to_string(),
                        path: PathBuf::from("/mnt/nas/laura"),
                    },
                ],
                filter: FilterConfig {
                    cutoff: "start_of_current_month".to_string(),
                },
            },
            performance: PerformanceConfig { max_io_workers: 2 },
        };

        config::save_config(&cfg, &path).unwrap();
        let loaded = config::load_config(&path).unwrap();
        assert_eq!(cfg, loaded);
    }
}
