import { createContext, useContext } from "react";

import type { useApplicationsPolling } from "./use-applications-polling";

/** The Apps section's one applications polling instance, shared by the Apps and Catalog pages. */
export type ApplicationsPolling = ReturnType<typeof useApplicationsPolling>;

export const ApplicationsPollingContext = createContext<ApplicationsPolling | undefined>(undefined);

/**
 * Reads the Apps section's shared applications polling. A deployment overlay applied on one page
 * stays visible on the other, so a deploy from the catalog lands on a deployment panel that already
 * shows the accepted application deployment.
 */
export function useSharedApplicationsPolling(): ApplicationsPolling {
  const polling = useContext(ApplicationsPollingContext);
  if (!polling)
    throw new Error(
      "useSharedApplicationsPolling must be used inside ApplicationsPollingProvider.",
    );
  return polling;
}
