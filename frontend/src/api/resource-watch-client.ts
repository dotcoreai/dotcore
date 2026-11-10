import { type Client, createClient } from "@connectrpc/connect";

import { createConsoleTransport } from "@/api/console-transport";
import { resolveManagementOrigin } from "@/api/management-origin";
import { ResourceWatchService } from "@/gen/dotcore/watch/v1/service_pb";

export type ResourceWatchClient = Pick<Client<typeof ResourceWatchService>, "watchResources">;

const resourceWatchServerUrl = resolveManagementOrigin(
  import.meta.env.VITE_DOTCORE_MANAGEMENT_ORIGIN,
  {
    allowTestFallback: import.meta.env.MODE === "test",
    allowViteFallback: import.meta.env.DEV || import.meta.env.MODE === "test",
  },
);

export function createResourceWatchClient(baseUrl = resourceWatchServerUrl): ResourceWatchClient {
  const generatedClient = createClient(ResourceWatchService, createConsoleTransport(baseUrl));

  return { watchResources: generatedClient.watchResources };
}
