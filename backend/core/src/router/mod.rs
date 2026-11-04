//! Private inference request handling over route-manager snapshots and controller-published serving targets.

pub(crate) mod adapters;
pub(crate) mod cancellation;
pub(crate) mod decision_validation;
pub(crate) mod http;
pub(crate) mod images;
pub(crate) mod metrics;
pub(crate) mod openai;
pub(crate) mod openai_compatible_codec;
pub(crate) mod peer_service;
pub(crate) mod request_controls;
pub(crate) mod selection;
pub(crate) mod systemone;

pub(crate) use selection::InferenceRouter;

#[cfg(test)]
pub(crate) mod test_fixtures;
