import { create } from "@bufbuild/protobuf";
import { render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { resolveInferenceOrigin } from "@/api/inference-origin";
import { ChatOpenerContext } from "@/features/chat/chat-opener";
import {
  counterPoint,
  distributionPoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import {
  InferenceRequestKind,
  ModelInputModality,
} from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import {
  FallbackGroupAvailabilitySchema,
  FallbackGroupSchema,
  FallbackGroupSelection,
  InferenceRouteAvailabilitySchema,
  InferenceRouteMemberAvailabilitySchema,
  InferenceRouteMemberExclusionReason,
  InferenceRouteSchema,
  InferenceRouteUnavailabilityReason,
  InferenceRouteViewSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import type { MetricSeries } from "@/gen/dotcore/metrics/v1/metric_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import * as clipboard from "@/lib/clipboard";
import { moreActionNames } from "@/test/inspector-actions";
import { expectResourceMarker } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

import { buildModelConnectionExamples } from "./model-connection";
import { RouteConnectionView, RouteDetails, RouteInspectorActions } from "./route-inspector";

const writeClipboard = vi.fn<typeof clipboard.writeClipboard>();
beforeEach(() => {
  vi.spyOn(clipboard, "writeClipboard").mockImplementation(writeClipboard);
});

function view() {
  return create(InferenceRouteViewSchema, {
    route: create(InferenceRouteSchema, {
      inferenceRouteId: "route-7",
      name: "route/model-alpha",
      enabled: true,
      fallbackGroups: [
        create(FallbackGroupSchema, {
          selection: FallbackGroupSelection.SINGLE,
          modelDeploymentIds: ["private-deployment-id"],
        }),
      ],
    }),
    availability: create(InferenceRouteAvailabilitySchema, { available: true }),
  });
}

describe("RouteDetails requests", () => {
  it("places six request cards between routing and details, reading the route's requests", async () => {
    const user = setupUser();
    const counter = (delta: number) => [metricSeries({}, [counterPoint(0, 0, false, delta)])];
    const answers: Record<string, MetricSeries[]> = {
      "router.request.ttft": [
        metricSeries({}, [distributionPoint(0, { 0.95: 1.5 }, false, { count: 2, sum: 0.5 })]),
      ],
      "router.requests": counter(7),
      "router.tokens.completion": counter(70),
      "router.tokens.prompt": counter(200),
    };
    const watchMetrics = vi.fn(
      staticWatch((query) =>
        queryResult({ series: answers[query?.metric ?? ""] ?? [], stepCount: 60, stepSeconds: 15 }),
      ),
    );

    render(
      <RouteDetails
        deploymentMentions={new Map()}
        metricsClient={metricsClientWith(watchMetrics)}
        view={view()}
      />,
    );

    expect(
      screen.getAllByRole("region").map((region) => region.getAttribute("aria-labelledby")),
    ).toEqual(["route-routing-heading", "route-requests-heading", "route-details-heading"]);
    const requests = screen.getByRole("region", { name: "Requests" });
    expect(
      await within(requests).findByRole("button", { name: /^Time to first token250 ms/ }),
    ).toBeVisible();
    expect(
      within(requests)
        .getAllByRole("button")
        .map((card) => card.querySelector('[data-slot="metric-card-label"]')?.textContent),
    ).toEqual([
      "Requests",
      "Processed tokens",
      "Generated tokens",
      "Prompt processing",
      "Time to first token",
      "Token generation",
    ]);
    expect(within(requests).getByRole("button", { name: /^Requests7/ })).toBeVisible();
    expect(within(requests).getByRole("button", { name: /^Generated tokens70/ })).toHaveTextContent(
      "10 per request",
    );
    expect(watchMetrics).toHaveBeenCalledTimes(1);
    expect(
      watchMetrics.mock.calls.flatMap(([request]) =>
        (request.queries ?? []).map(({ query }) => [query?.metric, query?.matchers]),
      ),
    ).toEqual([
      ["router.requests", { route: "route-7" }],
      ["router.tokens.prompt", { route: "route-7" }],
      ["router.tokens.cached_prompt", { route: "route-7" }],
      ["router.tokens.completion", { route: "route-7" }],
      ["router.requests", { outcome: "success", route: "route-7" }],
      ["router.attempt.prefill_rate", { route: "route-7" }],
      ["router.request.ttft", { route: "route-7" }],
      ["router.attempt.decode_rate", { route: "route-7" }],
    ]);

    await user.click(within(requests).getByRole("button", { name: /^Time to first token/ }));

    expect(
      await within(requests).findByRole("img", { name: "Time to first token history plot" }),
    ).toBeVisible();
    expect(watchMetrics).toHaveBeenCalledTimes(1);
  });

  it("shows no requests section without a metrics client", () => {
    render(<RouteDetails deploymentMentions={new Map()} view={view()} />);

    expect(screen.queryByRole("region", { name: "Requests" })).toBeNull();
  });
});

describe("RouteInspector", () => {
  it("keeps route details and Connect content as distinct stack screen bodies", async () => {
    const user = setupUser();
    writeClipboard.mockResolvedValue(undefined);
    const onConnect = vi.fn();
    render(
      <>
        <RouteDetails deploymentMentions={new Map()} view={view()} />
        <RouteInspectorActions
          enabled
          onConnect={onConnect}
          onDelete={vi.fn()}
          onEdit={vi.fn()}
          onToggleEnabled={vi.fn()}
        />
      </>,
    );
    expect(screen.getByTestId("route-inspector")).toHaveTextContent("private-deployment-id");
    expect(screen.getAllByRole("button").map((button) => button.textContent)).toEqual([
      "Connect",
      "",
    ]);
    expect(screen.getByRole("button", { name: "Connect" })).not.toHaveClass("bg-primary");
    expect(await moreActionNames(user)).toEqual(["Edit", "Disable", "Delete…"]);
    await user.keyboard("{Escape}");
    await user.click(screen.getByRole("button", { name: "Connect" }));
    expect(onConnect).toHaveBeenCalledOnce();

    const origin = resolveInferenceOrigin(import.meta.env.VITE_DOTCORE_INFERENCE_ORIGIN, {
      allowTestFallback: true,
      allowViteFallback: true,
    }).replace(/\/+$/, "");
    const examples = buildModelConnectionExamples(origin, "route/model-alpha");
    render(<RouteConnectionView name="route/model-alpha" />);
    expect(screen.getByTestId("route-connection-view")).toHaveTextContent("route/model-alpha");
    // The section and its code block share the label, so the block is addressed by element.
    for (const [label, command] of [
      ["OpenCode", examples.opencodeCommand],
      ["Pi", examples.piCommand],
      ["Codex", examples.codexCommand],
      ["curl example", examples.curl],
    ] as const) {
      expect(screen.getByLabelText(label, { selector: "pre" }).textContent).toBe(command);
      await user.click(screen.getByRole("button", { name: `Copy ${label}` }));
      expect(writeClipboard).toHaveBeenLastCalledWith(command);
    }
    expect(examples.opencodeCommand).toBe(
      `dotc code opencode --model route/model-alpha --inference-addr ${origin}`,
    );
    expect(examples.piCommand).toBe(
      `dotc code pi --model route/model-alpha --inference-addr ${origin}`,
    );
    expect(examples.codexCommand).toBe(
      `dotc code codex --model route/model-alpha --inference-addr ${origin}`,
    );
  });

  it("titles every Connect section and derives ids a document can carry", () => {
    render(<RouteConnectionView name="route/model-alpha" />);

    const view = screen.getByTestId("route-connection-view");
    expect(
      [...view.querySelectorAll(":scope > section")].map((section) =>
        section.getAttribute("aria-labelledby"),
      ),
    ).toEqual([
      "route-connect-endpoint-heading",
      "route-connect-opencode-heading",
      "route-connect-pi-heading",
      "route-connect-codex-heading",
      "route-connect-curl-example-heading",
    ]);
    for (const heading of view.querySelectorAll("h2")) {
      expect(heading.id).not.toBe("");
      expect(heading.id).not.toContain(" ");
    }
    expect(screen.getByRole("region", { name: "Endpoint" })).toHaveTextContent(
      "Inference API origin",
    );
    const example = screen.getByRole("region", { name: "OpenCode" });
    expect(within(example).getByRole("button", { name: "Copy OpenCode" })).toBeVisible();
    expect(view.querySelectorAll("h3, h4")).toHaveLength(0);
  });

  it("lays routing out as Primary then Fallback tiers with modes, status dots and the serving dot", () => {
    // The seeded `chat` shape with the primary down: a round robin primary of two not-ready
    // deployments, then a single fallback whose one member is ready for text.
    render(
      <RouteDetails
        deploymentMentions={
          new Map([
            [
              "e2b",
              { colorKey: "model-e2b", name: "Gemma 4 E2B", status: ModelDeploymentStatus.PAUSED },
            ],
            [
              "qwen",
              { colorKey: "model-qwen", name: "Qwen3.8 27B", status: ModelDeploymentStatus.READY },
            ],
          ])
        }
        view={chatView({ e2b: false, e4b: false, qwen: true })}
      />,
    );

    const routing = screen.getByRole("region", { name: "Routing" });
    const primary = screen.getByTestId("route-tier-0");
    const fallback = screen.getByTestId("route-tier-1");
    expect([...within(routing).getByRole("list", { name: "Routing" }).children]).toEqual([
      primary,
      fallback,
    ]);
    // Tier headers name the tier only; the selection mode is not shown.
    expect(tierHeader(primary)).toHaveTextContent(/^Primary$/);
    expect(tierHeader(fallback)).toHaveTextContent(/^Fallback$/);
    expect(routing).not.toHaveTextContent(/round robin|single/);

    // Only the derived serving group carries the green dot, placed before its tier name; the
    // primary without a ready member carries a hollow amber one.
    expect(within(primary).queryByRole("img", { name: "Serving" })).not.toBeInTheDocument();
    const serving = within(fallback).getByRole("img", { name: "Serving" });
    expect(tierHeader(fallback).firstElementChild).toBe(serving);
    expect(serving.querySelector("svg")).toBeNull();
    expect([primary, fallback].map(tierDot)).toEqual([
      { hollow: true, label: "Unavailable", tone: "warning" },
      { hollow: false, label: "Serving", tone: "success" },
    ]);
    // Only the unavailable tier reveals its label, so only its header takes focus.
    expect(primary.querySelector('[data-slot="tier-header"]')).toHaveAttribute("tabindex", "0");
    expect(fallback.querySelector('[data-slot="tier-header"]')).not.toHaveAttribute("tabindex");

    // A not-ready member names its deployment status when it has one, else the exclusion reason.
    const primaryMembers = within(
      within(primary).getByRole("list", { name: "Primary members" }),
    ).getAllByRole("listitem");
    expect(primaryMembers.map((member) => member.getAttribute("aria-label"))).toEqual([
      "Gemma 4 E2B, Paused",
      "e4b, Model deployment not ready",
    ]);
    expect(within(primary).getByRole("listitem", { name: "Gemma 4 E2B, Paused" })).toHaveAttribute(
      "tabindex",
      "0",
    );
    expectResourceMarker(within(primaryMembers[0]).getByText("Gemma 4 E2B"), "model-e2b");
    // The lookup is deliberately partial: an unknown member keeps its raw ID and carries no marker.
    expect(primaryMembers[1].querySelector('[data-slot="resource-marker"]')).toBeNull();
    expect(primaryMembers.map(statusDot)).toEqual([
      { hollow: true, tone: "warning" },
      { hollow: true, tone: "warning" },
    ]);

    // A ready member carries a solid green dot and a named row with no label to reveal.
    const ready = within(
      within(fallback).getByRole("list", { name: "Fallback members" }),
    ).getByRole("listitem");
    expect(ready).toHaveAccessibleName("Qwen3.8 27B, Ready");
    expect(ready).not.toHaveAttribute("tabindex");
    expect(ready.querySelector('[data-slot="member-status"]')).toBeNull();
    expect(statusDot(ready)).toEqual({ hollow: false, tone: "success" });

    // Not-ready labels are revealed on hover or focus only, in their dot's tone.
    expect(primaryMembers.map((member) => revealedLabel(member, "member-status"))).toEqual([
      { text: "Paused", tone: "warning" },
      { text: "Model deployment not ready", tone: "warning" },
    ]);

    // Members are not links, so the tiers add no control to the inspector body.
    expect(within(routing).queryByRole("button")).not.toBeInTheDocument();
  });

  it("names every later tier Fallback without numbering and adds no explanatory copy", () => {
    render(
      <RouteDetails
        deploymentMentions={new Map()}
        view={chatView({ e2b: true, e4b: true, qwen: true }, [["e2b", "e4b"], ["qwen"], ["e4b"]])}
      />,
    );

    const inspector = screen.getByTestId("route-inspector");
    expect(
      [0, 1, 2].map((index) => tierHeader(screen.getByTestId(`route-tier-${index}`)).textContent),
    ).toEqual(["Primary", "Fallback", "Fallback"]);
    // With every member ready, the primary alone serves and every fallback stands by.
    expect([0, 1, 2].map((index) => tierDot(screen.getByTestId(`route-tier-${index}`)))).toEqual([
      { hollow: false, label: "Serving", tone: "success" },
      { hollow: false, label: "Standby", tone: "neutral" },
      { hollow: false, label: "Standby", tone: "neutral" },
    ]);
    expect(inspector).not.toHaveTextContent(/Kind|Fallback group|tried in order|Enabled/);
    // The route ID stays reachable as a low-emphasis detail at the bottom, not up front.
    const details = screen.getByRole("region", { name: "Details" });
    expect(within(details).getByText("Route ID").nextElementSibling).toHaveTextContent("route-7");
    expect(within(details).getByText("Input modalities").nextElementSibling).toHaveTextContent(
      "text",
    );
    expect(inspector.lastElementChild).toBe(details);
  });

  it.each([
    ["chat-only", [InferenceRequestKind.CHAT], "Chat"],
    ["decision-only", [InferenceRequestKind.DECISION], "Decisions"],
    ["mixed", [InferenceRequestKind.CHAT, InferenceRequestKind.DECISION], "Chat, Decisions"],
    ["undeclared", [], "None"],
  ])("shows the request kinds of a %s route", (_name, kinds, expected) => {
    render(
      <RouteDetails
        deploymentMentions={new Map()}
        view={chatView({ e2b: true }, [["e2b"]], {}, kinds)}
      />,
    );

    const details = screen.getByRole("region", { name: "Details" });
    expect(within(details).getByText("Request kinds").nextElementSibling).toHaveTextContent(
      new RegExp(`^${expected}$`),
    );
  });

  it("offers Enable for a disabled route and shows a pending toggle on the menu", async () => {
    const user = setupUser();
    const actions = (props: { enabled: boolean; togglePending?: boolean }) => (
      <RouteInspectorActions
        onConnect={vi.fn()}
        onDelete={vi.fn()}
        onEdit={vi.fn()}
        onToggleEnabled={vi.fn()}
        pending={props.togglePending}
        {...props}
      />
    );
    const { rerender } = render(actions({ enabled: false }));
    expect(await moreActionNames(user)).toEqual(["Edit", "Enable", "Delete…"]);
    await user.keyboard("{Escape}");

    rerender(actions({ enabled: false, togglePending: true }));
    const trigger = screen.getByRole("button", { name: "More actions" });
    expect(trigger).toBeDisabled();
    expect(trigger).toHaveAttribute("aria-busy", "true");
    expect(screen.getByRole("button", { name: "Connect" })).toBeDisabled();
  });

  it("puts Chat first inside the console layout and leaves it out elsewhere", () => {
    const actions = (
      <RouteInspectorActions
        chatTarget={{ label: "Chat", unavailableReason: "Enable the route to chat with it." }}
        enabled={false}
        onConnect={vi.fn()}
        onDelete={vi.fn()}
        onEdit={vi.fn()}
        onToggleEnabled={vi.fn()}
      />
    );
    const { rerender } = render(actions);
    expect(screen.queryByRole("button", { name: "Chat" })).not.toBeInTheDocument();

    rerender(<ChatOpenerContext.Provider value={vi.fn()}>{actions}</ChatOpenerContext.Provider>);

    expect(screen.getAllByRole("button").map((button) => button.textContent)).toEqual([
      "Chat",
      "Connect",
      "",
    ]);
    const chat = screen.getByRole("button", { name: "Chat" });
    expect(chat).toHaveAttribute("aria-disabled", "true");
    expect(chat).toHaveAccessibleDescription("Enable the route to chat with it.");
  });

  it("states the route-level reason only when an enabled route is unavailable", () => {
    const { rerender } = render(
      <RouteDetails
        deploymentMentions={new Map()}
        view={chatView({ e2b: false, e4b: false, qwen: false }, undefined, {
          available: false,
          exclusionReasons: [InferenceRouteUnavailabilityReason.NO_ELIGIBLE_TEXT_MEMBER],
        })}
      />,
    );

    const notices = within(screen.getByRole("region", { name: "Routing" })).getAllByRole("alert");
    expect(notices.map((notice) => notice.textContent)).toEqual(["No eligible text member"]);
    expect(
      within(screen.getByRole("region", { name: "Routing" })).queryByRole("img", {
        name: "Serving",
      }),
    ).toBeNull();
    expect(screen.getByText("None available")).toBeVisible();

    // A disabled route already reads Disabled in the header, so no reason notice repeats it.
    rerender(
      <RouteDetails
        deploymentMentions={new Map()}
        view={chatView({ e2b: false, e4b: false, qwen: false }, undefined, {
          available: false,
          enabled: false,
          exclusionReasons: [InferenceRouteUnavailabilityReason.DISABLED],
        })}
      />,
    );
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    // An available route shows no notice either, and a fallback that is down reads Unavailable.
    rerender(
      <RouteDetails
        deploymentMentions={new Map()}
        view={chatView({ e2b: true, e4b: true, qwen: false })}
      />,
    );
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect([0, 1].map((index) => tierDot(screen.getByTestId(`route-tier-${index}`)))).toEqual([
      { hollow: false, label: "Serving", tone: "success" },
      { hollow: true, label: "Unavailable", tone: "warning" },
    ]);
  });

  it("keeps plain membership when the controller reported no availability", () => {
    const routeView = create(InferenceRouteViewSchema, {
      route: create(InferenceRouteSchema, {
        inferenceRouteId: "route-7",
        name: "fallback",
        enabled: true,
        fallbackGroups: [
          create(FallbackGroupSchema, {
            selection: FallbackGroupSelection.SINGLE,
            modelDeploymentIds: ["known"],
          }),
          create(FallbackGroupSchema, { selection: FallbackGroupSelection.SINGLE }),
        ],
      }),
    });

    render(
      <RouteDetails
        deploymentMentions={new Map([["known", { colorKey: "model-known", name: "Gemma 4 12B" }]])}
        view={routeView}
      />,
    );

    // Nothing is asserted about availability the controller never reported.
    const details = screen.getByRole("region", { name: "Details" });
    expect(within(details).getByText("Input modalities").nextElementSibling).toHaveTextContent(
      "Not reported",
    );
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("img", { name: "Serving" })).not.toBeInTheDocument();
    expect(tierDot(screen.getByTestId("route-tier-0"))).toEqual({
      hollow: true,
      label: "Not reported",
      tone: "neutral",
    });
    const member = within(screen.getByTestId("route-tier-0")).getByRole("listitem");
    expect(member).toHaveAccessibleName("Gemma 4 12B, Not reported");
    expect(statusDot(member)).toEqual({ hollow: true, tone: "neutral" });
    expect(revealedLabel(member, "member-status")).toEqual({
      text: "Not reported",
      tone: "neutral",
    });

    // An empty tier states itself instead of drawing an empty member list.
    const empty = screen.getByTestId("route-tier-1");
    expect(within(empty).getByText("No member deployments.")).toBeVisible();
    expect(within(empty).queryByRole("list")).not.toBeInTheDocument();
  });

  it("uses one heading level across the root, matching every other inspector", () => {
    render(<RouteDetails deploymentMentions={new Map()} view={view()} />);

    const inspector = screen.getByTestId("route-inspector");
    expect(inspector.querySelectorAll("h3, h4")).toHaveLength(0);
    expect([...inspector.querySelectorAll("h2")].map((heading) => heading.textContent)).toEqual([
      "Routing",
      "Details",
    ]);
    expect([...inspector.children].filter((child) => child.tagName !== "SECTION")).toHaveLength(0);
  });
});

/**
 * The seeded `chat` route: a round robin primary of `e2b` and `e4b`, then a single `qwen` fallback,
 * with each member's reported readiness for text requests.
 */
function chatView(
  ready: Record<string, boolean>,
  groups: string[][] = [["e2b", "e4b"], ["qwen"]],
  availability: {
    available?: boolean;
    enabled?: boolean;
    exclusionReasons?: InferenceRouteUnavailabilityReason[];
  } = {},
  requestKinds: InferenceRequestKind[] = [InferenceRequestKind.CHAT],
) {
  return create(InferenceRouteViewSchema, {
    requestKinds,
    route: create(InferenceRouteSchema, {
      inferenceRouteId: "route-7",
      name: "chat",
      enabled: availability.enabled ?? true,
      fallbackGroups: groups.map((ids) =>
        create(FallbackGroupSchema, {
          selection:
            ids.length > 1 ? FallbackGroupSelection.ROUND_ROBIN : FallbackGroupSelection.SINGLE,
          modelDeploymentIds: ids,
        }),
      ),
    }),
    availability: create(InferenceRouteAvailabilitySchema, {
      available: availability.available ?? true,
      availableInputModalities: Object.values(ready).some(Boolean) ? [ModelInputModality.TEXT] : [],
      exclusionReasons: availability.exclusionReasons ?? [],
      fallbackGroups: groups.map((ids, index) =>
        create(FallbackGroupAvailabilitySchema, {
          fallbackGroupIndex: index,
          members: ids.map((id) =>
            create(InferenceRouteMemberAvailabilitySchema, {
              availableInputModalities: ready[id] ? [ModelInputModality.TEXT] : [],
              eligible: ready[id],
              exclusionReasons: ready[id]
                ? []
                : [InferenceRouteMemberExclusionReason.MODEL_DEPLOYMENT_NOT_READY],
              modelDeploymentId: id,
            }),
          ),
        }),
      ),
    }),
  });
}

/** The tone and fill of the status dot inside an element. */
function statusDot(element: HTMLElement) {
  const dot = element.querySelector('[data-slot="status-dot"]');
  // The row reveals its own label, so the dot carries no tooltip repeating it.
  expect(dot).not.toHaveAttribute("title");
  return {
    hollow: dot?.classList.contains("border-[1.5px]"),
    tone: dot?.getAttribute("data-tone"),
  };
}

/**
 * A tier's status dot, named by its label. A hollow dot's tier also reveals that label on hover or
 * focus; a solid one reveals none.
 */
function tierDot(tier: HTMLElement) {
  const header = tier.querySelector<HTMLElement>('[data-slot="tier-header"]')!;
  const dot = within(header).getByRole("img");
  const label = dot.getAttribute("aria-label");
  const status = statusDot(dot);
  expect(tierHeader(tier).firstElementChild).toBe(dot);
  if (status.hollow) {
    expect(revealedLabel(header, "tier-status")).toEqual({ text: label, tone: status.tone });
  } else {
    expect(header.querySelector('[data-slot="tier-status"]')).toBeNull();
  }
  return { label, ...status };
}

/**
 * The status label a row reveals on hover or focus, asserting it is hidden by default, always shown
 * where hover is unavailable, and drawn in the same tone as the row's dot.
 */
function revealedLabel(row: HTMLElement, slot: string) {
  const label = row.querySelector(`[data-slot="${slot}"]`);
  expect(label).toHaveClass(
    "opacity-0",
    "group-hover:opacity-100",
    "group-focus:opacity-100",
    "[@media(hover:none)]:opacity-100",
  );
  const text = label!.querySelector('[data-slot="status-text"]');
  const tone = text?.getAttribute("data-tone");
  expect(tone).toBe(row.querySelector('[data-slot="status-dot"]')?.getAttribute("data-tone"));
  expect(text).toHaveClass(`text-${tone === "neutral" ? "muted" : tone}-foreground`);
  return { text: text?.textContent, tone };
}

/** A tier's header row: the element holding its status dot and its tier name. */
function tierHeader(tier: HTMLElement): HTMLElement {
  return within(tier).getByText(/^(Primary|Fallback)/);
}
