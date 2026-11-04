import { Code, ConnectError } from "@connectrpc/connect";
import { afterEach, describe, expect, it, vi } from "vitest";

import { createLatencyInterceptor } from "@/api/console-transport";
import type { DeveloperSettings } from "@/lib/console-preferences";

const enabled: DeveloperSettings = { latencyEnabled: true, latencyMinMs: 200, latencyMaxMs: 1_000 };

function invoke(
  interceptor: ReturnType<typeof createLatencyInterceptor>,
  next: (request: unknown) => Promise<unknown>,
  { signal = new AbortController().signal, stream = false } = {},
) {
  const request = { signal, stream };

  return interceptor(next as never)(request as never);
}

describe("latency interceptor", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("dispatches without waiting while simulation is disabled", async () => {
    const sleep = vi.fn();
    const next = vi.fn(() => Promise.resolve("response"));
    const interceptor = createLatencyInterceptor({
      readSettings: () => ({ ...enabled, latencyEnabled: false }),
      sleep,
    });

    await expect(invoke(interceptor, next)).resolves.toBe("response");

    expect(sleep).not.toHaveBeenCalled();
    expect(next).toHaveBeenCalledTimes(1);
  });

  it.each([
    [0, 200],
    [0.5, 600],
    [0.999999, 1_000],
  ])("draws random %s as a %i ms delay within the range", async (random, expected) => {
    const sleep = vi.fn<(delay: number, signal: AbortSignal) => Promise<void>>(() =>
      Promise.resolve(),
    );
    const interceptor = createLatencyInterceptor({
      random: () => random,
      readSettings: () => enabled,
      sleep,
    });

    await invoke(interceptor, () => Promise.resolve("response"));

    expect(sleep).toHaveBeenCalledWith(expected, expect.any(AbortSignal));
  });

  it("waits exactly the bound when minimum equals maximum", async () => {
    const sleep = vi.fn<(delay: number, signal: AbortSignal) => Promise<void>>(() =>
      Promise.resolve(),
    );
    const interceptor = createLatencyInterceptor({
      random: () => 0.7,
      readSettings: () => ({ latencyEnabled: true, latencyMinMs: 350, latencyMaxMs: 350 }),
      sleep,
    });

    await invoke(interceptor, () => Promise.resolve("response"));

    expect(sleep).toHaveBeenCalledWith(350, expect.any(AbortSignal));
  });

  it("holds the request until the delay elapses, then dispatches it once", async () => {
    vi.useFakeTimers();
    const next = vi.fn(() => Promise.resolve("response"));
    const interceptor = createLatencyInterceptor({ random: () => 0, readSettings: () => enabled });

    const pending = invoke(interceptor, next);
    await vi.advanceTimersByTimeAsync(199);
    expect(next).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(1);
    await expect(pending).resolves.toBe("response");
    expect(next).toHaveBeenCalledTimes(1);
  });

  it("reads settings on every call", async () => {
    const sleep = vi.fn<(delay: number, signal: AbortSignal) => Promise<void>>(() =>
      Promise.resolve(),
    );
    let settings = enabled;
    const interceptor = createLatencyInterceptor({
      random: () => 0,
      readSettings: () => settings,
      sleep,
    });

    await invoke(interceptor, () => Promise.resolve("first"));
    settings = { ...enabled, latencyEnabled: false };
    await invoke(interceptor, () => Promise.resolve("second"));
    settings = { latencyEnabled: true, latencyMinMs: 5, latencyMaxMs: 5 };
    await invoke(interceptor, () => Promise.resolve("third"));

    expect(sleep.mock.calls.map(([delay]) => delay)).toEqual([200, 5]);
  });

  it("rejects with Canceled when the caller aborts during the delay, without dispatching", async () => {
    vi.useFakeTimers();
    const controller = new AbortController();
    const next = vi.fn(() => Promise.resolve("response"));
    const interceptor = createLatencyInterceptor({ random: () => 0, readSettings: () => enabled });

    const pending = invoke(interceptor, next, { signal: controller.signal });
    const rejection = expect(pending).rejects.toMatchObject({ code: Code.Canceled });
    controller.abort();

    await rejection;
    expect(next).not.toHaveBeenCalled();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("rejects with DeadlineExceeded when a timeout aborts during the delay", async () => {
    vi.useFakeTimers();
    const controller = new AbortController();
    const next = vi.fn(() => Promise.resolve("response"));
    const interceptor = createLatencyInterceptor({ random: () => 0, readSettings: () => enabled });

    const pending = invoke(interceptor, next, { signal: controller.signal });
    const rejection = expect(pending).rejects.toMatchObject({ code: Code.DeadlineExceeded });
    controller.abort(new ConnectError("the operation timed out", Code.DeadlineExceeded));

    await rejection;
    expect(next).not.toHaveBeenCalled();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("rejects immediately for an already aborted request", async () => {
    vi.useFakeTimers();
    const controller = new AbortController();
    controller.abort();
    const next = vi.fn(() => Promise.resolve("response"));
    const interceptor = createLatencyInterceptor({ random: () => 0, readSettings: () => enabled });

    await expect(invoke(interceptor, next, { signal: controller.signal })).rejects.toMatchObject({
      code: Code.Canceled,
    });

    expect(next).not.toHaveBeenCalled();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("delays a server stream once and hands back its messages untouched", async () => {
    const sleep = vi.fn<(delay: number, signal: AbortSignal) => Promise<void>>(() =>
      Promise.resolve(),
    );
    const message = (async function* () {
      await Promise.resolve();
      yield "first";
      yield "second";
    })();
    const response = { stream: true, message };
    const next = vi.fn(() => Promise.resolve(response));
    const interceptor = createLatencyInterceptor({
      random: () => 0,
      readSettings: () => enabled,
      sleep,
    });

    const result = (await invoke(interceptor, next, {
      stream: true,
    })) as unknown as typeof response;

    expect(sleep).toHaveBeenCalledTimes(1);
    expect(result).toBe(response);
    expect(result.message).toBe(message);
    const received: string[] = [];
    for await (const item of result.message) received.push(item);
    expect(received).toEqual(["first", "second"]);
    expect(sleep).toHaveBeenCalledTimes(1);
  });
});
