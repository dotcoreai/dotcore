import { create } from "@bufbuild/protobuf";
import { renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import {
  dedicatedPreview,
  gib,
} from "@/features/models/deploy/memory/model-deploy-memory-test-support";
import { EngineParameterValuesSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import {
  type ExactPlacement,
  ExactPlacementSchema,
} from "@/gen/dotcore/deployment/v1/placement_pb";

import {
  type LocalResourceEstimate,
  type ResourceEstimateLoader,
  type ResourceEstimateRequest,
  useResourceEstimate,
} from "./use-resource-estimate";

describe("useResourceEstimate", () => {
  it("reports the configuration's placement preview", async () => {
    const preview = dedicatedPreview({ accelerator: 8n * gib });
    const onEstimate = vi.fn<ResourceEstimateLoader>().mockResolvedValue({
      placementPreview: preview,
    });
    const { result } = renderHook(() => useResourceEstimate(onEstimate, request("a")));

    expect(result.current.pending).toBe(true);
    await waitFor(() => expect(result.current.pending).toBe(false));
    expect(onEstimate).toHaveBeenCalledExactlyOnceWith(request("a").values);
    expect(result.current).toMatchObject({
      error: undefined,
      launchRejection: undefined,
      placementPreview: preview,
    });
  });

  it("keeps a rejected configuration's preview and holds the reason until the next result", async () => {
    const resolvers: Array<(value: LocalResourceEstimate) => void> = [];
    const onEstimate = vi.fn<ResourceEstimateLoader>(
      () =>
        new Promise((resolve) => {
          resolvers.push(resolve);
        }),
    );
    const { rerender, result } = renderHook(
      ({ key }) => useResourceEstimate(onEstimate, request(key)),
      { initialProps: { key: "a" } },
    );
    const rejected = dedicatedPreview({ accelerator: 3n * gib });

    resolvers[0]({ launchRejection: "total context is too large", placementPreview: rejected });
    await waitFor(() => expect(result.current.pending).toBe(false));
    expect(result.current).toMatchObject({
      error: undefined,
      launchRejection: "total context is too large",
      placementPreview: rejected,
    });

    rerender({ key: "b" });
    expect(result.current.pending).toBe(true);
    expect(result.current.launchRejection).toBe("total context is too large");

    const next = dedicatedPreview({ accelerator: 2n * gib });
    resolvers[1]({ placementPreview: next });
    await waitFor(() => expect(result.current.launchRejection).toBeUndefined());
    expect(result.current.placementPreview).toBe(next);
  });

  it("names the placement the shown preview was read for while the next one loads", async () => {
    const resolvers: Array<(value: LocalResourceEstimate) => void> = [];
    const onEstimate = vi.fn<ResourceEstimateLoader>(
      () =>
        new Promise((resolve) => {
          resolvers.push(resolve);
        }),
    );
    const { rerender, result } = renderHook(
      ({ key, target }) => useResourceEstimate(onEstimate, request(key, target)),
      { initialProps: { key: "a", target: "target-a" } },
    );
    const first = dedicatedPreview({ accelerator: 3n * gib });

    resolvers[0]({ placementPreview: first });
    await waitFor(() => expect(result.current.pending).toBe(false));
    expect(result.current.placement).toEqual(placement("target-a"));

    rerender({ key: "b", target: "target-b" });
    expect(result.current).toMatchObject({
      pending: true,
      placement: placement("target-a"),
      placementPreview: first,
    });

    const next = dedicatedPreview({ accelerator: 2n * gib });
    resolvers[1]({ placementPreview: next });
    await waitFor(() => expect(result.current.placementPreview).toBe(next));
    expect(result.current.placement).toEqual(placement("target-b"));
  });

  it("reports a failed estimate without a preview", async () => {
    const onEstimate = vi.fn<ResourceEstimateLoader>().mockRejectedValue(new Error("no sizing"));
    const { result } = renderHook(() => useResourceEstimate(onEstimate, request("a")));

    await waitFor(() => expect(result.current.pending).toBe(false));
    expect(result.current).toMatchObject({
      error: "Resource estimate unavailable: no sizing.",
      placementPreview: undefined,
    });
  });

  it("reads the same configuration again for a new revision, staying settled", async () => {
    const first = dedicatedPreview({ accelerator: 8n * gib });
    const refreshed = dedicatedPreview({ accelerator: 8n * gib, host: 2n * gib });
    const onEstimate = vi
      .fn<ResourceEstimateLoader>()
      .mockResolvedValueOnce({ placementPreview: first })
      .mockResolvedValueOnce({ placementPreview: refreshed });
    const { rerender, result } = renderHook(
      ({ revision }) => useResourceEstimate(onEstimate, { ...request("a"), revision }),
      { initialProps: { revision: 1 } },
    );
    await waitFor(() => expect(result.current.placementPreview).toBe(first));

    rerender({ revision: 2 });
    expect(result.current.pending).toBe(false);
    await waitFor(() => expect(result.current.placementPreview).toBe(refreshed));
    expect(onEstimate).toHaveBeenCalledTimes(2);
    expect(onEstimate).toHaveBeenLastCalledWith(request("a").values);
  });

  it("is never pending without an estimate loader or a complete configuration", () => {
    const withoutLoader = renderHook(() => useResourceEstimate(undefined, request("a")));
    const withoutRequest = renderHook(() => useResourceEstimate(vi.fn(), undefined));

    expect(withoutLoader.result.current.pending).toBe(false);
    expect(withoutRequest.result.current.pending).toBe(false);
  });
});

function request(key: string, target?: string): ResourceEstimateRequest {
  return {
    key,
    values: {
      configuration: {
        customArgumentTokens: [],
        engineParameterValues: create(EngineParameterValuesSchema),
        modelArtifactBindingId: `binding-${key}`,
        optionOverrides: [],
      },
      placement: target === undefined ? undefined : placement(target),
      profileId: "default",
    },
  };
}

function placement(target: string): ExactPlacement {
  return create(ExactPlacementSchema, { executionTargetId: target, siteNodeId: "gpu1" });
}
