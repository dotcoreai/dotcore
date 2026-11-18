import { type Client, createClient } from "@connectrpc/connect";

import { createConsoleTransport } from "@/api/console-transport";
import { resolveManagementOrigin } from "@/api/management-origin";
import { SiteJoinService } from "@/gen/dotcore/site/v1/join_pb";

export type SiteJoinClient = Pick<
  Client<typeof SiteJoinService>,
  "approveJoin" | "cancelJoin" | "rejectJoin" | "startJoin" | "watchJoin"
>;

const siteJoinServerUrl = resolveManagementOrigin(import.meta.env.VITE_DOTCORE_MANAGEMENT_ORIGIN, {
  allowTestFallback: import.meta.env.MODE === "test",
  allowViteFallback: import.meta.env.DEV || import.meta.env.MODE === "test",
});

export function createSiteJoinClient(baseUrl = siteJoinServerUrl): SiteJoinClient {
  const generatedClient = createClient(SiteJoinService, createConsoleTransport(baseUrl));

  return {
    approveJoin: (request, options) => generatedClient.approveJoin(request, options),
    cancelJoin: (request, options) => generatedClient.cancelJoin(request, options),
    rejectJoin: (request, options) => generatedClient.rejectJoin(request, options),
    startJoin: (request, options) => generatedClient.startJoin(request, options),
    watchJoin: (request, options) => generatedClient.watchJoin(request, options),
  };
}
