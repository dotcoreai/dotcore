import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { useAdvancedMode } from "./use-advanced-mode";

describe("useAdvancedMode", () => {
  it("starts in basic mode and announces toggles with the step count", () => {
    const { result } = renderHook(() => useAdvancedMode(true));
    expect(result.current.advanced).toBe(false);
    expect(result.current.announcement).toBe("");

    act(() => result.current.toggle(true, 3));
    expect(result.current.advanced).toBe(true);
    expect(result.current.announcement).toBe("Advanced on: 3 steps");

    act(() => result.current.toggle(false, 1));
    expect(result.current.advanced).toBe(false);
    expect(result.current.announcement).toBe("Advanced off: 1 step");
  });

  it("turns on with a custom announcement", () => {
    const { result } = renderHook(() => useAdvancedMode(true));

    act(() => result.current.enable("Advanced on to fix routing settings"));
    expect(result.current.advanced).toBe(true);
    expect(result.current.announcement).toBe("Advanced on to fix routing settings");
  });

  it("returns to basic mode when the dialog reopens", () => {
    const { result, rerender } = renderHook(({ open }) => useAdvancedMode(open), {
      initialProps: { open: true },
    });
    act(() => result.current.toggle(true, 4));

    rerender({ open: false });
    expect(result.current.advanced).toBe(true);

    rerender({ open: true });
    expect(result.current.advanced).toBe(false);
    expect(result.current.announcement).toBe("");
  });
});
