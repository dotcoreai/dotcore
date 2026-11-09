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
import { AppCatalogPage } from "@/features/applications/app-catalog-page";
import { ApplicationsPage } from "@/features/applications/applications-page";
import { ApplicationsSection } from "@/features/applications/applications-section";
import { ChatPage } from "@/features/chat/chat-page";
import { RoutesPage } from "@/features/inference-routes/routes-page";
import { MachinesPage } from "@/features/machines/machines-page";
import { ModelCatalogPage } from "@/features/models/model-catalog-page";
import { ModelsPage } from "@/features/models/models-page";
import { OverviewPage } from "@/features/overview/overview-page";
import { PreferencesPage } from "@/features/preferences/preferences-page";

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
          element: createElement(OverviewPage, {
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
              element: createElement(ModelsPage, { client }),
            },
            {
              path: "catalog",
              element: createElement(ModelCatalogPage, { client, siteClient: machinesClient }),
            },
          ],
        },
        {
          path: "/routes",
          element: createElement(RoutesPage, { client: routesClient }),
        },
        {
          path: "/chat",
          element: createElement(ChatPage),
        },
        {
          path: "/machines",
          element: createElement(MachinesPage),
        },
        {
          path: "/preferences",
          element: createElement(PreferencesPage),
        },
        {
          path: "/apps",
          element: createElement(ApplicationsSection, { client: appsClient }),
          children: [
            {
              index: true,
              element: createElement(ApplicationsPage, { client: appsClient }),
            },
            {
              path: "catalog",
              element: createElement(AppCatalogPage, { client: appsClient }),
            },
          ],
        },
      ],
    },
  ];

  return appRoutes;
}

export const routes = createRoutes();
