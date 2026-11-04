//! Upstream model template validation for immutable catalog declarations.

use dotcore_domain::valid_id;
use dotcore_proto::dotcore::catalog::v1::{UpstreamModelTemplate, UpstreamWireProtocol};

const OPENAI_COMPATIBLE_TEMPLATE_ID: &str = "openai-compatible";
const OPENAI_COMPATIBLE_TEMPLATE_DISPLAY_NAME: &str = "OpenAI-compatible";

pub fn validate_upstream_model_template(template: &UpstreamModelTemplate) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    if !valid_id(&template.upstream_model_template_id) {
        errors.push(anyhow::anyhow!(
            "upstream model template id {:?} must match [a-z][a-z0-9-]{{0,62}}",
            template.upstream_model_template_id
        ));
    } else if template.upstream_model_template_id != OPENAI_COMPATIBLE_TEMPLATE_ID {
        errors.push(anyhow::anyhow!(
            "upstream model template id must be {OPENAI_COMPATIBLE_TEMPLATE_ID:?}"
        ));
    }
    if template.display_name != OPENAI_COMPATIBLE_TEMPLATE_DISPLAY_NAME {
        errors.push(anyhow::anyhow!(
            "upstream model template displayName must be {OPENAI_COMPATIBLE_TEMPLATE_DISPLAY_NAME:?}"
        ));
    }
    if template.wire_protocol != UpstreamWireProtocol::OpenaiCompatible as i32 {
        errors.push(anyhow::anyhow!(
            "upstream model template wireProtocol must be OPENAI_COMPATIBLE"
        ));
    }
    errors
}
