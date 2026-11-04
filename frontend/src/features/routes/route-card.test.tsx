import { create } from "@bufbuild/protobuf";
import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { ModelInputModality } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import {
  FallbackGroupAvailabilitySchema,
  FallbackGroupSchema,
  FallbackGroupSelection,
  InferenceRouteAvailabilitySchema,
  InferenceRouteMemberAvailabilitySchema,
  InferenceRouteSchema,
  InferenceRouteViewSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";

import { RouteCard } from "./route-card";

const mentions = new Map([
  ["e2b", { colorKey: "model-e2b", name: "Gemma 4 E2B" }],
  ["e4b", { colorKey: "model-e4b", name: "Gemma 4 E4B QAT" }],
  ["qwen", { colorKey: "model-qwen", name: "Qwen3.8 27B" }],
]);

/** The seeded `chat` route: round robin E2B and E4B QAT, then a single Qwen fallback. */
function chatView(ready: Record<string, boolean>) {
  const groups = [["e2b", "e4b"], ["qwen"]];
  return create(InferenceRouteViewSchema, {
    route: create(InferenceRouteSchema, {
      enabled: true,
      fallbackGroups: groups.map((ids) =>
        create(FallbackGroupSchema, {
          modelDeploymentIds: ids,
          selection:
            ids.length > 1 ? FallbackGroupSelection.ROUND_ROBIN : FallbackGroupSelection.SINGLE,
        }),
      ),
      inferenceRouteId: "route-chat",
      name: "chat",
    }),
    availability: create(InferenceRouteAvailabilitySchema, {
      available: true,
      fallbackGroups: groups.map((ids, index) =>
        create(FallbackGroupAvailabilitySchema, {
          fallbackGroupIndex: index,
          members: ids.map((id) =>
            create(InferenceRouteMemberAvailabilitySchema, {
              availableInputModalities: ready[id] ? [ModelInputModality.TEXT] : [],
              eligible: ready[id],
              modelDeploymentId: id,
            }),
          ),
        }),
      ),
    }),
  });
}

function renderCard(view: ReturnType<typeof chatView>) {
  render(
    <ul>
      <RouteCard
        deploymentMentions={mentions}
        onSelect={vi.fn()}
        panelId="route-inspector-panel"
        selected={false}
        view={view}
      />
    </ul>,
  );
  return screen.getByTestId("route-chat");
}

describe("RouteCard", () => {
  it("shows the flow in priority order and strikes through the members that are not ready", () => {
    const card = renderCard(chatView({ e2b: true, e4b: true, qwen: false }));

    const flow = within(card).getByTestId("route-flow");
    expect(flow).toHaveAttribute("title", "Gemma 4 E2B, Gemma 4 E4B QAT → Qwen3.8 27B");
    // The flow wraps to at most two lines, so a fallback stays visible on a narrow card.
    expect(flow).toHaveClass("line-clamp-2");
    expect(flow).not.toHaveClass("truncate");
    expect(flow.textContent).toBe("Gemma 4 E2B, Gemma 4 E4B QAT → Qwen3.8 27B (not ready)");
    expect(
      [...flow.querySelectorAll('[data-ready="false"]')].map(
        (member) => member.firstChild?.textContent,
      ),
    ).toEqual(["Qwen3.8 27B"]);
    expect(flow.querySelector('[data-ready="false"]')).toHaveClass("line-through");
    expect(within(card).getByText("Degraded")).toHaveAttribute("data-variant", "warning");
  });

  it("keeps a fully ready route unstruck and Available", () => {
    const card = renderCard(chatView({ e2b: true, e4b: true, qwen: true }));

    expect(within(card).getByTestId("route-flow").querySelector('[data-ready="false"]')).toBeNull();
    expect(within(card).getByText("Available")).toHaveAttribute("data-variant", "success");
  });
});
