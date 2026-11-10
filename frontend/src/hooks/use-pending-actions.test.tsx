import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { usePendingActions } from "@/hooks/use-pending-actions";

type Action = "pause" | "resume" | "remove";

describe("usePendingActions", () => {
  it("refuses to start an action already running for the same resource", () => {
    const { result } = renderHook(() => usePendingActions<Action>());

    let first: boolean[] = [];
    act(() => {
      first = [
        result.current.start("a", "pause"),
        result.current.start("a", "pause"),
        result.current.start("a", "resume"),
        result.current.start("b", "pause"),
      ];
    });

    expect(first).toEqual([true, false, true, true]);
  });

  it("clears a finished action so it can start again", () => {
    const { result } = renderHook(() => usePendingActions<Action>());

    act(() => {
      result.current.start("a", "pause");
    });
    expect(result.current.isRunning("a", "pause")).toBe(true);
    expect(result.current.isPending("a")).toBe(true);
    expect(result.current.isPending("a", "pause")).toBe(true);
    expect(result.current.isPending("a", "resume")).toBe(false);
    expect(result.current.isPending("b")).toBe(false);

    act(() => result.current.finish("a", "pause"));
    expect(result.current.isRunning("a", "pause")).toBe(false);
    expect(result.current.isPending("a")).toBe(false);

    let restarted = false;
    act(() => {
      restarted = result.current.start("a", "pause");
    });
    expect(restarted).toBe(true);
  });

  it("answers isRunning before the next render", () => {
    const { result } = renderHook(() => usePendingActions<Action>());

    act(() => {
      result.current.start("a", "remove");
      expect(result.current.isRunning("a", "remove")).toBe(true);
    });
  });

  it("returns the first running action in the given order", () => {
    const { result } = renderHook(() => usePendingActions<Action>());
    const order: readonly Action[] = ["pause", "resume", "remove"];

    expect(result.current.pendingAction("a", order)).toBeUndefined();

    act(() => {
      result.current.start("a", "remove");
      result.current.start("a", "resume");
      result.current.start("b", "pause");
    });
    expect(result.current.pendingAction("a", order)).toBe("resume");
    expect(result.current.pendingAction("a", ["remove", "resume"])).toBe("remove");

    act(() => result.current.finish("a", "resume"));
    expect(result.current.pendingAction("a", order)).toBe("remove");
  });

  it("does not mix up resource IDs that start with one another", () => {
    const { result } = renderHook(() => usePendingActions<Action>());

    act(() => {
      result.current.start("a:1", "pause");
    });
    expect(result.current.isPending("a")).toBe(false);
    expect(result.current.isPending("a:1")).toBe(true);
  });
});
