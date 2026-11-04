import { type Client, createClient } from "@connectrpc/connect";

import { createConsoleTransport } from "@/api/console-transport";
import { resolveManagementOrigin } from "@/api/management-origin";
import { SiteService } from "@/gen/dotcore/site/v1/service_pb";

export type SiteClient = Pick<Client<typeof SiteService>, "listConfiguredExecutionTargets">;

const siteServerUrl = resolveManagementOrigin(import.meta.env.VITE_DOTCORE_MANAGEMENT_ORIGIN, {
  allowTestFallback: import.meta.env.MODE === "test",
  allowViteFallback: import.meta.env.DEV || import.meta.env.MODE === "test",
});

export function createSiteClient(baseUrl = siteServerUrl): SiteClient {
  const generatedClient = createClient(SiteService, createConsoleTransport(baseUrl));

  return {
    listConfiguredExecutionTargets: (request, options) =>
      generatedClient.listConfiguredExecutionTargets(request, options),
  };
}

export const siteClient = createSiteClient();
