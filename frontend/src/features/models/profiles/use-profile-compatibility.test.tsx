import { create } from "@bufbuild/protobuf";
import { act, fireEvent, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  ListModelProfileCompatibilityResponseSchema,
  ModelCompatibilityStatus,
} from "@/gen/dotcore/model/v1/service_pb";

import { profileEstimateKey } from "./model-row";
import { profileTargetVerdict } from "./profile-compatibility";
import { profileSizing } from "./profile-compatibility-test-support";
import type { SizingScenario } from "./sizing-scenario";
import {
  PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS,
  type ProfileCompatibilityLoader,
  useProfileCompatibility,
} from "./use-profile-compatibility";

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

const gpu = { executionTargetId: "gpu", siteNodeId: "node" };

describe("useProfileCompatibility", () => {
  it("reads the model while active and re-reads it on the refresh interval", async () => {
    vi.useFakeTimers();
    const loader = vi.fn<ProfileCompatibilityLoader>((modelId) =>
      Promise.resolve(response(modelId, ModelCompatibilityStatus.RUNS)),
    );
    const { result } = renderHook(() => useProfileCompatibility(loader, "alpha", true));

    expect(result.current).toEqual({
      compatibility: undefined,
      error: undefined,
      estimates: { estimates: new Map(), pending: true },
      loading: true,
      sizingFailed: false,
    });
    await act(async () => {});
    expect(result.current.loading).toBe(false);
    expect(status(result.current.compatibility, "alpha")).toBe("runs");
    expect(result.current.estimates).toEqual({
      estimates: new Map([
        [
          profileEstimateKey("alpha", "fast"),
          [
            {
              acceleratorMemoryBytes: 8_000_000_000n,
              hardwareClassId: "nvidia",
              hostMemoryBytes: undefined,
            },
          ],
        ],
      ]),
      pending: false,
    });
    expect(loader).toHaveBeenCalledExactlyOnceWith("alpha");

    loader.mockResolvedValueOnce(response("alpha", ModelCompatibilityStatus.CANNOT_RUN));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    });
    expect(loader).toHaveBeenCalledTimes(2);
    expect(status(result.current.compatibility, "alpha")).toBe("cannot-run");
  });

  it("keeps the last good read when a refresh fails", async () => {
    vi.useFakeTimers();
    const loader = vi.fn<ProfileCompatibilityLoader>((modelId) =>
      Promise.resolve(response(modelId, ModelCompatibilityStatus.RUNS)),
    );
    const { result } = renderHook(() => useProfileCompatibility(loader, "alpha", true));
    await act(async () => {});

    loader.mockRejectedValueOnce(new Error("unavailable"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    });

    expect(loader).toHaveBeenCalledTimes(2);
    expect(result.current.error).toBeUndefined();
    expect(status(result.current.compatibility, "alpha")).toBe("runs");
  });

  it("reports a failed first read, then recovers on the next one", async () => {
    vi.useFakeTimers();
    const loader = vi
      .fn<ProfileCompatibilityLoader>()
      .mockRejectedValueOnce(new Error("compatibility unavailable"))
      .mockResolvedValue(response("alpha", ModelCompatibilityStatus.RUNS));
    const { result } = renderHook(() => useProfileCompatibility(loader, "alpha", true));
    await act(async () => {});

    expect(result.current).toEqual({
      compatibility: undefined,
      error: "compatibility unavailable",
      estimates: { estimates: new Map(), pending: false },
      loading: false,
      sizingFailed: false,
    });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    });
    expect(result.current.error).toBeUndefined();
    expect(status(result.current.compatibility, "alpha")).toBe("runs");
  });

  it("keeps the newest read when an older one answers after it", async () => {
    let visibility: DocumentVisibilityState = "visible";
    vi.spyOn(document, "visibilityState", "get").mockImplementation(() => visibility);
    const answers: ((status: ModelCompatibilityStatus) => void)[] = [];
    const loader = vi.fn<ProfileCompatibilityLoader>(
      (modelId) =>
        new Promise((resolve) => answers.push((status) => resolve(response(modelId, status)))),
    );
    const { result } = renderHook(() => useProfileCompatibility(loader, "alpha", true));

    visibility = "hidden";
    fireEvent(document, new Event("visibilitychange"));
    visibility = "visible";
    fireEvent(document, new Event("visibilitychange"));
    expect(loader).toHaveBeenCalledTimes(2);

    await act(async () => {
      answers[1](ModelCompatibilityStatus.RUNS);
      await Promise.resolve();
    });
    await act(async () => {
      answers[0](ModelCompatibilityStatus.CANNOT_RUN);
      await Promise.resolve();
    });
    expect(status(result.current.compatibility, "alpha")).toBe("runs");
  });

  it("drops the previous model's read when the model changes", async () => {
    const loader = vi.fn<ProfileCompatibilityLoader>((modelId) =>
      Promise.resolve(response(modelId, ModelCompatibilityStatus.RUNS)),
    );
    const { rerender, result } = renderHook(
      ({ modelId }) => useProfileCompatibility(loader, modelId, true),
      { initialProps: { modelId: "alpha" } },
    );
    await act(async () => {});

    rerender({ modelId: "beta" });
    expect(result.current).toEqual({
      compatibility: undefined,
      error: undefined,
      estimates: { estimates: new Map(), pending: true },
      loading: true,
      sizingFailed: false,
    });
    await act(async () => {});
    expect(status(result.current.compatibility, "beta")).toBe("runs");
    expect(loader.mock.calls).toEqual([["alpha"], ["beta"]]);
  });

  it("reads a new scenario at once, keeping the last read and estimates pending until it lands", async () => {
    const answers: ((bytes: bigint) => void)[] = [];
    const loader = vi.fn<ProfileCompatibilityLoader>(
      (modelId) =>
        new Promise((resolve) =>
          answers.push((bytes) => resolve(response(modelId, ModelCompatibilityStatus.RUNS, bytes))),
        ),
    );
    const { rerender, result } = renderHook(
      ({ scenario }) => useProfileCompatibility(loader, "alpha", true, scenario),
      { initialProps: { scenario: undefined as SizingScenario | undefined } },
    );
    await act(async () => {
      answers[0](8_000_000_000n);
      await Promise.resolve();
    });
    const defaults = result.current.compatibility;

    rerender({ scenario: { contextTokensPerRequest: 32768, totalContextTokens: 131072 } });
    expect(loader.mock.calls).toEqual([
      ["alpha"],
      ["alpha", { contextTokensPerRequest: 32768, totalContextTokens: 131072 }],
    ]);
    expect(result.current.compatibility).toBe(defaults);
    expect(result.current.sizing).toBeUndefined();
    expect(result.current.sizingFailed).toBe(false);
    expect(result.current.loading).toBe(false);
    expect(result.current.estimates.pending).toBe(true);

    // The same values in a new object don't read again.
    rerender({ scenario: { contextTokensPerRequest: 32768, totalContextTokens: 131072 } });
    expect(loader).toHaveBeenCalledTimes(2);

    await act(async () => {
      answers[1](20_000_000_000n);
      await Promise.resolve();
    });
    expect(result.current.sizing).toBe(result.current.compatibility);
    expect(result.current.estimates).toEqual({
      estimates: new Map([
        [
          profileEstimateKey("alpha", "fast"),
          [
            {
              acceleratorMemoryBytes: 20_000_000_000n,
              hardwareClassId: "nvidia",
              hostMemoryBytes: undefined,
            },
          ],
        ],
      ]),
      pending: false,
    });
  });

  it("drops the estimates when a new scenario's read fails, keeping the verdicts", async () => {
    const loader = vi
      .fn<ProfileCompatibilityLoader>()
      .mockResolvedValueOnce(response("alpha", ModelCompatibilityStatus.RUNS))
      .mockRejectedValueOnce(new Error("unavailable"));
    const { rerender, result } = renderHook(
      ({ scenario }) => useProfileCompatibility(loader, "alpha", true, scenario),
      { initialProps: { scenario: undefined as SizingScenario | undefined } },
    );
    await act(async () => {});

    rerender({ scenario: { contextTokensPerRequest: 32768, totalContextTokens: 131072 } });
    await act(async () => {});

    expect(status(result.current.compatibility, "alpha")).toBe("runs");
    expect(result.current.error).toBeUndefined();
    expect(result.current.sizing).toBeUndefined();
    expect(result.current.sizingFailed).toBe(true);
    expect(result.current.estimates).toEqual({ estimates: new Map(), pending: false });
  });

  it("stops reading once inactive and reads nothing without a loader", async () => {
    vi.useFakeTimers();
    const loader = vi.fn<ProfileCompatibilityLoader>((modelId) =>
      Promise.resolve(response(modelId, ModelCompatibilityStatus.RUNS)),
    );
    const { rerender, result } = renderHook(
      ({ active }) => useProfileCompatibility(loader, "alpha", active),
      { initialProps: { active: true } },
    );
    await act(async () => {});

    rerender({ active: false });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS * 2);
    });
    expect(loader).toHaveBeenCalledTimes(1);
    expect(result.current.loading).toBe(false);

    const { result: withoutLoader } = renderHook(() =>
      useProfileCompatibility(undefined, "alpha", true),
    );
    expect(withoutLoader.current).toEqual({
      compatibility: undefined,
      error: undefined,
      estimates: { estimates: new Map(), pending: false },
      loading: false,
      sizingFailed: false,
    });
  });
});

function response(modelId: string, status: ModelCompatibilityStatus, bytes = 8_000_000_000n) {
  return create(ListModelProfileCompatibilityResponseSchema, {
    entries: [{ modelId, profileId: "fast", status, target: gpu }],
    profiles: [
      profileSizing(modelId, "fast", { acceleratorMemoryBytes: bytes, hardwareClassId: "nvidia" }),
    ],
    targets: [{ status: ModelCompatibilityStatus.RUNS, target: gpu }],
  });
}

function status(
  compatibility: Parameters<typeof profileTargetVerdict>[0] | undefined,
  modelId: string,
) {
  return compatibility && profileTargetVerdict(compatibility, modelId, "fast", gpu)?.status;
}
