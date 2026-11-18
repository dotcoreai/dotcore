import { type Client, createClient } from "@connectrpc/connect";

import { createConsoleTransport } from "@/api/console-transport";
import { resolveManagementOrigin } from "@/api/management-origin";
import { SiteMembershipService } from "@/gen/dotcore/site/v1/membership_pb";

export type SiteMembershipClient = Pick<
  Client<typeof SiteMembershipService>,
  "approveJoin" | "cancelJoin" | "rejectJoin" | "removeMachine" | "startJoin" | "watchJoin"
>;

const siteMembershipServerUrl = resolveManagementOrigin(
  import.meta.env.VITE_DOTCORE_MANAGEMENT_ORIGIN,
  {
    allowTestFallback: import.meta.env.MODE === "test",
    allowViteFallback: import.meta.env.DEV || import.meta.env.MODE === "test",
  },
);

export function createSiteMembershipClient(
  baseUrl = siteMembershipServerUrl,
): SiteMembershipClient {
  const generatedClient = createClient(SiteMembershipService, createConsoleTransport(baseUrl));

  return {
    approveJoin: (request, options) => generatedClient.approveJoin(request, options),
    cancelJoin: (request, options) => generatedClient.cancelJoin(request, options),
    rejectJoin: (request, options) => generatedClient.rejectJoin(request, options),
    removeMachine: (request, options) => generatedClient.removeMachine(request, options),
    startJoin: (request, options) => generatedClient.startJoin(request, options),
    watchJoin: (request, options) => generatedClient.watchJoin(request, options),
  };
}
