//! gRPC service edge for immutable catalog releases.

use std::sync::Arc;

use dotcore_proto::dotcore::catalog::v1::{
    ListApplicationsRequest, ListApplicationsResponse, ListEngineReleasesRequest,
    ListEngineReleasesResponse, ListHardwareClassesRequest, ListHardwareClassesResponse,
    ListModelsRequest, ListModelsResponse, ListUpstreamModelTemplatesRequest,
    ListUpstreamModelTemplatesResponse,
    catalog_service_server::CatalogService as CatalogServiceContract,
};
use tonic::{Request, Response, Status};
use tracing::{Instrument, Span, debug_span, error, field};

use super::Catalog;

/// Translates catalog service reads to the catalog owner.
#[derive(Debug)]
pub struct CatalogService {
    catalog: Arc<Catalog>,
}

impl CatalogService {
    /// Creates a read service over the supplied immutable catalog.
    pub fn new(catalog: Arc<Catalog>) -> Self {
        Self { catalog }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn catalog_pointer(&self) -> *const Catalog {
        Arc::as_ptr(&self.catalog)
    }
}

#[tonic::async_trait]
impl CatalogServiceContract for CatalogService {
    async fn list_models(
        &self,
        _request: Request<ListModelsRequest>,
    ) -> Result<Response<ListModelsResponse>, Status> {
        let span = debug_span!(
            "catalog_list_models",
            operation = "catalog_list_models",
            model_count = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
            status = field::Empty,
        );
        let result = async {
            let models = self.catalog.list_model_releases();
            span.record("model_count", models.len());
            Ok(Response::new(ListModelsResponse { models }))
        }
        .instrument(span.clone())
        .await;
        record_catalog_result(&span, &result);
        result
    }

    async fn list_engine_releases(
        &self,
        _request: Request<ListEngineReleasesRequest>,
    ) -> Result<Response<ListEngineReleasesResponse>, Status> {
        let span = debug_span!(
            "catalog_list_engine_releases",
            operation = "catalog_list_engine_releases",
            engine_release_count = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
            status = field::Empty,
        );
        let result = async {
            let engine_releases = self.catalog.list_engine_releases();
            span.record("engine_release_count", engine_releases.len());
            Ok(Response::new(ListEngineReleasesResponse {
                engine_releases,
            }))
        }
        .instrument(span.clone())
        .await;
        record_catalog_result(&span, &result);
        result
    }

    async fn list_applications(
        &self,
        _request: Request<ListApplicationsRequest>,
    ) -> Result<Response<ListApplicationsResponse>, Status> {
        let span = debug_span!(
            "catalog_list_applications",
            operation = "catalog_list_applications",
            application_count = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
            status = field::Empty,
        );
        let result = async {
            let applications = self.catalog.list_application_releases();
            span.record("application_count", applications.len());
            Ok(Response::new(ListApplicationsResponse { applications }))
        }
        .instrument(span.clone())
        .await;
        record_catalog_result(&span, &result);
        result
    }

    async fn list_upstream_model_templates(
        &self,
        _request: Request<ListUpstreamModelTemplatesRequest>,
    ) -> Result<Response<ListUpstreamModelTemplatesResponse>, Status> {
        let span = debug_span!(
            "catalog_list_upstream_model_templates",
            operation = "catalog_list_upstream_model_templates",
            upstream_model_template_count = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
            status = field::Empty,
        );
        let result = async {
            let upstream_model_templates = self.catalog.list_upstream_model_templates();
            span.record(
                "upstream_model_template_count",
                upstream_model_templates.len(),
            );
            Ok(Response::new(ListUpstreamModelTemplatesResponse {
                upstream_model_templates,
            }))
        }
        .instrument(span.clone())
        .await;
        record_catalog_result(&span, &result);
        result
    }

    async fn list_hardware_classes(
        &self,
        _request: Request<ListHardwareClassesRequest>,
    ) -> Result<Response<ListHardwareClassesResponse>, Status> {
        let span = debug_span!(
            "catalog_list_hardware_classes",
            operation = "catalog_list_hardware_classes",
            hardware_class_count = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
            status = field::Empty,
        );
        let result = async {
            let hardware_classes = self.catalog.list_hardware_classes();
            span.record("hardware_class_count", hardware_classes.len());
            Ok(Response::new(ListHardwareClassesResponse {
                hardware_classes,
            }))
        }
        .instrument(span.clone())
        .await;
        record_catalog_result(&span, &result);
        result
    }
}

fn record_catalog_result<T>(span: &Span, result: &Result<Response<T>, Status>) {
    match result {
        Ok(_) => {
            span.record("outcome", "success");
        }
        Err(error) => {
            span.record("outcome", "failed");
            span.record("error_kind", "catalog_read");
            span.record("status", error.code().to_string());
            error!(parent: span, error_kind = "catalog_read", status = ?error.code(), "Catalog read failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use dotcore_domain::test_support::{
        oci::digest_artifact,
        tracing::{Record, capture_records, capture_records_async},
    };
    use dotcore_proto::dotcore::catalog::v1::{
        Application, ApplicationEndpointProtocol, ApplicationGraph, ApplicationPrimaryEndpoint,
        ApplicationService, ApplicationServicePort, ListApplicationsRequest,
        ListHardwareClassesRequest, ListModelsResponse,
    };
    use tonic::{Request, Status};
    use tracing::{debug_span, field};

    use super::{CatalogService, CatalogServiceContract, record_catalog_result};
    use crate::{CatalogBuilder, test_support::vendor_hardware_class};

    #[test]
    fn catalog_read_failures_record_error_spans_and_error_events() {
        let (_, records, output) = capture_records(|| {
            let span = debug_span!(
                "catalog_list_models",
                operation = "catalog_list_models",
                model_count = field::Empty,
                outcome = field::Empty,
                error_kind = field::Empty,
                status = field::Empty,
            );
            let result = Err::<tonic::Response<ListModelsResponse>, _>(Status::internal(
                "secret catalog details",
            ));
            span.in_scope(|| record_catalog_result(&span, &result));
        });

        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "catalog_list_models"
                    && fields.get("outcome").is_some_and(|value| value.contains("failed"))
                    && fields.get("error_kind").is_some_and(|value| value.contains("catalog_read"))
        )));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::Event { level, fields }
                if level == "ERROR"
                    && fields.get("error_kind").is_some_and(|value| value.contains("catalog_read"))
        )));
        assert!(!output.contains("secret catalog details"));
    }

    #[tokio::test]
    async fn list_applications_records_count_and_success_outcome() {
        let service = CatalogService::new(Arc::new(
            CatalogBuilder::new()
                .application("test", test_application())
                .build()
                .expect("valid test catalog"),
        ));
        let (response, records, _) = capture_records_async(|| async {
            CatalogServiceContract::list_applications(
                &service,
                Request::new(ListApplicationsRequest {}),
            )
            .await
            .expect("Catalog application read")
            .into_inner()
        })
        .await;

        assert_eq!(response.applications.len(), 1);
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "catalog_list_applications"
                    && fields.get("application_count") == Some(&"1".to_owned())
                    && fields.get("outcome") == Some(&"\"success\"".to_owned())
        )));
    }

    #[tokio::test]
    async fn list_hardware_classes_returns_classes_in_identity_order() {
        let service = CatalogService::new(Arc::new(
            CatalogBuilder::new()
                .hardware_class("nvidia.yaml", vendor_hardware_class("nvidia"))
                .hardware_class("apple.yaml", vendor_hardware_class("apple"))
                .build()
                .expect("valid test catalog"),
        ));
        let (response, records, _) = capture_records_async(|| async {
            CatalogServiceContract::list_hardware_classes(
                &service,
                Request::new(ListHardwareClassesRequest {}),
            )
            .await
            .expect("Catalog hardware class read")
            .into_inner()
        })
        .await;

        assert_eq!(
            response.hardware_classes,
            vec![
                vendor_hardware_class("apple"),
                vendor_hardware_class("nvidia")
            ]
        );
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "catalog_list_hardware_classes"
                    && fields.get("hardware_class_count") == Some(&"2".to_owned())
                    && fields.get("outcome") == Some(&"\"success\"".to_owned())
        )));
    }

    fn test_application() -> Application {
        Application {
            application_id: "open-webui".to_owned(),
            display_name: "Open WebUI".to_owned(),
            description: String::new(),
            notes: String::new(),
            version: "1".to_owned(),
            graph: Some(ApplicationGraph {
                services: vec![ApplicationService {
                    service_name: "web".to_owned(),
                    artifact: Some(digest_artifact(
                        None,
                        "example/web",
                        "5c75d3cc7ff9c199c07e157ae7710370e2b698979a70a64639ead03b7cf4c679",
                        None,
                    )),
                    ports: vec![ApplicationServicePort {
                        port_name: "http".to_owned(),
                        port_number: 8080,
                    }],
                    ..Default::default()
                }],
                primary_endpoint: Some(ApplicationPrimaryEndpoint {
                    service_name: "web".to_owned(),
                    port_name: "http".to_owned(),
                    protocol: ApplicationEndpointProtocol::Http as i32,
                }),
                parameters: Vec::new(),
            }),
        }
    }
}
