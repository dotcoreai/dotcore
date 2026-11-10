import type { ReactNode } from "react";

import { ClientsContext, type ConsoleClients } from "@/api/clients";

/** Supplies the console's API clients to every page and provider beneath it. */
export function ClientsProvider({
  children,
  clients,
}: {
  children: ReactNode;
  clients: ConsoleClients;
}) {
  return <ClientsContext.Provider value={clients}>{children}</ClientsContext.Provider>;
}
