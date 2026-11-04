//! The management API's `CapacityService`, answering the node memory read off the async executor
//! with a safe status when it fails.

use std::sync::Arc;

use dotcore_proto::dotcore::capacity::v1::{
    ListNodeMemoryRequest, ListNodeMemoryResponse,
    capacity_service_server::{CapacityService as CapacityServiceContract, CapacityServiceServer},
};
use tonic::{Request, Response, Status};
use tracing::warn;

use super::NodeMemoryRead;

/// Public management `CapacityService` answering the node memory read.
#[derive(Clone)]
pub(crate) struct CapacityService {
    read: Arc<NodeMemoryRead>,
}

impl CapacityService {
    pub(crate) fn new(read: Arc<NodeMemoryRead>) -> Self {
        Self { read }
    }

    /// Wraps the service for route assembly.
    pub(crate) fn into_server(self) -> CapacityServiceServer<Self> {
        CapacityServiceServer::new(self)
    }
}

#[tonic::async_trait]
impl CapacityServiceContract for CapacityService {
    async fn list_node_memory(
        &self,
        _request: Request<ListNodeMemoryRequest>,
    ) -> Result<Response<ListNodeMemoryResponse>, Status> {
        // Registry reads take the node coordinator's lock, which is held across disk writes.
        let read = Arc::clone(&self.read);
        tokio::task::spawn_blocking(move || read.list())
            .await
            .map_err(|_| Status::internal("Node memory read stopped"))?
            .map(Response::new)
            .map_err(|error| {
                warn!(error = %error, "node memory read failed");
                Status::unavailable("Node memory is unavailable")
            })
    }
}
