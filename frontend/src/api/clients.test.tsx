import { renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { useClients } from "@/api/clients";
import type { MetricsClient } from "@/api/metrics-client";
import { clientsWrapper } from "@/test/render-with-clients";

describe("useClients", () => {
  it("throws outside a clients provider", () => {
    expect(() => renderHook(() => useClients())).toThrow(
      "useClients must be used inside ClientsProvider.",
    );
  });

  it("returns the given clients and fails loudly on the others", () => {
    const metricsClient = { metrics: {} } as MetricsClient;
    const { result } = renderHook(() => useClients(), {
      wrapper: clientsWrapper({ metricsClient }),
    });

    expect(result.current.metricsClient).toBe(metricsClient);
    expect(() => result.current.siteClient.listConfiguredExecutionTargets({})).toThrow(
      "siteClient.listConfiguredExecutionTargets is not faked in this test: pass it to the test clients.",
    );
  });
});
