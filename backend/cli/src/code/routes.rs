//! Reads the inference routes a coding agent can use from the site's inference API.
//!
//! The inference API lists the routes that can serve chat right now, the same list any
//! OpenAI-compatible client sees, so the command needs nothing beyond the API the agent itself
//! calls. One bounded request is made, and every failure names the address that was tried.

use std::time::Duration;

use reqwest::{Client, redirect::Policy, retry};
use serde::Deserialize;

use super::{CodeError, address::InferenceAddress};

/// How long reading the routes may take, connecting included.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// Reads the listed routes, in the inference API's order.
pub(super) async fn list_routes(address: &InferenceAddress) -> Result<Vec<ListedRoute>, CodeError> {
    let unreachable = |source| CodeError::InferenceUnreachable {
        address: address.clone(),
        source,
    };

    // The agent connects directly, so a proxy must not decide whether this read succeeds.
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .retry(retry::never())
        .timeout(LIST_TIMEOUT)
        .build()
        .map_err(unreachable)?;
    let response = client
        .get(address.models_url())
        .send()
        .await
        .map_err(unreachable)?;

    let status = response.status();
    if !status.is_success() {
        return Err(CodeError::InferenceRefused {
            address: address.clone(),
            status: status.as_u16(),
        });
    }

    let body = response.bytes().await.map_err(unreachable)?;
    let models = serde_json::from_slice::<ModelList>(&body).map_err(|source| {
        CodeError::InvalidRouteList {
            address: address.clone(),
            source,
        }
    })?;
    Ok(models.data)
}

/// One listed route: a model of the listing, whose id is the route's callable name.
#[derive(Debug, Deserialize)]
pub(super) struct ListedRoute {
    #[serde(rename = "id")]
    pub(super) name: String,

    /// Largest context one request may use on the route, in tokens, when the API states it.
    pub(super) context_length: Option<u64>,
}

/// The inference API's model listing; its other fields are not needed.
#[derive(Debug, Deserialize)]
struct ModelList {
    data: Vec<ListedRoute>,
}
