import { createElement } from "react";
import { Navigate } from "react-router-dom";

import { type ApplicationClient, applicationClient } from "@/api/application-client";
import { type MetricsClient, metricsClient } from "@/api/metrics-client";
import { type ModelClient, modelClient } from "@/api/model-client";
import { type ResourceWatchClient, resourceWatchClient } from "@/api/resource-watch-client";
import { type RouteClient, routeClient } from "@/api/route-client";
import { type SiteClient, siteClient } from "@/api/site-client";
import { ConsoleLayout } from "@/app/console-layout";
import { createPanelTitleLookup } from "@/app/panel-title-lookup";
import { AppCatalogRoute } from "@/routes/app-catalog";
import { AppsRoute } from "@/routes/apps";
import { AppsSectionRoute } from "@/routes/apps-section";
import { ChatRoute } from "@/routes/chat";
import { HomeRoute } from "@/routes/home";
import { MachinesRoute } from "@/routes/machines";
import { ModelCatalogRoute } from "@/routes/model-catalog";
import { OverviewRoute } from "@/routes/overview";
import { PreferencesRoute } from "@/routes/preferences";
import { RoutesRoute } from "@/routes/routes";

export interface RouteOptions {
  applicationClient?: ApplicationClient;
  metricsClient?: MetricsClient;
  modelClient?: ModelClient;
  resourceWatchClient?: ResourceWatchClient;
  routeClient?: RouteClient;
  siteClient?: SiteClient;
}

export function createRoutes({
  applicationClient: appsClient = applicationClient,
  metricsClient: metrics = metricsClient,
  modelClient: client = modelClient,
  resourceWatchClient: watchClient = resourceWatchClient,
  routeClient: routesClient = routeClient,
  siteClient: machinesClient = siteClient,
}: RouteOptions = {}) {
  const appRoutes = [
    {
      path: "/",
      element: createElement(Navigate, { replace: true, to: "/overview" }),
    },
    {
      element: createElement(ConsoleLayout, {
        modelClient: client,
        panelTitleLookup: createPanelTitleLookup(routesClient, appsClient),
        resourceWatchClient: watchClient,
      }),
      children: [
        {
          path: "/overview",
          element: createElement(OverviewRoute, {
            metricsClient: metrics,
            modelClient: client,
            routeClient: routesClient,
          }),
        },
        {
          path: "/models",
          children: [
            {
              index: true,
              element: createElement(HomeRoute, { client }),
            },
            {
              path: "catalog",
              element: createElement(ModelCatalogRoute, { client, siteClient: machinesClient }),
            },
          ],
        },
        {
          path: "/routes",
          element: createElement(RoutesRoute, { client: routesClient }),
        },
        {
          path: "/chat",
          element: createElement(ChatRoute),
        },
        {
          path: "/machines",
          element: createElement(MachinesRoute),
        },
        {
          path: "/preferences",
          element: createElement(PreferencesRoute),
        },
        {
          path: "/apps",
          element: createElement(AppsSectionRoute, { client: appsClient }),
          children: [
            {
              index: true,
              element: createElement(AppsRoute, { client: appsClient }),
            },
            {
              path: "catalog",
              element: createElement(AppCatalogRoute, { client: appsClient }),
            },
          ],
        },
      ],
    },
  ];

  return appRoutes;
}

export const routes = createRoutes();
