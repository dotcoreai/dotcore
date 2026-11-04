import { act, renderHook } from "@testing-library/react";
import { type ReactNode, StrictMode, Suspense, use, useLayoutEffect } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  LOADING_MINIMUM_VISIBLE_MS,
  LOADING_REVEAL_DELAY_MS,
  useDelayedLoading,
} from "@/hooks/use-delayed-loading";

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
});

describe("useDelayedLoading", () => {
  it("reveals one aggregate pending interval at the configured delay", async () => {
    const { result } = renderHook(() => useDelayedLoading(true));

    expect(result.current).toBe(false);

    await advanceBy(LOADING_REVEAL_DELAY_MS - 1);
    expect(result.current).toBe(false);

    await advanceBy(1);
    expect(result.current).toBe(true);
  });

  it("cancels feedback when loading finishes before the reveal delay", async () => {
    const { result, rerender } = renderHook(
      ({ loading }: { loading: boolean }) => useDelayedLoading(loading),
      { initialProps: { loading: true } },
    );

    await advanceBy(LOADING_REVEAL_DELAY_MS - 1);
    rerender({ loading: false });
    await advanceBy(LOADING_REVEAL_DELAY_MS);

    expect(result.current).toBe(false);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("cancels obsolete reveal before completion layout advances the actual scheduler", () => {
    const commits: [boolean, boolean][] = [];
    const { result, rerender } = renderHook(
      ({ loading }: { loading: boolean }) => {
        const visible = useDelayedLoading(loading);
        useLayoutEffect(() => {
          commits.push([loading, visible]);
          if (!loading) vi.advanceTimersByTime(LOADING_REVEAL_DELAY_MS);
        }, [loading, visible]);
        return visible;
      },
      { initialProps: { loading: true } },
    );

    rerender({ loading: false });

    expect(commits).toEqual([
      [true, false],
      [false, false],
    ]);
    expect(result.current).toBe(false);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("cancels obsolete hide before resumed-loading layout advances the actual scheduler", async () => {
    const commits: [boolean, boolean][] = [];
    const { result, rerender } = renderHook(
      ({ loading, resume }: { loading: boolean; resume: boolean }) => {
        const visible = useDelayedLoading(loading);
        useLayoutEffect(() => {
          commits.push([loading, visible]);
          if (resume) vi.advanceTimersByTime(LOADING_MINIMUM_VISIBLE_MS);
        }, [loading, resume, visible]);
        return visible;
      },
      { initialProps: { loading: true, resume: false } },
    );
    await advanceBy(LOADING_REVEAL_DELAY_MS);
    rerender({ loading: false, resume: false });
    rerender({ loading: true, resume: true });

    expect(commits).toEqual([
      [true, false],
      [true, true],
      [false, true],
      [true, true],
    ]);
    expect(result.current).toBe(true);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("retains revealed feedback for the configured minimum", async () => {
    const { result, rerender } = renderHook(
      ({ loading }: { loading: boolean }) => useDelayedLoading(loading),
      { initialProps: { loading: true } },
    );

    await advanceBy(LOADING_REVEAL_DELAY_MS);
    await advanceBy(50);
    rerender({ loading: false });

    await advanceBy(LOADING_MINIMUM_VISIBLE_MS - 51);
    expect(result.current).toBe(true);

    await advanceBy(1);
    expect(result.current).toBe(false);
  });
  it("measures minimum visibility from the committed visible render", async () => {
    const blockedCommit = new Promise<void>(() => undefined);
    const wrapper = ({ children }: { children: ReactNode }) => (
      <Suspense fallback={null}>{children}</Suspense>
    );
    const { result, rerender } = renderHook(
      ({ blockCommit, loading }: { blockCommit: boolean; loading: boolean }) => {
        const visible = useDelayedLoading(loading);

        if (blockCommit && visible) {
          use(blockedCommit);
        }

        return visible;
      },
      { initialProps: { blockCommit: true, loading: true }, wrapper },
    );

    await advanceBy(LOADING_REVEAL_DELAY_MS);
    expect(result.current).toBe(false);

    await advanceBy(150);
    rerender({ blockCommit: false, loading: false });
    expect(result.current).toBe(true);

    await advanceBy(LOADING_MINIMUM_VISIBLE_MS - 1);
    expect(result.current).toBe(true);

    await advanceBy(1);
    expect(result.current).toBe(false);
  });

  it("hides immediately when loading finishes after the minimum", async () => {
    const { result, rerender } = renderHook(
      ({ loading }: { loading: boolean }) => useDelayedLoading(loading),
      { initialProps: { loading: true } },
    );

    await advanceBy(LOADING_REVEAL_DELAY_MS);
    await advanceBy(LOADING_MINIMUM_VISIBLE_MS);
    expect(result.current).toBe(true);

    rerender({ loading: false });
    expect(result.current).toBe(false);
  });

  it("does not restart the reveal timer on a true-to-true replacement", async () => {
    const { result, rerender } = renderHook(
      ({ loading, request }: { loading: boolean; request: number }) => {
        void request;
        return useDelayedLoading(loading);
      },
      { initialProps: { loading: true, request: 1 } },
    );

    await advanceBy(LOADING_REVEAL_DELAY_MS - 1);
    rerender({ loading: true, request: 2 });
    await advanceBy(1);

    expect(result.current).toBe(true);
  });

  it("cancels a pending hide when loading resumes", async () => {
    const { result, rerender } = renderHook(
      ({ loading }: { loading: boolean }) => useDelayedLoading(loading),
      { initialProps: { loading: true } },
    );

    await advanceBy(LOADING_REVEAL_DELAY_MS + 50);
    rerender({ loading: false });
    await advanceBy(50);
    rerender({ loading: true });
    await advanceBy(LOADING_MINIMUM_VISIBLE_MS);

    expect(result.current).toBe(true);

    rerender({ loading: false });
    expect(result.current).toBe(false);
  });

  it("starts a fresh reveal delay after feedback has hidden", async () => {
    const { result, rerender } = renderHook(
      ({ loading }: { loading: boolean }) => useDelayedLoading(loading),
      { initialProps: { loading: true } },
    );

    await advanceBy(LOADING_REVEAL_DELAY_MS);
    await advanceBy(LOADING_MINIMUM_VISIBLE_MS);
    rerender({ loading: false });
    expect(result.current).toBe(false);

    rerender({ loading: true });
    await advanceBy(LOADING_REVEAL_DELAY_MS - 1);
    expect(result.current).toBe(false);

    await advanceBy(1);
    expect(result.current).toBe(true);
  });

  it("owns one timer under StrictMode and clears it on unmount", () => {
    const wrapper = ({ children }: { children: ReactNode }) => <StrictMode>{children}</StrictMode>;
    const { unmount } = renderHook(() => useDelayedLoading(true), { wrapper });

    expect(vi.getTimerCount()).toBe(1);

    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });
});

async function advanceBy(milliseconds: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(milliseconds);
  });
}
