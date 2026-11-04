//! Public OpenAI API codec modules for router requests and responses.

pub(crate) mod chat;
pub(crate) mod responses;
mod values;

pub(crate) use chat::*;
pub(crate) use values::OpenAiResponse;
