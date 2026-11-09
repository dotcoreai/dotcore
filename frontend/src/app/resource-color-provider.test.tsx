import { create } from "@bufbuild/protobuf";
import { render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { MetricBar } from "@/components/metrics/metric-row/metric-row";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { ListApplicationDeploymentsResponseSchema } from "@/gen/dotcore/application/v1/service_pb";
import { ListRoutesResponseSchema } from "@/gen/dotcore/inference/v1/routes_pb";
import { ListModelDeploymentsResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import { ListMachinesResponseSchema } from "@/gen/dotcore/site/v1/service_pb";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";
import { getResourceColor } from "@/theme/resource-color";
import { useResourceColors } from "@/theme/resource-color-context";
import { ResourceColorProvider } from "@/theme/resource-color-provider";

describe("ResourceColorProvider", () => {
  it("gives machines and deployments whose IDs hash to the same color distinct colors", async () => {
    // gpu1 and gpu18 share a hash color, as do both deployments of the same catalog model.
    renderColors(["gpu1", "gpu18", "model-llama", "model-qwen", "gemma-catalog"]);

    await waitFor(() => expect(markerColor("gpu18")).toBe(cssColor("#9f8418")));
    expect(markerColor("gpu1")).toBe(cssColor("#a78151"));
    expect(markerColor("model-llama")).toBe(cssColor("#0593bf"));
    expect(markerColor("model-qwen")).toBe(cssColor("#578fa8"));
    // A catalog model is outside the pool and keeps its hash color.
    expect(markerColor("gemma-catalog")).toBe(cssColor(getResourceColor("gemma-catalog")));
  });

  it("draws a resource's marker and its metric bar in the same color", async () => {
    renderColors(["gpu18"]);

    await waitFor(() => expect(markerColor("gpu18")).toBe(cssColor("#9f8418")));
    const bar = screen.getByTestId("bar-gpu18").querySelector('[data-slot="metric-bar-fill"]');
    expect(bar).toHaveStyle({ backgroundColor: "#9f8418" });
  });

  it("offers the least used color for a resource about to join", async () => {
    renderColors([]);

    // Every pooled resource took its own color, so the first palette color is still free.
    await waitFor(() => expect(screen.getByTestId("next")).toHaveTextContent("#b27587"));
  });
});

function renderColors(keys: string[]) {
  const watch = new FakeResourceWatch({
    sources: {
      applicationDeployments: () =>
        Promise.resolve(create(ListApplicationDeploymentsResponseSchema, {})),
      machines: () =>
        Promise.resolve(
          create(ListMachinesResponseSchema, {
            machines: [{ machineId: "gpu18" }, { machineId: "gpu1" }],
          }),
        ),
      modelDeployments: () =>
        Promise.resolve(
          create(ListModelDeploymentsResponseSchema, {
            deployments: ["model-qwen", "model-llama"].map((modelDeploymentId) => ({
              modelDeploymentId,
              snapshot: { source: { case: "local" as const, value: { modelId: "gemma-catalog" } } },
            })),
          }),
        ),
      routes: () => Promise.resolve(create(ListRoutesResponseSchema, {})),
    },
  });

  render(
    <ResourceWatchProvider client={watch.client()}>
      <ResourceColorProvider>
        {keys.map((key) => (
          <span data-testid={`marker-${key}`} key={key}>
            <ResourceMarker colorKey={key} />
            <span data-testid={`bar-${key}`}>
              <MetricBar color={{ resource: key }} fraction={1} />
            </span>
          </span>
        ))}
        <NextColor />
      </ResourceColorProvider>
    </ResourceWatchProvider>,
  );
}

function NextColor() {
  return <span data-testid="next">{useResourceColors().nextColor}</span>;
}

function markerColor(key: string): string | undefined {
  return screen
    .getByTestId(`marker-${key}`)
    .querySelector<HTMLElement>('[data-slot="resource-marker"]')?.style.backgroundColor;
}

function cssColor(hex: string): string {
  const element = document.createElement("span");
  element.style.backgroundColor = hex;
  return element.style.backgroundColor;
}
