import { modelDeploymentPresentationName, modelStatusLabel } from "@/domain/model-projection";
import { ModelInputModality } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import {
  type FallbackGroupAvailability,
  FallbackGroupSelection,
  type InferenceRouteMemberAvailability,
  InferenceRouteMemberExclusionReason,
  InferenceRouteUnavailabilityReason,
  type InferenceRouteView,
} from "@/gen/dotcore/inference/v1/routes_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import type { StatusProjection } from "@/lib/status-projection";

/** A route's projected status and the fallback group the console shows as serving, if any. */
export interface RouteHealth {
  status: StatusProjection;
  servingGroupIndex?: number;
}

/**
 * The status a route card and the route inspector both render, and the group serving text requests.
 * A route reports availability rather than a lifecycle, so it keeps its own vocabulary: an
 * administrator disables a route deliberately and an unavailable route is a configuration outcome,
 * neither of which is a failure.
 *
 * Disabled, Unknown, and Unavailable come straight from the route manager's report. Only a route it
 * reports available is refined: the serving group is the first group with a member eligible for
 * text, as the router picks it, and the route reads Degraded when that is not the primary or when
 * some fallback group has no such member. Degraded and the serving group are console display over
 * the reported member eligibility, not a route status the route manager owns.
 */
export function projectRouteHealth(view: InferenceRouteView): RouteHealth {
  if (!view.route?.enabled) return { status: { label: "Disabled", tone: "neutral" } };

  const availability = view.availability;
  if (!availability) return { status: { label: "Unknown", tone: "neutral" } };
  if (!availability.available) {
    return {
      status: {
        attention:
          availability.exclusionReasons.map(unavailabilityReasonLabel).join(", ") || undefined,
        label: "Unavailable",
        tone: "neutral",
      },
    };
  }

  const serving = view.route.fallbackGroups.map((_, index) =>
    hasTextEligibleMember(availability.fallbackGroups.find((g) => g.fallbackGroupIndex === index)),
  );
  const servingGroupIndex = serving.indexOf(true);
  const degraded = servingGroupIndex !== 0 || serving.slice(1).includes(false);

  return {
    servingGroupIndex: servingGroupIndex === -1 ? undefined : servingGroupIndex,
    status: degraded
      ? { label: "Degraded", tone: "warning" }
      : { label: "Available", tone: "success" },
  };
}

/** The selection a group's member count implies: one member is single, more is round robin. */
export function selectionForMemberCount(count: number): FallbackGroupSelection {
  return count > 1 ? FallbackGroupSelection.ROUND_ROBIN : FallbackGroupSelection.SINGLE;
}

export function selectionLabel(selection: FallbackGroupSelection): string {
  return selection === FallbackGroupSelection.ROUND_ROBIN ? "round robin" : "single";
}

/** A group's tier name: the first group is the primary and every later one is a fallback. */
export function tierLabel(index: number): string {
  return index === 0 ? "Primary" : "Fallback";
}

export function unavailabilityReasonLabel(reason: InferenceRouteUnavailabilityReason): string {
  if (reason === InferenceRouteUnavailabilityReason.DISABLED) return "Disabled";
  if (reason === InferenceRouteUnavailabilityReason.NO_ELIGIBLE_TEXT_MEMBER) {
    return "No eligible text member";
  }
  return "Unspecified";
}

export function memberExclusionReasonLabel(reason: InferenceRouteMemberExclusionReason): string {
  if (reason === InferenceRouteMemberExclusionReason.MODEL_DEPLOYMENT_NOT_READY) {
    return "Model deployment not ready";
  }
  return "Unspecified";
}

/**
 * A member's status in the route inspector. The route manager's eligibility decides it: an eligible
 * member is Ready, and a member it omits from its report is Not reported. An ineligible member
 * names its deployment's status when that is not ready, such as Paused, because that says why;
 * otherwise the route manager's exclusion reasons stand.
 */
export function projectMemberStatus(
  member: InferenceRouteMemberAvailability | undefined,
  deploymentStatus?: ModelDeploymentStatus,
): StatusProjection {
  if (!member) return { label: "Not reported", tone: "neutral" };
  if (member.eligible) return { label: "Ready", tone: "success" };
  if (deploymentStatus !== undefined && deploymentStatus !== ModelDeploymentStatus.READY) {
    return { label: modelStatusLabel(deploymentStatus), tone: "warning" };
  }

  return {
    label: member.exclusionReasons.map(memberExclusionReasonLabel).join(", ") || "Not ready",
    tone: "warning",
  };
}

/** A tier's status in the route inspector, and whether its dot is hollow for no member in service. */
export interface TierStatus {
  hollow: boolean;
  status: StatusProjection;
}

/**
 * A tier's status in the route inspector: Serving for the group `projectRouteHealth` derives as
 * serving, Standby for another group with a member eligible for text, and Unavailable for a group
 * without one. A group the route manager omits from its report is Not reported, like its members.
 */
export function projectTierStatus(
  group: FallbackGroupAvailability | undefined,
  serving: boolean,
): TierStatus {
  if (serving) return { hollow: false, status: { label: "Serving", tone: "success" } };
  if (!group) return { hollow: true, status: { label: "Not reported", tone: "neutral" } };
  if (hasTextEligibleMember(group)) {
    return { hollow: false, status: { label: "Standby", tone: "neutral" } };
  }

  return { hollow: true, status: { label: "Unavailable", tone: "warning" } };
}

/** The name the console shows for a model deployment: its own name, or its model's name. */
export function deploymentLabel(deployment: ModelDeployment): string {
  return modelDeploymentPresentationName(deployment);
}

/** Whether a reported group has a member the router could pick for a text request. */
function hasTextEligibleMember(group: FallbackGroupAvailability | undefined): boolean {
  return Boolean(
    group?.members.some(
      (member) =>
        member.eligible && member.availableInputModalities.includes(ModelInputModality.TEXT),
    ),
  );
}
