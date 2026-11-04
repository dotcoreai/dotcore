//! Shared public OpenAI response values for supported protocol routes.

/// Public OpenAI API response bytes and headers.
#[derive(Debug)]
pub(crate) struct OpenAiResponse {
    pub(crate) status: u16,
    pub(crate) content_type: String,
    pub(crate) body: Vec<u8>,
    pub(crate) passthrough_headers: Vec<(String, String)>,
}
