import { Outlet } from "react-router-dom";

import { useClients } from "@/api/clients";

import { ApplicationsPollingProvider } from "./applications-polling-provider";

/** Layout route for the Apps section pages, which share one applications polling instance. */
export function ApplicationsSection() {
  const { applicationClient } = useClients();

  return (
    <ApplicationsPollingProvider client={applicationClient}>
      <Outlet />
    </ApplicationsPollingProvider>
  );
}
