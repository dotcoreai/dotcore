import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { useDirtyFields } from "@/hooks/use-dirty-fields";

type Field = "name" | "url";

describe("useDirtyFields", () => {
  it("marks only changed fields dirty until reset", () => {
    const { result } = renderHook(() => useDirtyFields<Field>(true));

    expect(result.current.isDirty("name")).toBe(false);
    expect(result.current.isDirty("url")).toBe(false);

    act(() => result.current.markDirty("name"));
    expect(result.current.isDirty("name")).toBe(true);
    expect(result.current.isDirty("url")).toBe(false);

    act(() => result.current.markDirty("name"));
    expect(result.current.isDirty("name")).toBe(true);

    act(() => result.current.reset());
    expect(result.current.isDirty("name")).toBe(false);
    expect(result.current.isDirty("url")).toBe(false);
  });

  it("keeps dirty fields while closed and clears them when the form reopens", () => {
    const { result, rerender } = renderHook(({ open }) => useDirtyFields<Field>(open), {
      initialProps: { open: true },
    });

    act(() => result.current.markDirty("url"));
    rerender({ open: false });
    expect(result.current.isDirty("url")).toBe(true);

    rerender({ open: true });
    expect(result.current.isDirty("url")).toBe(false);
  });
});
