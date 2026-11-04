import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, render, renderHook as renderReactHook } from "@testing-library/react";
import { useEffect } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ModelClient } from "@/api/model-client";
import { ResourceWatchProvider } from "@/app/resource-watch-provider";
import { ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import {
  ListEngineReleasesResponseSchema,
  ListHardwareClassesResponseSchema,
  ListModelsResponseSchema,
  ListUpstreamModelTemplatesResponseSchema,
} from "@/gen/dotcore/catalog/v1/service_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
  ModelDeploymentSchema,
  UpstreamModelDesiredConfigurationSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { ListModelDeploymentsResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import { useSharedModelsPolling } from "./models-polling-context";
import { ModelsPollingProvider } from "./models-polling-provider";
import { useModelsPolling } from "./use-models-polling";

const { PAUSED, PRESENT, TEARDOWN } = ModelDeploymentDesiredState;

/** The hook's latest value, for the test that rendered it. */
let polling: () => ReturnType<typeof useModelsPolling>;

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useModelsPolling", () => {
  it("lists deployments from the watch, including one created elsewhere", async () => {
    const { watch } = await renderHook();

    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();
    expect(shown()).toBe("a:present:a");

    watch.payload(
      "modelDeployments",
      deployments(deployment("a", PRESENT), deployment("b", PRESENT)),
    );
    await flush();
    expect(shown()).toBe("a:present:a b:present:b");
  });

  it("keeps a pause overlay through a payload read before the pause", async () => {
    const { watch } = await renderHook();
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();

    act(() => polling().applyDeploymentOverlay(deployment("a", PAUSED)));
    expect(shown()).toBe("a:paused:a");

    // Read by the manager before the pause, delivered after its response.
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();
    expect(shown()).toBe("a:paused:a");

    watch.payload("modelDeployments", deployments(deployment("a", PAUSED)));
    await flush();
    expect(shown()).toBe("a:paused:a");

    // The settled overlay is gone, so a resume made elsewhere shows.
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();
    expect(shown()).toBe("a:present:a");
  });

  it("shows a created deployment until the stream lists it", async () => {
    const { watch } = await renderHook();
    watch.payload("modelDeployments", deployments());
    await flush();

    act(() => polling().applyDeploymentOverlay(deployment("new", PRESENT)));
    watch.payload("modelDeployments", deployments());
    await flush();
    expect(shown()).toBe("new:present:new");

    watch.payload("modelDeployments", deployments(deployment("new", PRESENT, "streamed")));
    await flush();
    expect(shown()).toBe("new:present:streamed");
  });

  it("shows a delete until the stream shows teardown or the deployment gone", async () => {
    const { watch } = await renderHook();
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();

    act(() => polling().applyDeploymentOverlay(deployment("a", TEARDOWN)));
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();
    expect(shown()).toBe("a:teardown:a");

    watch.payload("modelDeployments", deployments());
    await flush();
    expect(shown()).toBe("");
  });

  it("ignores a pause accepted after the deployment started tearing down", async () => {
    const { watch } = await renderHook();
    watch.payload("modelDeployments", deployments(deployment("a", TEARDOWN)));
    await flush();

    act(() => polling().applyDeploymentOverlay(deployment("a", PAUSED)));
    expect(shown()).toBe("a:teardown:a");
  });

  it("keeps a rename until the stream shows the new name", async () => {
    const { watch } = await renderHook();
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT, "old")));
    await flush();

    act(() => polling().applyDeploymentMetadataOverlay("a", { displayName: "new" }));
    watch.payload("modelDeployments", deployments(deployment("a", PAUSED, "old")));
    await flush();
    // The rename applies over the streamed deployment, so its other changes still show.
    expect(shown()).toBe("a:paused:new");

    watch.payload("modelDeployments", deployments(deployment("a", PAUSED, "new")));
    await flush();
    watch.payload("modelDeployments", deployments(deployment("a", PAUSED, "renamed elsewhere")));
    await flush();
    expect(shown()).toBe("a:paused:renamed elsewhere");
  });

  it("keeps an endpoint edit until the stream shows the new endpoint, ignoring the API key", async () => {
    const { watch } = await renderHook();
    watch.payload("modelDeployments", deployments(upstream("a", "https://old.example")));
    await flush();

    act(() =>
      polling().applyDeploymentEndpointOverlay("a", endpoint("https://new.example", "key")),
    );
    watch.payload("modelDeployments", deployments(upstream("a", "https://old.example")));
    await flush();
    expect(baseUrl()).toBe("https://new.example");

    watch.payload("modelDeployments", deployments(upstream("a", "https://new.example")));
    await flush();
    watch.payload("modelDeployments", deployments(upstream("a", "https://other.example")));
    await flush();
    expect(baseUrl()).toBe("https://other.example");
  });

  it("keeps the last deployments and reports them failed while the watch reconnects", async () => {
    const { watch } = await renderHook();
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();

    watch.close(new ConnectError("manager stopped", Code.Unavailable));
    await flush();
    expect(shown()).toBe("a:present:a");
    expect(polling().deploymentsFailed).toBe(true);
    expect(polling().error).toBeUndefined();
  });

  it("reads the catalog once per manager session", async () => {
    const { client, watch } = await renderHook();
    watch.payload("modelDeployments", deployments());
    await flush();
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();
    expect(client.catalog.listModels).toHaveBeenCalledTimes(1);

    watch.close(new ConnectError("manager stopped", Code.Unavailable));
    await flush();
    await act(() => vi.advanceTimersByTimeAsync(1_000));
    watch.sessionId = "session-b";
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();
    expect(client.catalog.listModels).toHaveBeenCalledTimes(2);
    expect(shown()).toBe("a:present:a");
  });

  it("reports the service unavailable when the catalog read fails before any value", async () => {
    const { watch } = await renderHook({
      listModels: () => Promise.reject(new ConnectError("down", Code.Unavailable)),
    });
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();

    expect(polling().snapshot).toBeUndefined();
    expect(polling().error).toBe(
      "The catalog or model service is unavailable. Check that the backend is running and try again.",
    );
  });
});

describe("ModelsPollingProvider", () => {
  it("watches and reads only while a page reads it, keeping overlays across pages", async () => {
    const watch = new FakeResourceWatch();
    const client = clientFor(() => Promise.resolve(create(ListModelsResponseSchema)));
    const consumer = (mounted: boolean) => (
      <ResourceWatchProvider client={watch.client()} reopenDelayMs={0}>
        <ModelsPollingProvider client={client}>
          {mounted ? <SharedPolling /> : null}
        </ModelsPollingProvider>
      </ResourceWatchProvider>
    );
    const view = render(consumer(false));
    await flush();
    expect(watch.watchedKinds()).toEqual([]);
    expect(client.catalog.listModels).not.toHaveBeenCalled();

    view.rerender(consumer(true));
    await flush();
    expect(watch.watchedKinds()).toEqual(["modelDeployments"]);
    expect(client.catalog.listModels).toHaveBeenCalledTimes(1);
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();
    act(() => polling().applyDeploymentOverlay(deployment("a", PAUSED)));
    expect(shown()).toBe("a:paused:a");

    view.rerender(consumer(false));
    await flush();
    expect(watch.watchedKinds()).toEqual([]);

    view.rerender(consumer(true));
    await flush();
    expect(watch.watchedKinds()).toEqual(["modelDeployments"]);
    watch.payload("modelDeployments", deployments(deployment("a", PRESENT)));
    await flush();
    expect(shown()).toBe("a:paused:a");
  });
});

/** Publishes the shared polling a page reads, for the provider tests. */
function SharedPolling() {
  const shared = useSharedModelsPolling();
  useEffect(() => {
    polling = () => shared;
  });
  return null;
}

async function renderHook({
  listModels = () =>
    Promise.resolve(
      create(ListModelsResponseSchema, {
        models: [create(ModelSchema, { displayName: "Model", modelId: "model-1" })],
      }),
    ),
}: { listModels?: ModelClient["catalog"]["listModels"] } = {}) {
  const watch = new FakeResourceWatch();
  const client = clientFor(listModels);
  const { result } = renderReactHook(() => useModelsPolling(client), {
    wrapper: ({ children }) => (
      <ResourceWatchProvider client={watch.client()} reopenDelayMs={0}>
        {children}
      </ResourceWatchProvider>
    ),
  });
  polling = () => result.current;
  await flush();
  return { client, watch };
}

/** Each listed deployment as `id:desired state:name`. */
function shown(): string {
  return (polling().snapshot?.runningModels ?? [])
    .map(
      ({ desiredState, modelDeploymentId, modelDeploymentName }) =>
        `${modelDeploymentId}:${ModelDeploymentDesiredState[desiredState].toLowerCase()}:${modelDeploymentName}`,
    )
    .join(" ");
}

function baseUrl(): string | undefined {
  return polling().snapshot?.runningModels[0]?.upstreamDesiredConfiguration?.endpoint?.baseUrl;
}

function deployment(
  modelDeploymentId: string,
  desiredState: ModelDeploymentDesiredState,
  modelDeploymentName = modelDeploymentId,
): ModelDeployment {
  return create(ModelDeploymentSchema, { desiredState, modelDeploymentId, modelDeploymentName });
}

function upstream(modelDeploymentId: string, url: string): ModelDeployment {
  return create(ModelDeploymentSchema, {
    desiredState: PRESENT,
    modelDeploymentId,
    upstreamDesiredConfiguration: endpoint(url),
  });
}

function endpoint(url: string, apiKey = "") {
  return create(UpstreamModelDesiredConfigurationSchema, {
    apiKey,
    endpoint: { baseUrl: url },
    upstreamModelId: "upstream-model",
  });
}

function deployments(...values: ModelDeployment[]) {
  return create(ListModelDeploymentsResponseSchema, { deployments: values });
}

function clientFor(listModels: ModelClient["catalog"]["listModels"]): ModelClient {
  return {
    catalog: {
      listEngineReleases: vi.fn(() => Promise.resolve(create(ListEngineReleasesResponseSchema))),
      listHardwareClasses: vi.fn(() => Promise.resolve(create(ListHardwareClassesResponseSchema))),
      listModels: vi.fn(listModels),
      listUpstreamModelTemplates: vi.fn(() =>
        Promise.resolve(create(ListUpstreamModelTemplatesResponseSchema)),
      ),
    },
    model: {
      createModelDeployment: vi.fn(),
      listModelDeployments: vi.fn(),
      pauseModel: vi.fn(),
      removeModelDeployment: vi.fn(),
      resumeModel: vi.fn(),
      updateModelDeployment: vi.fn(),
      updateModelDeploymentMetadata: vi.fn(),
    },
  };
}

async function flush() {
  await act(() => vi.advanceTimersByTimeAsync(0));
}
