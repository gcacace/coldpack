# coldpack

Glacier Deep Archive backup CLI for personal NAS disaster recovery.

Backs up family photos and videos from multiple NAS folders to Amazon S3 Glacier Deep Archive with:
- One zip per monthly backup run (minimal object count, minimal cost)
- Move/rename detection (reorganized files are not re-uploaded)
- Full file-level manifest for searching without downloading
- Resumable multipart uploads with checkpointing
- Two-step restore workflow for Glacier's 12-hour retrieval delay

## Cost Estimate

For ~500 GB of photos/videos:
- Storage: ~$0.50/month (Deep Archive at $0.00099/GB/month)
- Monthly upload: ~$0.05 (PUT requests + data transfer for 1-2.5 GB)
- Manifest: negligible (few MB in S3 Standard)
- Full restore: ~$10 + $0.02/GB retrieval (one-time disaster recovery)

## Getting Started

### 1. Prerequisites

- Rust toolchain (for building from source)
- AWS account with an S3 bucket
- IAM user/role with appropriate permissions

### 2. Build

```bash
# On your development machine:
cargo build --release

# The binary will be at target/release/coldpack
# Copy it to your Synology NAS:
scp target/release/coldpack your-nas:/usr/local/bin/
```

For Synology NAS cross-compilation (if your NAS uses a different architecture):
```bash
# For x86_64 Synology (most modern models):
cargo build --release --target x86_64-unknown-linux-gnu

# For ARM-based Synology (older/smaller models):
cargo build --release --target aarch64-unknown-linux-gnu
```

### 3. AWS Setup

Create a dedicated IAM user with minimal permissions:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:PutObject",
        "s3:GetObject",
        "s3:HeadObject",
        "s3:ListMultipartUploadParts",
        "s3:AbortMultipartUpload",
        "s3:ListBucketMultipartUploads",
        "s3:CreateMultipartUpload",
        "s3:CompleteMultipartUpload",
        "s3:RestoreObject"
      ],
      "Resource": [
        "arn:aws:s3:::YOUR-BUCKET-NAME",
        "arn:aws:s3:::YOUR-BUCKET-NAME/*"
      ]
    }
  ]
}
```

Configure credentials on the NAS:
```bash
mkdir -p ~/.aws
cat > ~/.aws/credentials << 'EOF'
[default]
aws_access_key_id = YOUR_ACCESS_KEY
aws_secret_access_key = YOUR_SECRET_KEY
EOF
chmod 600 ~/.aws/credentials
```

### 4. Setup (Interactive)

Run the setup wizard to create a profile:
```bash
coldpack setup
```

The wizard will guide you through:
- S3 bucket name and AWS region
- Source directories (with labels like "marco", "laura", "common")
- Cutoff strategy (defaults to ignoring current month's files)
- Performance settings

This creates a profile at `~/.coldpack/profiles/default/config.toml`.

To create additional profiles (e.g., for a different backup set):
```bash
coldpack setup --profile work-laptop
```

### 5. First Run (Dry Run)

Verify everything is detected correctly without uploading:
```bash
coldpack backup --dry-run
```

This will show you what would be backed up:
```
Scan complete:
  Files scanned: 12345
  Skipped (cutoff): 47
  Unchanged: 0
  New: 12298
  Modified: 0
  Moved: 0
  Deleted: 0

(dry run - no changes made)
```

### 6. First Backup

```bash
coldpack backup
```

### 7. Schedule Monthly Backups (Synology Task Scheduler)

In **DSM > Control Panel > Task Scheduler**, create a new **User-defined script**:

- **Task**: coldpack monthly backup
- **User**: root (or a user with read access to photo folders)
- **Schedule**: 2nd of every month, 3:00 AM
- **Command**:

```bash
nice -n 19 ionice -c 3 /usr/local/bin/coldpack backup >> /var/log/coldpack.log 2>&1
```

The `nice`/`ionice` flags ensure coldpack runs at lowest priority, so it doesn't interfere with Home Assistant or other NAS services.

## Profiles

All commands support `--profile <name>` to target a specific profile. If omitted, the `default` profile is used.

```bash
# Use default profile
coldpack backup --dry-run

# Use a named profile
coldpack --profile work-laptop backup --dry-run
coldpack --profile work-laptop status
```

Each profile is fully isolated — its own config, manifest cache, upload checkpoints, and restore state.

## Commands

### `coldpack setup [--profile <name>]`

Interactive wizard to create or overwrite a profile configuration.

### `coldpack backup [--dry-run] [--cutoff <date|"none">]`

Scan all configured sources, detect changes, create a zip of new/modified files, upload to Glacier Deep Archive, and update the manifest.

- `--dry-run`: Show what would be backed up without uploading
- `--cutoff 2026-05-01`: Override the cutoff date (only backup files older than this)
- `--cutoff none`: Backup everything regardless of file age

### `coldpack browse [--path <glob>] [--after <date>] [--before <date>]`

Search the manifest for backed-up files.

```bash
# All files
coldpack browse

# Only Marco's photos from 2026
coldpack browse --path "marco/2026/**"

# All videos
coldpack browse --path "**/*.mp4"

# Files modified in May 2026
coldpack browse --after 2026-05-01 --before 2026-06-01
```

### `coldpack restore-request [--all | --path <glob> | --archive <id>]`

Initiate Glacier Deep Archive restoration (takes ~12 hours):

```bash
# Restore everything
coldpack restore-request --all

# Restore specific files
coldpack restore-request --path "marco/2026/05/**"

# Restore a specific archive
coldpack restore-request --archive "backup-2026-05-02T03:00:00Z"
```

### `coldpack restore-download [--output <dir>]`

Download files that have been restored from Glacier:

```bash
coldpack restore-download --output /volume1/restored
```

Run this ~12 hours after `restore-request`. For a full restore (`--all`), the latest version of each file is placed at its canonical path, older versions go to `__versions/`.

### `coldpack status`

Show backup summary and pending operations.

### `coldpack cleanup`

Remove stale multipart upload checkpoints and abort orphaned uploads.

## How It Works

### Change Detection

On each backup run, coldpack compares the current state of your NAS folders against the manifest:

| State | Condition | Action |
|-------|-----------|--------|
| Unchanged | Same path, same mtime + size | Skip |
| Modified | Same path, different mtime or size | Include in zip |
| Moved | New path, same fingerprint (xxHash of first 64KB + file size) | Update manifest only |
| New | New path, no fingerprint match | Include in zip |
| Deleted | Path in manifest gone, no move match | Mark deleted in manifest |

### Move Detection

When you move a photo between folders (e.g., from your personal folder to the family shared folder), coldpack recognizes it by fingerprint and updates the manifest without re-uploading. This saves bandwidth and storage costs.

### Cutoff Filter

By default, coldpack ignores files modified in the current month. This gives your NAS daily-sort job time to move photos to the right `year/month/` folder before they're backed up. Running on the 2nd of each month means you back up everything through the last day of the previous month.

### Resumable Uploads

If a backup is interrupted (power loss, network issue), the next run automatically resumes from where it left off. Upload progress is checkpointed locally after each 100 MB part.

## File Layout

```
~/.coldpack/
  profiles/
    default/                     # Default profile
      config.toml                # Profile configuration
      manifest.json              # Local manifest cache
      uploads/                   # Upload checkpoints (temporary)
      restores/                  # Restore job state
    work-laptop/                 # Another profile (fully isolated)
      config.toml
      manifest.json
      uploads/
      restores/

s3://<bucket>/
  manifest/manifest.json         # Authoritative manifest (S3 Standard)
  archives/backup-<date>.zip     # Monthly backup archives (Deep Archive)
```
