use aws_sdk_s3::types::StorageClass;
use aws_sdk_s3::Client;

use crate::config::Config;

pub fn parse_storage_class(s: &str) -> StorageClass {
    match s {
        "STANDARD" => StorageClass::Standard,
        "STANDARD_IA" => StorageClass::StandardIa,
        "GLACIER_IR" => StorageClass::GlacierIr,
        "GLACIER" => StorageClass::Glacier,
        _ => StorageClass::DeepArchive,
    }
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

pub async fn create_s3_client(config: &Config) -> Client {
    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(config.storage.region.clone()))
        .load()
        .await;
    Client::new(&aws_config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_storage_class_all_values() {
        assert_eq!(parse_storage_class("STANDARD"), StorageClass::Standard);
        assert_eq!(parse_storage_class("STANDARD_IA"), StorageClass::StandardIa);
        assert_eq!(parse_storage_class("GLACIER_IR"), StorageClass::GlacierIr);
        assert_eq!(parse_storage_class("GLACIER"), StorageClass::Glacier);
        assert_eq!(parse_storage_class("DEEP_ARCHIVE"), StorageClass::DeepArchive);
    }

    #[test]
    fn test_parse_storage_class_unknown_defaults_to_deep_archive() {
        assert_eq!(parse_storage_class("INVALID"), StorageClass::DeepArchive);
        assert_eq!(parse_storage_class(""), StorageClass::DeepArchive);
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1048576), "1.0 MB");
        assert_eq!(format_bytes(1073741824), "1.0 GB");
        assert_eq!(format_bytes(1610612736), "1.5 GB");
    }
}
