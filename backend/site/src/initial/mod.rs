//! Initial site creation: the typed request and identities of a new site, the one-time probe of the
//! local host, the site and node configuration composed from them, their validation, and the locked
//! writer that installs the input files.
//!
//! Nothing here runs against a live site: every step works on a stopped site directory, and the
//! input store's directory lock keeps a running server and offline administration apart.

pub mod input_store;
pub mod inputs;
pub mod probe;
pub mod request;
pub mod target_name;
pub mod validation;
