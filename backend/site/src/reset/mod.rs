//! Destructive administration of a stopped site: resetting it to fresh inputs or deleting its
//! directory, the guarded removal of its persistence root, and the runtime workload removal that
//! must succeed before a site directory or node folder is deleted.
//!
//! Every operation holds its site directory or node folder lock, so it never races a running
//! server.

pub mod persistence;
pub mod runtime_removal;
pub mod transaction;
