//! Router-owned raw inference HTTP facade.

mod connection;
mod errors;
mod request;
mod response;
mod stream;
mod watchers;

#[cfg(test)]
mod tests;

pub(crate) use connection::handle_inference_connection;
pub use errors::ConnectionError;
pub(crate) use request::MAX_OPENAI_REQUEST_BODY_BYTES;
