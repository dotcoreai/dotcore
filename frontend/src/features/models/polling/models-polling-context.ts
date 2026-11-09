import { createContext, useContext, useEffect } from "react";

import type { useModelsPolling } from "./use-models-polling";

/** The console's one models polling instance, shared by every page that reads models. */
export type ModelsPolling = ReturnType<typeof useModelsPolling>;

/** The shared polling, and the registration of a reader that keeps it watching while mounted. */
export interface ModelsPollingContextValue {
  polling: ModelsPolling;
  register: () => () => void;
}

export const ModelsPollingContext = createContext<ModelsPollingContextValue | undefined>(undefined);

/**
 * Reads the console's shared models polling, keeping it watching while mounted. A deployment
 * overlay applied on one page stays visible on the next, so a deploy from the catalog lands on a
 * deployment panel that already shows the accepted deployment.
 */
export function useSharedModelsPolling(): ModelsPolling {
  const shared = useContext(ModelsPollingContext);
  if (!shared) throw new Error("useSharedModelsPolling must be used inside ModelsPollingProvider.");

  const { register } = shared;
  useEffect(() => register(), [register]);

  return shared.polling;
}
