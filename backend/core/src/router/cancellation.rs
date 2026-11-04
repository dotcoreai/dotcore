//! Request cancellation state for router listener and engine I/O.

pub(crate) use crate::inference::{
    InferenceCancellation as RequestCancellation,
    InferenceCancellationReason as RequestCancellationReason,
};
