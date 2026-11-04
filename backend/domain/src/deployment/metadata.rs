//! Shared parsing and validation for owner-specific deployment metadata patches.

use std::collections::BTreeSet;

use dotcore_proto::google::protobuf::FieldMask;

use crate::deployment::name::DeploymentNameError;

const DISPLAY_NAME_PATH: &str = "display_name";

/// A fully validated metadata change that an owning controller can apply atomically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeploymentMetadataPatch {
    display_name: String,
}

impl DeploymentMetadataPatch {
    /// Parses the explicit writable metadata fields supplied by one owner adapter.
    pub fn parse(
        metadata_display_name: Option<Option<&str>>,
        update_mask: Option<&FieldMask>,
    ) -> Result<Self, DeploymentMetadataPatchError> {
        let metadata_display_name =
            metadata_display_name.ok_or(DeploymentMetadataPatchError::MissingMetadata)?;
        let update_mask = update_mask.ok_or(DeploymentMetadataPatchError::MissingUpdateMask)?;
        let mut paths = BTreeSet::new();

        if update_mask.paths.is_empty() {
            return Err(DeploymentMetadataPatchError::EmptyUpdateMask);
        }

        for path in &update_mask.paths {
            if !paths.insert(path.as_str()) {
                return Err(DeploymentMetadataPatchError::DuplicatePath(path.clone()));
            }
            if path != DISPLAY_NAME_PATH {
                return Err(DeploymentMetadataPatchError::UnwritablePath(path.clone()));
            }
        }

        let display_name = metadata_display_name.ok_or(
            DeploymentMetadataPatchError::MissingMaskedValue(DISPLAY_NAME_PATH),
        )?;
        let display_name = crate::deployment::name::normalize(display_name)
            .map_err(DeploymentMetadataPatchError::InvalidDisplayName)?;

        Ok(Self { display_name })
    }

    /// Returns the normalized presentation name selected by the patch.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }
}

/// Rejects a metadata request before an owner can mutate its declaration.
#[derive(Clone, Debug, thiserror::Error)]
pub enum DeploymentMetadataPatchError {
    /// The typed metadata input was omitted.
    #[error("deployment metadata is required")]
    MissingMetadata,

    /// The explicit metadata update mask was omitted.
    #[error("deployment metadata update_mask is required")]
    MissingUpdateMask,

    /// The update mask did not select any metadata fields.
    #[error("deployment metadata update_mask must not be empty")]
    EmptyUpdateMask,

    /// A requested path occurred more than once.
    #[error("deployment metadata update_mask repeats {0:?}")]
    DuplicatePath(String),

    /// A requested path is not currently writable.
    #[error("deployment metadata update_mask path {0:?} is not writable")]
    UnwritablePath(String),

    /// A selected metadata field did not supply a value.
    #[error("deployment metadata update_mask selects {0:?} without a value")]
    MissingMaskedValue(&'static str),

    /// The selected presentation name failed shared validation.
    #[error(transparent)]
    InvalidDisplayName(DeploymentNameError),
}

#[cfg(test)]
mod tests {
    use dotcore_proto::google::protobuf::FieldMask;

    use super::{DeploymentMetadataPatch, DeploymentMetadataPatchError};
    use crate::deployment::name::DeploymentNameError;

    #[test]
    fn parses_the_only_writable_path_and_normalizes_its_value() {
        let patch = DeploymentMetadataPatch::parse(
            Some(Some("  Research Model  ")),
            Some(&FieldMask {
                paths: vec!["display_name".to_owned()],
            }),
        )
        .expect("valid metadata patch");

        assert_eq!(patch.display_name(), "Research Model");
    }

    #[test]
    fn rejects_missing_metadata_mask_and_masked_value() {
        let display_name = Some("Research Model");
        let mask = FieldMask {
            paths: vec!["display_name".to_owned()],
        };

        assert!(matches!(
            DeploymentMetadataPatch::parse(None, Some(&mask)),
            Err(DeploymentMetadataPatchError::MissingMetadata)
        ));
        assert!(matches!(
            DeploymentMetadataPatch::parse(Some(display_name), None),
            Err(DeploymentMetadataPatchError::MissingUpdateMask)
        ));
        assert!(matches!(
            DeploymentMetadataPatch::parse(Some(None), Some(&mask)),
            Err(DeploymentMetadataPatchError::MissingMaskedValue(
                "display_name"
            ))
        ));
    }

    #[test]
    fn rejects_empty_duplicate_and_unwritable_paths() {
        for (paths, expected) in [
            (Vec::new(), "empty"),
            (
                vec!["display_name".to_owned(), "display_name".to_owned()],
                "duplicate",
            ),
            (vec!["*".to_owned()], "unwritable"),
            (vec!["displayName".to_owned()], "unwritable"),
            (vec!["metadata.display_name".to_owned()], "unwritable"),
            (vec!["display_name.value".to_owned()], "unwritable"),
        ] {
            let error = DeploymentMetadataPatch::parse(
                Some(Some("Research Model")),
                Some(&FieldMask { paths }),
            )
            .expect_err("invalid mask");
            match expected {
                "empty" => assert!(matches!(
                    error,
                    DeploymentMetadataPatchError::EmptyUpdateMask
                )),
                "duplicate" => assert!(matches!(
                    error,
                    DeploymentMetadataPatchError::DuplicatePath(_)
                )),
                "unwritable" => assert!(matches!(
                    error,
                    DeploymentMetadataPatchError::UnwritablePath(_)
                )),
                _ => unreachable!("test expectation is exhaustive"),
            }
        }
    }

    #[test]
    fn preserves_deployment_name_validation() {
        assert!(matches!(
            DeploymentMetadataPatch::parse(
                Some(Some(" \t ")),
                Some(&FieldMask {
                    paths: vec!["display_name".to_owned()],
                }),
            ),
            Err(DeploymentMetadataPatchError::InvalidDisplayName(
                DeploymentNameError::Empty
            ))
        ));
    }
}
