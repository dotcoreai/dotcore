import { createContext, useContext } from "react";

import { type ApplicationClient, createApplicationClient } from "@/api/application-client";
import { type ChatClient, createChatClient } from "@/api/chat-client";
import { createDecisionClient, type DecisionClient } from "@/api/decision-client";
import {
  createDiagnosticLogOperations,
  type DiagnosticLogOperations,
} from "@/api/diagnostic-log-client";
import { createMetricsClient, type MetricsClient } from "@/api/metrics-client";
import { createModelClient, type ModelClient } from "@/api/model-client";
import { createResourceWatchClient, type ResourceWatchClient } from "@/api/resource-watch-client";
import { createRouteClient, type RouteClient } from "@/api/route-client";
import { createSiteClient, type SiteClient } from "@/api/site-client";

/**
 * Every API client the console uses, one per backend service. Pages and providers read them from
 * the clients context, so the app supplies the production clients once and tests supply fakes.
 */
export interface ConsoleClients {
  applicationClient: ApplicationClient;
  chatClient: ChatClient;
  decisionClient: DecisionClient;
  diagnosticLogOperations: DiagnosticLogOperations;
  metricsClient: MetricsClient;
  modelClient: ModelClient;
  resourceWatchClient: ResourceWatchClient;
  routeClient: RouteClient;
  siteClient: SiteClient;
}

export const ClientsContext = createContext<ConsoleClients | undefined>(undefined);

/** Creates the production clients, each talking to the backend the console was served with. */
export function createConsoleClients(): ConsoleClients {
  return {
    applicationClient: createApplicationClient(),
    chatClient: createChatClient(),
    decisionClient: createDecisionClient(),
    diagnosticLogOperations: createDiagnosticLogOperations(),
    metricsClient: createMetricsClient(),
    modelClient: createModelClient(),
    resourceWatchClient: createResourceWatchClient(),
    routeClient: createRouteClient(),
    siteClient: createSiteClient(),
  };
}

/** Reads the console's API clients from the nearest clients provider. */
export function useClients(): ConsoleClients {
  const clients = useContext(ClientsContext);
  if (!clients) throw new Error("useClients must be used inside ClientsProvider.");
  return clients;
}
