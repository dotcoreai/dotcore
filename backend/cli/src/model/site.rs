//! The site reads and writes that deploying one local model makes, behind one seam.
//!
//! Every deploy command reaches the site manager through this seam: the catalog and where its
//! profiles fit, the names of the nodes, the server's preview and estimate of one configuration,
//! and the deployment's creation. The running manager serves them; tests drive the guided flow
//! against a fake site instead.

use dotcore_proto::dotcore::{
    catalog::v1::{ListModelsRequest, Model},
    model::v1::{
        CreateModelDeploymentRequest, EstimateLocalModelResourcesRequest,
        EstimateLocalModelResourcesResponse, ListModelProfileCompatibilityRequest,
        ListModelProfileCompatibilityResponse, ModelDeployment, PreviewLocalModelDeploymentRequest,
        PreviewLocalModelDeploymentResponse,
    },
};
use futures_util::future::BoxFuture;
use tonic::Status;

use crate::management::{Manager, NodeNames};

/// What a model deployment asks of the site, each call answering as the management API does.
pub(super) trait DeploySite: Sync {
    /// The local catalog models.
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<Model>, Status>>;

    /// Where each catalog profile fits, narrowed as the request asks.
    fn compatibility(
        &self,
        request: ListModelProfileCompatibilityRequest,
    ) -> BoxFuture<'_, Result<ListModelProfileCompatibilityResponse, Status>>;

    /// The display names of the nodes and their targets; a failed read names none.
    fn node_names(&self) -> BoxFuture<'_, NodeNames>;

    /// The server's resolution of one deployment, with its advisories.
    fn preview(
        &self,
        request: PreviewLocalModelDeploymentRequest,
    ) -> BoxFuture<'_, Result<PreviewLocalModelDeploymentResponse, Status>>;

    /// The server's memory estimate of one configuration on its target.
    fn estimate(
        &self,
        request: EstimateLocalModelResourcesRequest,
    ) -> BoxFuture<'_, Result<EstimateLocalModelResourcesResponse, Status>>;

    /// Creates the deployment and returns it as accepted.
    fn create(
        &self,
        request: CreateModelDeploymentRequest,
    ) -> BoxFuture<'_, Result<ModelDeployment, Status>>;
}

impl DeploySite for Manager {
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<Model>, Status>> {
        Box::pin(async move {
            Ok(self
                .catalog()
                .list_models(ListModelsRequest {})
                .await?
                .into_inner()
                .models)
        })
    }

    fn compatibility(
        &self,
        request: ListModelProfileCompatibilityRequest,
    ) -> BoxFuture<'_, Result<ListModelProfileCompatibilityResponse, Status>> {
        Box::pin(async move {
            Ok(self
                .models()
                .list_model_profile_compatibility(request)
                .await?
                .into_inner())
        })
    }

    fn node_names(&self) -> BoxFuture<'_, NodeNames> {
        Box::pin(Manager::node_names(self))
    }

    fn preview(
        &self,
        request: PreviewLocalModelDeploymentRequest,
    ) -> BoxFuture<'_, Result<PreviewLocalModelDeploymentResponse, Status>> {
        Box::pin(async move {
            Ok(self
                .models()
                .preview_local_model_deployment(request)
                .await?
                .into_inner())
        })
    }

    fn estimate(
        &self,
        request: EstimateLocalModelResourcesRequest,
    ) -> BoxFuture<'_, Result<EstimateLocalModelResourcesResponse, Status>> {
        Box::pin(async move {
            Ok(self
                .models()
                .estimate_local_model_resources(request)
                .await?
                .into_inner())
        })
    }

    fn create(
        &self,
        request: CreateModelDeploymentRequest,
    ) -> BoxFuture<'_, Result<ModelDeployment, Status>> {
        Box::pin(async move {
            Ok(self
                .models()
                .create_model_deployment(request)
                .await?
                .into_inner()
                .deployment
                .unwrap_or_default())
        })
    }
}
