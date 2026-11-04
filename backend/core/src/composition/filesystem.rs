//! Composition-owned filesystem preparation for existing model cache inputs.

use std::path::{Path, PathBuf};

use super::CompositionError;

/// Creates and canonicalizes the existing model artifact-cache filesystem input.
pub(super) fn model_artifact_cache_path(root: &Path) -> Result<PathBuf, CompositionError> {
    std::fs::create_dir_all(root).map_err(|error| {
        CompositionError::composition_with_source(
            "Inference model artifact cache directory could not be created",
            error,
        )
    })?;
    root.canonicalize().map_err(|error| {
        CompositionError::composition_with_source(
            "Inference model artifact cache directory could not be canonicalized",
            error,
        )
    })
}
