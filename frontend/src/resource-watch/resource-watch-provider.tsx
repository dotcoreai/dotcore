import { type ReactNode, useEffect, useState } from "react";

import type { ResourceWatchClient } from "@/api/resource-watch-client";
import { ResourceWatchContext } from "@/resource-watch/resource-watch";
import { ResourceWatchStore } from "@/resource-watch/resource-watch-store";

export interface ResourceWatchProviderProps {
  children: ReactNode;
  /** Read once at mount. */
  client: ResourceWatchClient;
}

/**
 * Owns the console's one resource watch stream, shared by every resource hook below it. The
 * stream opens once a component watches a kind and closes when none does.
 */
export function ResourceWatchProvider({ children, client }: ResourceWatchProviderProps) {
  const [store] = useState(() => new ResourceWatchStore(client));

  useEffect(() => () => store.close(), [store]);

  return <ResourceWatchContext value={store}>{children}</ResourceWatchContext>;
}
