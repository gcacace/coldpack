use aws_sdk_s3::types::StorageClass;

pub fn parse_storage_class(s: &str) -> StorageClass {
    match s {
        "STANDARD" => StorageClass::Standard,
        "STANDARD_IA" => StorageClass::StandardIa,
        "GLACIER_IR" => StorageClass::GlacierIr,
        "GLACIER" => StorageClass::Glacier,
        _ => StorageClass::DeepArchive,
    }
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
}
