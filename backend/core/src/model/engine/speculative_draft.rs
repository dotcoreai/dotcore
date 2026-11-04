//! Speculative draft sources shared by the engine adapters.
//!
//! An engine payload declares where a speculative draft comes from: a head embedded in the main
//! model weights or a separate model-local draft artifact. This module reads each engine's
//! declaration into one shape, so adapters match on it exhaustively and treat a missing source as
//! invalid.

use dotcore_proto::dotcore::engine::{
    llama_cpp::v1::{LlamaCppSpeculativeDraftSource, llama_cpp_speculative_draft_source},
    vllm::v1::{VllmMtpDraftSource, vllm_mtp_draft_source},
};

/// Where one speculative draft comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpeculativeDraft<'a> {
    /// The draft head is embedded in the main model weights.
    Embedded,

    /// The draft is a separate model-local auxiliary artifact.
    Artifact(&'a str),
}

impl<'a> SpeculativeDraft<'a> {
    /// Returns the draft auxiliary artifact, or none for an embedded head.
    pub(crate) fn artifact_id(self) -> Option<&'a str> {
        match self {
            Self::Embedded => None,
            Self::Artifact(artifact_id) => Some(artifact_id),
        }
    }
}

/// Reads a llama.cpp draft source, or none when it is missing or names an empty artifact.
pub(crate) fn llama_cpp(
    source: Option<&LlamaCppSpeculativeDraftSource>,
) -> Option<SpeculativeDraft<'_>> {
    match source?.source.as_ref()? {
        llama_cpp_speculative_draft_source::Source::Embedded(_) => Some(SpeculativeDraft::Embedded),
        llama_cpp_speculative_draft_source::Source::AuxiliaryArtifactId(artifact_id) => {
            artifact(artifact_id)
        }
    }
}

/// Reads a vLLM draft source, or none when it is missing or names an empty artifact.
pub(crate) fn vllm(source: Option<&VllmMtpDraftSource>) -> Option<SpeculativeDraft<'_>> {
    match source?.source.as_ref()? {
        vllm_mtp_draft_source::Source::Embedded(_) => Some(SpeculativeDraft::Embedded),
        vllm_mtp_draft_source::Source::AuxiliaryArtifactId(artifact_id) => artifact(artifact_id),
    }
}

fn artifact(artifact_id: &str) -> Option<SpeculativeDraft<'_>> {
    (!artifact_id.is_empty()).then_some(SpeculativeDraft::Artifact(artifact_id))
}
