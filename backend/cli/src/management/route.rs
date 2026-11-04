//! Reads the inference routes of the running site as an inference client sees them.
//!
//! Only enabled routes are returned, since a disabled route refuses every request. Each keeps its
//! callable name and whether a ready model deployment currently serves it, in the manager's
//! name order.

use dotcore_proto::dotcore::inference::v1::{
    InferenceRouteView, ListRoutesRequest, route_service_client::RouteServiceClient,
};

use super::{ManagementError, Manager};

/// An enabled inference route as an inference client sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InferenceRoute {
    /// The callable name, which inference requests pass as their model.
    pub(crate) name: String,

    /// Whether a ready model deployment serves the route right now.
    pub(crate) available: bool,
}

/// Reads the enabled inference routes in the manager's order.
pub(crate) async fn list_enabled_routes(
    manager: &Manager,
) -> Result<Vec<InferenceRoute>, ManagementError> {
    let response = RouteServiceClient::new(manager.channel.clone())
        .list_routes(ListRoutesRequest {})
        .await
        .map_err(|status| ManagementError::RouteList {
            code: status.code(),
            message: status.message().to_owned(),
        })?;
    Ok(enabled_routes(response.into_inner().routes))
}

/// Keeps the enabled routes with their name and availability.
fn enabled_routes(views: Vec<InferenceRouteView>) -> Vec<InferenceRoute> {
    views
        .into_iter()
        .filter_map(|view| {
            let route = view.route.filter(|route| route.enabled)?;
            Some(InferenceRoute {
                name: route.name,
                available: view
                    .availability
                    .is_some_and(|availability| availability.available),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::{
        InferenceRoute as RouteDeclaration, InferenceRouteAvailability,
    };

    use super::*;

    fn view(name: &str, enabled: bool, available: Option<bool>) -> InferenceRouteView {
        InferenceRouteView {
            route: Some(RouteDeclaration {
                inference_route_id: format!("route-{name}"),
                name: name.to_owned(),
                enabled,
                ..Default::default()
            }),
            availability: available.map(|available| InferenceRouteAvailability {
                available,
                ..Default::default()
            }),
            request_kinds: Vec::new(),
        }
    }

    #[test]
    fn only_enabled_routes_are_kept_with_their_availability_in_order() {
        let routes = enabled_routes(vec![
            view("chat", true, Some(true)),
            view("disabled", false, Some(false)),
            view("embed", true, Some(false)),
            view("unknown", true, None),
            InferenceRouteView::default(),
        ]);

        assert_eq!(
            routes,
            vec![
                InferenceRoute {
                    name: "chat".to_owned(),
                    available: true,
                },
                InferenceRoute {
                    name: "embed".to_owned(),
                    available: false,
                },
                InferenceRoute {
                    name: "unknown".to_owned(),
                    available: false,
                },
            ]
        );
    }
}
