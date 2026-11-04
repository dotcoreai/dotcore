//! Defines the integration-only gRPC contract used to qualify private ingress.
//! The contract is compiled only into this test target and is never mounted by normal backend assembly.

use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use dotcore_site::private_transport::{
    AuthenticatedRoleAuthorization, PrivateNodeRole, PrivateServiceContext,
};
use prost::Message;
use tonic::{Request, Response, Status, body::Body, codegen::BoxFuture, transport::Channel};

const SERVICE_NAME: &str = "dotcore.test.PrivateIngressQualification";
const WORKER_CALL_PATH: &str = "/dotcore.test.PrivateIngressQualification/WorkerCall";
const MANAGER_CALL_PATH: &str = "/dotcore.test.PrivateIngressQualification/ManagerCall";

#[derive(Clone, PartialEq, Message)]
pub struct QualificationRequest {}

#[derive(Clone, PartialEq, Message)]
pub struct QualificationResponse {
    #[prost(string, tag = "1")]
    pub site_id: String,

    #[prost(string, tag = "2")]
    pub node_id: String,
}

#[derive(Clone, Default)]
pub struct InvocationCounts {
    worker_calls: Arc<AtomicUsize>,
    manager_calls: Arc<AtomicUsize>,
}

impl InvocationCounts {
    pub fn worker_calls(&self) -> usize {
        self.worker_calls.load(Ordering::SeqCst)
    }

    pub fn manager_calls(&self) -> usize {
        self.manager_calls.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
pub struct QualificationService {
    worker_authorization: AuthenticatedRoleAuthorization,
    manager_authorization: AuthenticatedRoleAuthorization,
    counts: InvocationCounts,
}

impl QualificationService {
    pub fn add_routes(
        context: PrivateServiceContext,
        counts: InvocationCounts,
        routes: &mut tonic::service::RoutesBuilder,
    ) {
        routes.add_service(QualificationServiceServer::new(Self {
            worker_authorization: context.authorize_role(PrivateNodeRole::Worker),
            manager_authorization: context.authorize_role(PrivateNodeRole::Manager),
            counts,
        }));
    }

    async fn worker_call(
        &self,
        request: Request<QualificationRequest>,
    ) -> Result<Response<QualificationResponse>, Status> {
        let identity = self.worker_authorization.authorize(&request)?;
        self.counts.worker_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(QualificationResponse {
            site_id: identity.site_id,
            node_id: identity.node_id,
        }))
    }

    async fn manager_call(
        &self,
        request: Request<QualificationRequest>,
    ) -> Result<Response<QualificationResponse>, Status> {
        let identity = self.manager_authorization.authorize(&request)?;
        self.counts.manager_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(QualificationResponse {
            site_id: identity.site_id,
            node_id: identity.node_id,
        }))
    }
}

pub struct QualificationClient {
    inner: tonic::client::Grpc<Channel>,
}

impl QualificationClient {
    pub fn new(channel: Channel) -> Self {
        Self {
            inner: tonic::client::Grpc::new(channel),
        }
    }

    pub async fn worker_call(&mut self) -> Result<Response<QualificationResponse>, Status> {
        self.call(WORKER_CALL_PATH).await
    }

    pub async fn manager_call(&mut self) -> Result<Response<QualificationResponse>, Status> {
        self.call(MANAGER_CALL_PATH).await
    }

    async fn call(
        &mut self,
        path: &'static str,
    ) -> Result<Response<QualificationResponse>, Status> {
        self.inner.ready().await.map_err(|error| {
            Status::unknown(format!("test qualification service was not ready: {error}"))
        })?;
        self.inner
            .unary(
                Request::new(QualificationRequest {}),
                path.parse().expect("static qualification path"),
                tonic_prost::ProstCodec::default(),
            )
            .await
    }
}

#[derive(Clone)]
struct QualificationServiceServer {
    inner: Arc<QualificationService>,
}

impl QualificationServiceServer {
    fn new(inner: QualificationService) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}

impl<B> tonic::codegen::Service<http::Request<B>> for QualificationServiceServer
where
    B: http_body::Body + Send + 'static,
    B::Error: Into<tonic::codegen::StdError> + Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        match request.uri().path() {
            WORKER_CALL_PATH => unary_response(request, Arc::clone(&self.inner), Call::Worker),
            MANAGER_CALL_PATH => unary_response(request, Arc::clone(&self.inner), Call::Manager),
            _ => Box::pin(async move {
                let mut response = http::Response::new(Body::empty());
                response.headers_mut().insert(
                    Status::GRPC_STATUS,
                    (tonic::Code::Unimplemented as i32).into(),
                );
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    tonic::metadata::GRPC_CONTENT_TYPE,
                );
                Ok(response)
            }),
        }
    }
}

impl tonic::server::NamedService for QualificationServiceServer {
    const NAME: &'static str = SERVICE_NAME;
}

#[derive(Clone, Copy)]
enum Call {
    Worker,
    Manager,
}

fn unary_response<B>(
    request: http::Request<B>,
    service: Arc<QualificationService>,
    call: Call,
) -> BoxFuture<http::Response<Body>, Infallible>
where
    B: http_body::Body + Send + 'static,
    B::Error: Into<tonic::codegen::StdError> + Send + 'static,
{
    Box::pin(async move {
        let method = QualificationUnary { service, call };
        let mut grpc = tonic::server::Grpc::new(tonic_prost::ProstCodec::default());
        Ok(grpc.unary(method, request).await)
    })
}

struct QualificationUnary {
    service: Arc<QualificationService>,
    call: Call,
}

impl tonic::server::UnaryService<QualificationRequest> for QualificationUnary {
    type Response = QualificationResponse;
    type Future = BoxFuture<Response<Self::Response>, Status>;

    fn call(&mut self, request: Request<QualificationRequest>) -> Self::Future {
        let service = Arc::clone(&self.service);
        let call = self.call;
        Box::pin(async move {
            match call {
                Call::Worker => service.worker_call(request).await,
                Call::Manager => service.manager_call(request).await,
            }
        })
    }
}
