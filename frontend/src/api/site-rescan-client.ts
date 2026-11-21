import { type Client, createClient } from "@connectrpc/connect";

import { createConsoleTransport } from "@/api/console-transport";
import { resolveManagementOrigin } from "@/api/management-origin";
import { SiteRescanService } from "@/gen/dotcore/site/v1/rescan_pb";

export type SiteRescanClient = Pick<
  Client<typeof SiteRescanService>,
  "approveRescan" | "cancelRescan" | "rejectRescan" | "startRescan" | "watchRescan"
>;

const siteRescanServerUrl = resolveManagementOrigin(
  import.meta.env.VITE_DOTCORE_MANAGEMENT_ORIGIN,
  {
    allowTestFallback: import.meta.env.MODE === "test",
    allowViteFallback: import.meta.env.DEV || import.meta.env.MODE === "test",
  },
);

export function createSiteRescanClient(baseUrl = siteRescanServerUrl): SiteRescanClient {
  const generatedClient = createClient(SiteRescanService, createConsoleTransport(baseUrl));

  return {
    approveRescan: (request, options) => generatedClient.approveRescan(request, options),
    cancelRescan: (request, options) => generatedClient.cancelRescan(request, options),
    rejectRescan: (request, options) => generatedClient.rejectRescan(request, options),
    startRescan: (request, options) => generatedClient.startRescan(request, options),
    watchRescan: (request, options) => generatedClient.watchRescan(request, options),
  };
}
