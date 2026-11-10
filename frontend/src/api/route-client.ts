import { type Client, createClient } from "@connectrpc/connect";

import { createConsoleTransport } from "@/api/console-transport";
import { resolveManagementOrigin } from "@/api/management-origin";
import { RouteService } from "@/gen/dotcore/inference/v1/routes_pb";

export type RouteServiceClient = Pick<
  Client<typeof RouteService>,
  "listRoutes" | "getRoute" | "createRoute" | "replaceRoute" | "deleteRoute"
>;

export interface RouteClient {
  route: RouteServiceClient;
}

const routeServerUrl = resolveManagementOrigin(import.meta.env.VITE_DOTCORE_MANAGEMENT_ORIGIN, {
  allowTestFallback: import.meta.env.MODE === "test",
  allowViteFallback: import.meta.env.DEV || import.meta.env.MODE === "test",
});

export function createRouteClient(baseUrl = routeServerUrl): RouteClient {
  const transport = createConsoleTransport(baseUrl);
  const route = createClient(RouteService, transport);

  return {
    route: {
      listRoutes: route.listRoutes,
      getRoute: route.getRoute,
      createRoute: route.createRoute,
      replaceRoute: route.replaceRoute,
      deleteRoute: route.deleteRoute,
    },
  };
}
