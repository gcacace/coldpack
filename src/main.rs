mod archiver;
mod backup;
mod browse;
mod config;
mod manifest;
mod restore;
mod scanner;
mod setup;
mod uploader;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "coldpack")]
#[command(about = "Glacier Deep Archive backup CLI for personal NAS disaster recovery")]
#[command(version)]
struct Cli {
    /// Profile name (default: "default"). Each profile has its own config and state.
    #[arg(long, global = true, default_value = "default")]
    profile: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Interactive setup wizard to create a new profile
    Setup,

    /// Run a backup of all configured source directories
    Backup {
        /// Show what would be backed up without uploading
        #[arg(long)]
        dry_run: bool,

        /// Override cutoff date filter (YYYY-MM-DD or "none")
        #[arg(long)]
        cutoff: Option<String>,
    },

    /// Browse backed-up files in the manifest
    Browse {
        /// Filter by path glob pattern
        #[arg(long)]
        path: Option<String>,

        /// Only show files modified after this date (YYYY-MM-DD)
        #[arg(long)]
        after: Option<String>,

        /// Only show files modified before this date (YYYY-MM-DD)
        #[arg(long)]
        before: Option<String>,
    },

    /// Request restoration of files from Glacier Deep Archive
    RestoreRequest {
        /// Restore all backed-up files
        #[arg(long, conflicts_with_all = ["path", "archive"])]
        all: bool,

        /// Restore files matching this path glob
        #[arg(long, conflicts_with_all = ["all", "archive"])]
        path: Option<String>,

        /// Restore a specific archive by ID
        #[arg(long, conflicts_with_all = ["all", "path"])]
        archive: Option<String>,
    },

    /// Download files that have been restored from Glacier
    RestoreDownload {
        /// Output directory for restored files
        #[arg(long, default_value = "./restored")]
        output: PathBuf,
    },

    /// Show backup status and pending operations
    Status,

    /// Clean up stale multipart uploads and checkpoint files
    Cleanup,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let profile_dir = config::profile_dir(&cli.profile);

    // Setup doesn't require an existing profile
    if matches!(cli.command, Commands::Setup) {
        return setup::run_setup(&cli.profile);
    }

    // All other commands require a valid profile
    let config_path = config::profile_config_path(&cli.profile);
    if !config_path.exists() {
        anyhow::bail!(
            "Profile '{}' not found. Run 'coldpack setup --profile {}' to create it.",
            cli.profile,
            cli.profile
        );
    }
    let cfg = config::load_config(&config_path)?;

    match cli.command {
        Commands::Setup => unreachable!(),
        Commands::Backup { dry_run, cutoff } => {
            let options = backup::BackupOptions {
                dry_run,
                cutoff_override: cutoff,
            };
            let report = backup::run_backup(&cfg, &profile_dir, &options).await?;

            let stats = &report.scan_stats;
            println!("Scan complete:");
            println!("  Files scanned: {}", stats.total_files_scanned);
            println!("  Skipped (cutoff): {}", stats.skipped_by_cutoff);
            println!("  Unchanged: {}", stats.unchanged);
            println!("  New: {}", stats.new);
            println!("  Modified: {}", stats.modified);
            println!("  Moved: {}", stats.moved);
            println!("  Deleted: {}", stats.deleted);

            if dry_run {
                println!("\n(dry run — no changes made)");
            } else if let Some(size) = report.archive_size {
                println!(
                    "\nArchive uploaded: {} files, {:.1} MB",
                    report.archive_file_count,
                    size as f64 / 1024.0 / 1024.0
                );
            } else if report.manifest_updated {
                println!("\nManifest updated (moves/deletes only, no new archive).");
            }
        }
        Commands::Browse {
            path,
            after,
            before,
        } => {
            let local_manifest_path = manifest::manifest_local_path(&profile_dir);
            let m = if local_manifest_path.exists() {
                manifest::load_from_file(&local_manifest_path)?
            } else {
                eprintln!("No local manifest found. Run 'coldpack backup' first.");
                return Ok(());
            };

            let filter = browse::BrowseFilter {
                path_pattern: path,
                after: after.map(|d| browse::parse_date_filter(&d)).transpose()?,
                before: before.map(|d| browse::parse_date_filter(&d)).transpose()?,
            };

            let result = browse::browse(&m, &filter);

            if result.entries.is_empty() {
                println!("No files match the given filters.");
            } else {
                println!("{:<60} {:>10} {:>12} {:>20}", "PATH", "SIZE", "MODIFIED", "ARCHIVE");
                println!("{}", "-".repeat(100));
                for entry in &result.entries {
                    println!(
                        "{:<60} {:>10} {:>12} {}",
                        entry.path,
                        browse::format_size(entry.size),
                        entry.mtime.format("%Y-%m-%d"),
                        entry.archive_id
                    );
                }
                println!("\n{} file(s) found.", result.entries.len());
            }
        }
        Commands::RestoreRequest {
            all,
            path,
            archive,
        } => {
            let local_manifest_path = manifest::manifest_local_path(&profile_dir);
            let m = if local_manifest_path.exists() {
                manifest::load_from_file(&local_manifest_path)?
            } else {
                anyhow::bail!("No local manifest found. Run 'coldpack backup' first.");
            };

            let archives_needed = restore::determine_archives_needed(
                &m,
                all,
                path.as_deref(),
                archive.as_deref(),
            )?;

            if archives_needed.is_empty() {
                println!("No archives match the given criteria.");
                return Ok(());
            }

            let request_type = if all {
                restore::RestoreRequestType::All
            } else if let Some(p) = path {
                restore::RestoreRequestType::Path(p)
            } else if let Some(a) = archive {
                restore::RestoreRequestType::Archive(a)
            } else {
                unreachable!()
            };

            let job = restore::create_restore_job(request_type, archives_needed);
            println!("Restore job created: {}", job.id);
            println!("  Archives to restore: {}", job.archives.len());
            println!("  Estimated availability: ~12 hours (Deep Archive standard retrieval)");
            println!("\nNote: In production, this would call S3 RestoreObject for each archive.");
            println!("Run 'coldpack restore-download' after archives become available.");

            restore::save_restore_job(&profile_dir, &job)?;
        }
        Commands::RestoreDownload { output } => {
            let jobs = restore::load_restore_jobs(&profile_dir)?;
            if jobs.is_empty() {
                println!("No pending restore jobs found.");
                return Ok(());
            }

            println!("Found {} restore job(s).", jobs.len());
            println!("Output directory: {}", output.display());

            for (path, job) in &jobs {
                println!("\nJob: {} ({} archives)", job.id, job.archives.len());
                for archive in &job.archives {
                    match &archive.status {
                        restore::RestoreStatus::Requested => {
                            println!("  {} — still pending (check back later)", archive.s3_key);
                        }
                        restore::RestoreStatus::Available => {
                            println!("  {} — available for download", archive.s3_key);
                        }
                        restore::RestoreStatus::Downloaded => {
                            println!("  {} — already downloaded", archive.s3_key);
                        }
                        restore::RestoreStatus::Failed(e) => {
                            println!("  {} — FAILED: {}", archive.s3_key, e);
                        }
                    }
                }
                let _ = path;
            }

            println!("\nNote: In production, this would check S3 restore status, download available archives, and extract files.");
        }
        Commands::Status => {
            let local_manifest_path = manifest::manifest_local_path(&profile_dir);

            if local_manifest_path.exists() {
                let m = manifest::load_from_file(&local_manifest_path)?;
                println!("Backup Status (profile: '{}'):", cli.profile);
                println!("  Last backup: {}", m.last_backup.map_or("never".to_string(), |t| t.to_rfc3339()));
                println!("  Total archives: {}", m.archives.len());
                println!("  Total files tracked: {}", m.files.len());

                let total_size: u64 = m.archives.iter().map(|a| a.size_bytes).sum();
                println!("  Total archive size: {}", browse::format_size(total_size));
            } else {
                println!("No backup data found. Run 'coldpack backup' to start.");
            }

            let jobs = restore::load_restore_jobs(&profile_dir)?;
            if !jobs.is_empty() {
                println!("\nPending Restores:");
                for (_, job) in &jobs {
                    let pending = job.archives.iter().filter(|a| matches!(a.status, restore::RestoreStatus::Requested)).count();
                    let available = job.archives.iter().filter(|a| matches!(a.status, restore::RestoreStatus::Available)).count();
                    println!("  {} — {} pending, {} available", job.id, pending, available);
                }
            }

            let cp_dir = uploader::checkpoint_dir(&profile_dir);
            if cp_dir.exists() {
                let count = std::fs::read_dir(&cp_dir)?.filter(|e| e.is_ok()).count();
                if count > 0 {
                    println!("\nStale Uploads: {} checkpoint file(s) found. Run 'coldpack cleanup' to resolve.", count);
                }
            }
        }
        Commands::Cleanup => {
            let cp_dir = uploader::checkpoint_dir(&profile_dir);
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
        }
    }

    Ok(())
}
