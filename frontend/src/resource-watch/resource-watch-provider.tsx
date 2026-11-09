import { type ReactNode, useEffect, useState } from "react";

import type { ResourceWatchClient } from "@/api/resource-watch-client";
import { ResourceWatchContext } from "@/resource-watch/resource-watch";
import { ResourceWatchStore } from "@/resource-watch/resource-watch-store";

export interface ResourceWatchProviderProps {
  children: ReactNode;
  /** Read once at mount. */
  client: ResourceWatchClient;
  /** Wait before reopening the stream after the watched kinds change; read once at mount. */
  reopenDelayMs?: number;
}

/**
 * Owns the console's one resource watch stream, shared by every resource hook below it. The
 * stream opens once a component watches a kind and closes when none does.
 */
export function ResourceWatchProvider({
  children,
  client,
  reopenDelayMs,
}: ResourceWatchProviderProps) {
  const [store] = useState(() => new ResourceWatchStore(client, reopenDelayMs));

  useEffect(() => () => store.close(), [store]);

  return <ResourceWatchContext value={store}>{children}</ResourceWatchContext>;
}
