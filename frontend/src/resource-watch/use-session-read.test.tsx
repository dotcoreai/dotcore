import { act, render, waitFor } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import type { ReadState } from "@/lib/read-state";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import { useSessionRead } from "./use-session-read";

describe("useSessionRead", () => {
  it("keeps the value read when enabled again in the same session, without reading again", async () => {
    const read = vi.fn(() => Promise.resolve("catalog"));
    const { result, setEnabled } = renderRead(read);
    await waitFor(() => expect(result.current).toEqual({ failed: false, value: "catalog" }));

    setEnabled(false);
    setEnabled(true);

    expect(result.current).toEqual({ failed: false, value: "catalog" });
    expect(read).toHaveBeenCalledTimes(1);
  });

  it("reads again on enable after a failed read", async () => {
    const read = vi
      .fn<() => Promise<string>>()
      .mockRejectedValueOnce(new Error("down"))
      .mockResolvedValue("catalog");
    const { result, setEnabled } = renderRead(read);
    await waitFor(() => expect(result.current.failed).toBe(true));

    setEnabled(false);
    setEnabled(true);

    await waitFor(() => expect(result.current).toEqual({ failed: false, value: "catalog" }));
    expect(read).toHaveBeenCalledTimes(2);
  });
});

function renderRead(read: () => Promise<string>) {
  const result = {} as { current: ReadState<string> };
  const controls = {} as { setEnabled: (enabled: boolean) => void };
  const watch = new FakeResourceWatch({ sources: {} });

  function Probe({ enabled }: { enabled: boolean }) {
    result.current = useSessionRead(read, { enabled });
    return null;
  }
  function Host() {
    const [enabled, setEnabled] = useState(true);
    controls.setEnabled = setEnabled;
    return <Probe enabled={enabled} />;
  }
  act(() => {
    render(
      <ResourceWatchProvider client={watch.client()} reopenDelayMs={0}>
        <Host />
      </ResourceWatchProvider>,
    );
  });

  return {
    result,
    setEnabled: (enabled: boolean) => act(() => controls.setEnabled(enabled)),
  };
}
