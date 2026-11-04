import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { InferenceRequestKind } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import {
  InferenceRouteAvailabilitySchema,
  InferenceRouteSchema,
  InferenceRouteViewSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";

import { deploymentChatTarget, routeChatTarget } from "./chattable-route";

const { CHAT, DECISION } = InferenceRequestKind;

function routeView(
  name: string,
  members: string[],
  options: { enabled?: boolean; available?: boolean; kinds?: InferenceRequestKind[] } = {},
) {
  const { enabled = true, available = true, kinds = [] } = options;
  return create(InferenceRouteViewSchema, {
    availability: create(InferenceRouteAvailabilitySchema, { available }),
    requestKinds: kinds,
    route: create(InferenceRouteSchema, {
      enabled,
      fallbackGroups: [{ modelDeploymentIds: members }],
      inferenceRouteId: `id-${name}`,
      name,
    }),
  });
}

describe("routeChatTarget", () => {
  it("targets an enabled, available route and names why others cannot chat", () => {
    expect(routeChatTarget(routeView("alpha", []))).toEqual({
      label: "Chat",
      mode: "chat",
      route: "alpha",
    });
    expect(routeChatTarget(routeView("alpha", [], { enabled: false }))).toEqual({
      label: "Chat",
      unavailableReason: "Enable the route to chat with it.",
    });
    expect(routeChatTarget(routeView("alpha", [], { available: false }))).toEqual({
      label: "Chat",
      unavailableReason: "The route is unavailable.",
    });
  });

  it.each([
    { name: "chat-only", kinds: [CHAT], target: { label: "Chat", mode: "chat", route: "r" } },
    {
      name: "decision-only",
      kinds: [DECISION],
      target: { label: "Ask", mode: "decision", route: "r" },
    },
    { name: "mixed", kinds: [CHAT, DECISION], target: { label: "Chat", mode: "chat", route: "r" } },
  ])("labels and opens a $name route by the kinds it serves", ({ kinds, target }) => {
    expect(routeChatTarget(routeView("r", [], { kinds }))).toEqual(target);
  });

  it("labels an unavailable decision-only route Ask, with its reason", () => {
    expect(routeChatTarget(routeView("r", [], { enabled: false, kinds: [DECISION] }))).toEqual({
      label: "Ask",
      unavailableReason: "Enable the route to ask it.",
    });
    expect(routeChatTarget(routeView("r", [], { available: false, kinds: [DECISION] }))).toEqual({
      label: "Ask",
      unavailableReason: "The route is unavailable.",
    });
  });
});

describe("deploymentChatTarget", () => {
  it("targets the first chattable route listing the deployment", () => {
    const routes = [
      routeView("other", ["deployment-b"]),
      routeView("disabled", ["deployment-a"], { enabled: false }),
      routeView("down", ["deployment-a"], { available: false }),
      routeView("first", ["deployment-a"]),
      routeView("second", ["deployment-a"]),
    ];

    expect(deploymentChatTarget(routes, "deployment-a", [])).toEqual({
      label: "Chat",
      mode: "chat",
      route: "first",
    });
  });

  it("names why a deployment has no chattable route", () => {
    expect(
      deploymentChatTarget([routeView("other", ["deployment-b"])], "deployment-a", []),
    ).toEqual({
      label: "Chat",
      unavailableReason: "Add the deployment to a route to chat with it.",
    });
    expect(
      deploymentChatTarget(
        [
          routeView("disabled", ["deployment-a"], { enabled: false }),
          routeView("down", ["deployment-a"], { available: false }),
        ],
        "deployment-a",
        [],
      ),
    ).toEqual({
      label: "Chat",
      unavailableReason: "None of the deployment's routes is available.",
    });
  });

  it("asks on a mixed route for a decision-only deployment and chats for a chat one", () => {
    const routes = [routeView("mixed", ["clef", "gemma"], { kinds: [CHAT, DECISION] })];

    expect(deploymentChatTarget(routes, "clef", [DECISION])).toEqual({
      label: "Ask",
      mode: "decision",
      route: "mixed",
    });
    expect(deploymentChatTarget(routes, "gemma", [CHAT])).toEqual({
      label: "Chat",
      mode: "chat",
      route: "mixed",
    });
  });

  it("labels a decision-only deployment without a usable route Ask, with its reason", () => {
    expect(deploymentChatTarget([], "clef", [DECISION])).toEqual({
      label: "Ask",
      unavailableReason: "Add the deployment to a route to ask it.",
    });
    expect(
      deploymentChatTarget(
        [routeView("down", ["clef"], { available: false, kinds: [DECISION] })],
        "clef",
        [DECISION],
      ),
    ).toEqual({ label: "Ask", unavailableReason: "None of the deployment's routes is available." });
  });
});
