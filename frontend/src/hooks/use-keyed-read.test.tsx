import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { type KeyedReadRequest, useKeyedRead } from "./use-keyed-read";

describe("useKeyedRead", () => {
  it("reports the value read for its key", async () => {
    const load = vi.fn(() => Promise.resolve("a-value"));
    const { result } = renderHook(() => useKeyedRead(request("a", load)));

    expect(result.current.pending).toBe(true);
    await waitFor(() => expect(result.current.pending).toBe(false));
    expect(load).toHaveBeenCalledOnce();
    expect(result.current.value).toBe("a-value");
    expect(result.current.error).toBeUndefined();
  });

  it("reports a failed read under its label, without a value", async () => {
    const load = vi.fn(() => Promise.reject(new Error("estimate down")));
    const { result } = renderHook(() => useKeyedRead(request("a", load)));

    await waitFor(() => expect(result.current.pending).toBe(false));
    expect(result.current.value).toBeUndefined();
    expect(result.current.error).toBe("Read unavailable: estimate down.");
  });

  it("keeps the previous key's value in view while the next key loads", async () => {
    const resolvers: Array<(value: string) => void> = [];
    const load = vi.fn(
      () =>
        new Promise<string>((resolve) => {
          resolvers.push(resolve);
        }),
    );
    const { rerender, result } = renderHook(({ key }) => useKeyedRead(request(key, load)), {
      initialProps: { key: "a" },
    });

    resolvers[0]("a-value");
    await waitFor(() => expect(result.current.value).toBe("a-value"));

    rerender({ key: "b" });
    expect(result.current.pending).toBe(true);
    expect(result.current.value).toBe("a-value");
    resolvers[1]("b-value");
    await waitFor(() => expect(result.current.value).toBe("b-value"));
    expect(result.current.pending).toBe(false);
  });

  it("asks once for a key while its read is in flight", async () => {
    const resolvers: Array<(value: string) => void> = [];
    const load = vi.fn(
      () =>
        new Promise<string>((resolve) => {
          resolvers.push(resolve);
        }),
    );
    const { rerender, result } = renderHook(() => useKeyedRead(request("a", load)));

    rerender();
    expect(load).toHaveBeenCalledOnce();
    resolvers[0]("a-value");
    await waitFor(() => expect(result.current.value).toBe("a-value"));
  });

  it("reads a settled key again for a new revision without becoming pending", async () => {
    const resolvers: Array<(value: string) => void> = [];
    const load = vi.fn(
      () =>
        new Promise<string>((resolve) => {
          resolvers.push(resolve);
        }),
    );
    const { rerender, result } = renderHook(
      ({ revision }) => useKeyedRead({ ...request("a", load), revision }),
      { initialProps: { revision: 0 } },
    );
    resolvers[0]("first");
    await waitFor(() => expect(result.current.value).toBe("first"));

    rerender({ revision: 1 });
    expect(load).toHaveBeenCalledTimes(2);
    expect(result.current).toMatchObject({ pending: false, value: "first" });
    resolvers[1]("second");
    await waitFor(() => expect(result.current.value).toBe("second"));
    expect(result.current.pending).toBe(false);
  });

  it("keeps the settled value when a refresh fails, marked until a refresh succeeds", async () => {
    let rejectRefresh: (failure: Error) => void = () => undefined;
    const refresh = new Promise<string>((_, reject) => {
      rejectRefresh = reject;
    });
    const load = vi
      .fn<() => Promise<string>>()
      .mockResolvedValueOnce("first")
      .mockReturnValueOnce(refresh)
      .mockResolvedValueOnce("second");
    const { rerender, result } = renderHook(
      ({ revision }) => useKeyedRead({ ...request("a", load), revision }),
      { initialProps: { revision: 0 } },
    );
    await waitFor(() => expect(result.current.value).toBe("first"));

    rerender({ revision: 1 });
    expect(load).toHaveBeenCalledTimes(2);
    await act(async () => {
      rejectRefresh(new Error("estimate down"));
      await refresh.catch(() => undefined);
    });
    expect(result.current).toMatchObject({
      error: undefined,
      pending: false,
      refreshFailed: true,
      value: "first",
    });

    rerender({ revision: 2 });
    await waitFor(() => expect(result.current.value).toBe("second"));
    expect(result.current.refreshFailed).toBe(false);
  });

  it("asks once when the revision changes while the key's first read is in flight", async () => {
    const resolvers: Array<(value: string) => void> = [];
    const load = vi.fn(
      () =>
        new Promise<string>((resolve) => {
          resolvers.push(resolve);
        }),
    );
    const { rerender, result } = renderHook(
      ({ revision }) => useKeyedRead({ ...request("a", load), revision }),
      { initialProps: { revision: 0 } },
    );

    rerender({ revision: 1 });
    expect(load).toHaveBeenCalledOnce();
    resolvers[0]("first");
    await waitFor(() => expect(result.current.value).toBe("first"));
  });

  it("reads nothing without a request", () => {
    const { result } = renderHook(() => useKeyedRead<string>(undefined));

    expect(result.current).toEqual({
      error: undefined,
      loadingVisible: false,
      pending: false,
      refreshFailed: false,
      value: undefined,
    });
  });
});

function request(key: string, load: () => Promise<string>): KeyedReadRequest<string> {
  return { failureLabel: "Read unavailable", key, load };
}
