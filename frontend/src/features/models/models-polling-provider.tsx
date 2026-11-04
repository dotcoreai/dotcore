import { type ReactNode, useCallback, useMemo, useState } from "react";

import { type ModelClient, modelClient } from "@/api/model-client";

import { ModelsPollingContext } from "./models-polling-context";
import { useModelsPolling } from "./use-models-polling";

/**
 * Owns the console's one view of the catalog models and live model deployments for every page
 * beneath it, so an overlay applied on one page shows on the next. It watches and reads only while
 * a page reading it is mounted.
 */
export function ModelsPollingProvider({
  children,
  client = modelClient,
}: {
  children: ReactNode;
  client?: ModelClient;
}) {
  const [readers, setReaders] = useState(0);
  const polling = useModelsPolling(client, { enabled: readers > 0 });
  const register = useCallback(() => {
    setReaders((count) => count + 1);
    return () => setReaders((count) => count - 1);
  }, []);
  const value = useMemo(() => ({ polling, register }), [polling, register]);

  return <ModelsPollingContext.Provider value={value}>{children}</ModelsPollingContext.Provider>;
}
