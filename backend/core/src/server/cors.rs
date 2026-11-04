//! Management transport CORS policy.

use http::{
    HeaderName, HeaderValue, Method,
    header::{ACCEPT, CONTENT_TYPE},
};
use tower_http::cors::CorsLayer;

pub(super) fn for_origins(allowed_origins: Vec<HeaderValue>) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(allowed_origins)
        .allow_methods([Method::POST])
        .allow_headers([
            ACCEPT,
            CONTENT_TYPE,
            HeaderName::from_static("grpc-accept-encoding"),
            HeaderName::from_static("grpc-encoding"),
            HeaderName::from_static("grpc-timeout"),
            HeaderName::from_static("x-grpc-web"),
            HeaderName::from_static("x-user-agent"),
        ])
        .expose_headers([
            HeaderName::from_static("grpc-accept-encoding"),
            HeaderName::from_static("grpc-encoding"),
            HeaderName::from_static("grpc-message"),
            HeaderName::from_static("grpc-status"),
            HeaderName::from_static("grpc-status-details-bin"),
        ])
}
