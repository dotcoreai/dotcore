import type { ReactNode } from "react";

import { type ApplicationClient, applicationClient } from "@/api/application-client";

import { ApplicationsPollingContext } from "./applications-polling-context";
import { useApplicationsPolling } from "./use-applications-polling";

/**
 * Owns the Apps section's one view of the catalog applications and live application deployments
 * for every page beneath it, so an overlay applied on one page shows on the other.
 */
export function ApplicationsPollingProvider({
  children,
  client = applicationClient,
}: {
  children: ReactNode;
  client?: ApplicationClient;
}) {
  const polling = useApplicationsPolling(client);

  return (
    <ApplicationsPollingContext.Provider value={polling}>
      {children}
    </ApplicationsPollingContext.Provider>
  );
}
