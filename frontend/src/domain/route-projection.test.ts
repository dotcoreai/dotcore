import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { ModelInputModality } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import {
  FallbackGroupAvailabilitySchema,
  FallbackGroupSchema,
  InferenceRouteAvailabilitySchema,
  InferenceRouteMemberAvailabilitySchema,
  InferenceRouteSchema,
  InferenceRouteUnavailabilityReason,
  InferenceRouteViewSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";

import { projectRouteHealth, selectionForMemberCount, selectionLabel } from "./route-projection";

const TEXT = [ModelInputModality.TEXT];
const IMAGE = [ModelInputModality.IMAGE];

/** A reported member: whether it is eligible and which input modalities it serves. */
type Member = { eligible: boolean; modalities?: ModelInputModality[] };

function view({
  availability,
  enabled = true,
  groups = [[{ eligible: true }]],
}: {
  availability?: { available: boolean; exclusionReasons?: InferenceRouteUnavailabilityReason[] };
  enabled?: boolean;
  groups?: Member[][];
}) {
  return create(InferenceRouteViewSchema, {
    availability: availability
      ? create(InferenceRouteAvailabilitySchema, {
          available: availability.available,
          exclusionReasons: availability.exclusionReasons ?? [],
          fallbackGroups: groups.map((members, groupIndex) =>
            create(FallbackGroupAvailabilitySchema, {
              fallbackGroupIndex: groupIndex,
              members: members.map((member, memberIndex) =>
                create(InferenceRouteMemberAvailabilitySchema, {
                  availableInputModalities: member.eligible ? (member.modalities ?? TEXT) : [],
                  eligible: member.eligible,
                  modelDeploymentId: `deployment-${groupIndex}-${memberIndex}`,
                }),
              ),
            }),
          ),
        })
      : undefined,
    route: create(InferenceRouteSchema, {
      enabled,
      fallbackGroups: groups.map((members, groupIndex) =>
        create(FallbackGroupSchema, {
          modelDeploymentIds: members.map(
            (_, memberIndex) => `deployment-${groupIndex}-${memberIndex}`,
          ),
        }),
      ),
      inferenceRouteId: "route-1",
      name: "chat",
    }),
  });
}

const available = { available: true };

describe("route health projection", () => {
  it("reports a route with every group ready as Available, served by the primary", () => {
    expect(
      projectRouteHealth(
        view({
          availability: available,
          groups: [[{ eligible: true }, { eligible: true }], [{ eligible: true }]],
        }),
      ),
    ).toEqual({ servingGroupIndex: 0, status: { label: "Available", tone: "success" } });
  });

  it("reports Degraded, still served by the primary, when a fallback group is down", () => {
    expect(
      projectRouteHealth(
        view({
          availability: available,
          groups: [[{ eligible: true }, { eligible: true }], [{ eligible: false }]],
        }),
      ),
    ).toEqual({ servingGroupIndex: 0, status: { label: "Degraded", tone: "warning" } });
  });

  it("reports Degraded, served by the fallback, when the primary is down", () => {
    expect(
      projectRouteHealth(
        view({
          availability: available,
          groups: [[{ eligible: false }, { eligible: false }], [{ eligible: true }]],
        }),
      ),
    ).toEqual({ servingGroupIndex: 1, status: { label: "Degraded", tone: "warning" } });
  });

  it("reports a ready single-group route as Available", () => {
    expect(projectRouteHealth(view({ availability: available }))).toEqual({
      servingGroupIndex: 0,
      status: { label: "Available", tone: "success" },
    });
  });

  it("keeps a reported unavailable route Unavailable with its reason and no serving group", () => {
    expect(
      projectRouteHealth(
        view({
          availability: {
            available: false,
            exclusionReasons: [InferenceRouteUnavailabilityReason.NO_ELIGIBLE_TEXT_MEMBER],
          },
          groups: [[{ eligible: false }], [{ eligible: false }]],
        }),
      ),
    ).toEqual({
      status: { attention: "No eligible text member", label: "Unavailable", tone: "neutral" },
    });
  });

  it("joins several unavailability reasons into one attention label", () => {
    expect(
      projectRouteHealth(
        view({
          availability: {
            available: false,
            exclusionReasons: [
              InferenceRouteUnavailabilityReason.DISABLED,
              InferenceRouteUnavailabilityReason.NO_ELIGIBLE_TEXT_MEMBER,
            ],
          },
        }),
      ).status.attention,
    ).toBe("Disabled, No eligible text member");
  });

  it("reports a disabled route as Disabled before consulting availability", () => {
    expect(projectRouteHealth(view({ availability: available, enabled: false }))).toEqual({
      status: { label: "Disabled", tone: "neutral" },
    });
  });

  it("reports Unknown when no availability was reported, distinct from Unavailable", () => {
    expect(projectRouteHealth(view({}))).toEqual({ status: { label: "Unknown", tone: "neutral" } });
    expect(projectRouteHealth(view({ availability: { available: false } })).status.label).toBe(
      "Unavailable",
    );
  });

  it("serves from the first group with a text-capable member, not merely an eligible one", () => {
    expect(
      projectRouteHealth(
        view({
          availability: available,
          groups: [[{ eligible: true, modalities: IMAGE }], [{ eligible: true, modalities: TEXT }]],
        }),
      ),
    ).toEqual({ servingGroupIndex: 1, status: { label: "Degraded", tone: "warning" } });
  });

  it("never marks a route with the destructive tone a failed deployment carries", () => {
    for (const projected of [
      projectRouteHealth(view({ enabled: false })),
      projectRouteHealth(view({})),
      projectRouteHealth(view({ availability: { available: false } })),
    ]) {
      expect(projected.status.tone).not.toBe("destructive");
    }
  });
});

describe("group selection", () => {
  it("derives single from one member and round robin from two or more", () => {
    expect(selectionLabel(selectionForMemberCount(1))).toBe("single");
    expect(selectionLabel(selectionForMemberCount(2))).toBe("round robin");
    expect(selectionLabel(selectionForMemberCount(32))).toBe("round robin");
  });
});
