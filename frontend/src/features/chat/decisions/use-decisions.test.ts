import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { DecisionClient, DecisionResult } from "@/api/decision-client";
import { createDecisionStore } from "@/features/chat/decisions/decision-store";
import { useDecisions } from "@/features/chat/decisions/use-decisions";
import { MemoryStorage } from "@/test/memory-storage";

const answered: DecisionResult = {
  kind: "answered",
  responseText: "{}",
  answers: new Map([["refund", { type: "noul", yesProbability: 0.8 }]]),
  latencyMs: 1,
};

describe("useDecisions", () => {
  it("drops an Ask sent for a route that is no longer selected", async () => {
    const client: DecisionClient = { ask: () => Promise.resolve(answered) };
    const store = createDecisionStore(new MemoryStorage());
    const { rerender, result } = renderHook(({ route }) => useDecisions(client, route, store), {
      initialProps: { route: "clef" },
    });
    const askOnClef = result.current.ask;

    rerender({ route: "clef-q8" });
    // An async act flushes the state updates of the answered Ask.
    await act(() => {
      askOnClef();
      return Promise.resolve();
    });

    expect(result.current.run).toBeUndefined();
  });
});
