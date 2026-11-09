import { render, type RenderOptions } from "@testing-library/react";
import type { ReactElement, ReactNode } from "react";

import type { ConsoleClients } from "@/api/clients";
import { ClientsProvider } from "@/api/clients-provider";

/**
 * Renders a test tree inside a clients provider. The test passes only the clients it uses; every
 * other client is a fake that throws when called, so an unexpected request fails the test loudly.
 */
export function renderWithClients(
  ui: ReactElement,
  clients: Partial<ConsoleClients> = {},
  options?: Omit<RenderOptions, "wrapper">,
) {
  return render(ui, { ...options, wrapper: clientsWrapper(clients) });
}

/** A wrapper component providing test clients, for `renderHook` and custom render helpers. */
export function clientsWrapper(clients: Partial<ConsoleClients> = {}) {
  const value = testClients(clients);
  return function TestClientsProvider({ children }: { children: ReactNode }) {
    return <ClientsProvider clients={value}>{children}</ClientsProvider>;
  };
}

/** The given clients, with a fake that throws when called for every client not given. */
export function testClients(clients: Partial<ConsoleClients> = {}): ConsoleClients {
  return {
    applicationClient: clients.applicationClient ?? unfakedClient("applicationClient"),
    chatClient: clients.chatClient ?? unfakedClient("chatClient"),
    decisionClient: clients.decisionClient ?? unfakedClient("decisionClient"),
    diagnosticLogOperations:
      clients.diagnosticLogOperations ?? unfakedClient("diagnosticLogOperations"),
    metricsClient: clients.metricsClient ?? unfakedClient("metricsClient"),
    modelClient: clients.modelClient ?? unfakedClient("modelClient"),
    resourceWatchClient: clients.resourceWatchClient ?? unfakedClient("resourceWatchClient"),
    routeClient: clients.routeClient ?? unfakedClient("routeClient"),
    siteClient: clients.siteClient ?? unfakedClient("siteClient"),
  };
}

// Any method reached through the fake throws, naming the client path the test forgot to fake.
function unfakedClient<T>(path: string): T {
  return new Proxy(() => undefined, {
    apply() {
      throw new Error(`${path} is not faked in this test: pass it to the test clients.`);
    },
    get(_target, property) {
      // A thenable check or a symbol lookup must not look like a client call.
      if (typeof property === "symbol" || property === "then") return undefined;
      return unfakedClient<unknown>(`${path}.${property}`);
    },
  }) as T;
}
