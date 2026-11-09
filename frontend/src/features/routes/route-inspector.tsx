import { Ban, CircleCheck, Code, Pencil, Trash2 } from "lucide-react";
import type { RefObject } from "react";

import { consoleInferenceOrigin } from "@/api/inference-origin";
import type { MetricsClient } from "@/api/metrics-client";
import { InspectorActions } from "@/components/inspector-actions/inspector-actions";
import { CodeCopyControl } from "@/components/ui/copy-control";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { Notice } from "@/components/ui/notice";
import { StatusDot, StatusText } from "@/components/ui/status-badge";
import { modelInputModalityLabel } from "@/domain/model-projection";
import { requestKindsLabel } from "@/domain/request-kinds";
import {
  projectMemberStatus,
  projectRouteHealth,
  projectTierStatus,
  tierLabel,
  unavailabilityReasonLabel,
} from "@/domain/route-projection";
import type { ChatTarget } from "@/features/chat/chattable-route";
import { useChatAction } from "@/features/chat/use-chat-action";
import { PanelMention } from "@/features/deployments/panel-mention";
import type {
  FallbackGroup,
  FallbackGroupAvailability,
  InferenceRouteMemberAvailability,
  InferenceRouteView,
} from "@/gen/dotcore/inference/v1/routes_pb";
import type { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import type { StatusProjection } from "@/lib/status-projection";
import { cn } from "@/lib/utils";

import { buildModelConnectionExamples } from "./model-connection";
import { RouteRequests } from "./route-requests";

/**
 * The route details action bar: Chat as the primary action inside the console layout, Connect
 * beside it, and the less used Edit, Disable or Enable, and Delete in the more-actions menu. While an action is saving, every action is unavailable and the toggle names the pending
 * change. `deleteFocusRef` receives the element the delete confirmation returns focus to.
 */
export function RouteInspectorActions({
  chatTarget,
  deleteFocusRef,
  enabled,
  onConnect,
  onDelete,
  onEdit,
  onToggleEnabled,
  pending = false,
  togglePending = false,
}: {
  chatTarget?: ChatTarget;
  deleteFocusRef?: RefObject<HTMLElement | null>;
  enabled: boolean;
  onConnect: () => void;
  onDelete: () => void;
  onEdit: () => void;
  onToggleEnabled: () => void;
  pending?: boolean;
  togglePending?: boolean;
}) {
  const chat = useChatAction(chatTarget);

  return (
    <InspectorActions
      more={[
        { disabled: pending, icon: Pencil, label: "Edit", onSelect: onEdit },
        {
          disabled: pending && !togglePending,
          icon: enabled ? Ban : CircleCheck,
          label: enabled ? "Disable" : "Enable",
          onSelect: onToggleEnabled,
          pending: togglePending,
          pendingLabel: enabled ? "Disabling…" : "Enabling…",
        },
        {
          destructive: true,
          disabled: pending,
          focusRef: deleteFocusRef,
          icon: Trash2,
          label: "Delete",
          onSelect: onDelete,
        },
      ]}
      primary={chat && { ...chat, disabled: chat.disabled || pending }}
      secondary={[{ disabled: pending, icon: Code, label: "Connect", onSelect: onConnect }]}
    />
  );
}

/**
 * One model deployment the console knows: its identity-marker color key, its display name, and its
 * status when the console has read it.
 */
export interface RouteMemberMention {
  colorKey: string;
  /** The link that pushes the model deployment's inspector onto the panel stack. */
  href?: string;
  name: string;
  status?: ModelDeploymentStatus;
}

/**
 * The selected route's routing as a vertical flow of tiers, primary first, then its low-emphasis
 * details. The route status badge lives in the inspector header, so the body only adds the
 * route-level reason when the route is unavailable. `deploymentMentions` maps the model deployment
 * IDs known to the console to their marker and name; members missing from it render as plain IDs,
 * since the console cannot name a deployment it has not read. `error` is the last failed action
 * from the action bar, shown above the routing. The requests section appears only when given a
 * metrics client.
 */
export function RouteDetails({
  deploymentMentions,
  error,
  initialRange,
  metricsClient,
  view,
}: {
  deploymentMentions: ReadonlyMap<string, RouteMemberMention>;
  error?: string;
  /** The range label the requests section starts at. */
  initialRange?: string;
  metricsClient?: MetricsClient;
  view: InferenceRouteView;
}) {
  const route = view.route!;
  const availability = view.availability;
  const { servingGroupIndex } = projectRouteHealth(view);
  const unavailableReasons =
    route.enabled && availability && !availability.available ? availability.exclusionReasons : [];
  const modalities = availability
    ? availability.availableInputModalities.map(modelInputModalityLabel).join(", ") ||
      "None available"
    : "Not reported";

  return (
    <div className="inspector-sections" data-testid="route-inspector">
      {error ? (
        <Notice role="alert" tone="destructive">
          {error}
        </Notice>
      ) : null}
      <section aria-labelledby="route-routing-heading">
        <InspectorSectionHeader headingId="route-routing-heading" title="Routing" />
        {unavailableReasons.length > 0 ? (
          <div className="mt-3">
            {/* Notice renders a `p`, so each reason stays inline content rather than a block child. */}
            <Notice role="alert" tone="destructive">
              {unavailableReasons.map((reason, index) => (
                <span className={index > 0 ? "mt-1.5 block" : undefined} key={reason}>
                  {unavailabilityReasonLabel(reason)}
                </span>
              ))}
            </Notice>
          </div>
        ) : null}
        <ol aria-labelledby="route-routing-heading" className="mt-3">
          {route.fallbackGroups.map((group, index) => (
            <RouteTier
              deploymentMentions={deploymentMentions}
              group={group}
              index={index}
              key={index}
              observed={availability?.fallbackGroups.find(
                (candidate) => candidate.fallbackGroupIndex === index,
              )}
              serving={index === servingGroupIndex}
            />
          ))}
        </ol>
      </section>
      {metricsClient ? (
        <RouteRequests
          client={metricsClient}
          initialRange={initialRange}
          key={route.inferenceRouteId}
          routeId={route.inferenceRouteId}
        />
      ) : null}
      <section aria-labelledby="route-details-heading">
        <InspectorSectionHeader headingId="route-details-heading" title="Details" />
        <DetailList>
          <Detail label="Request kinds" value={requestKindsLabel(view.requestKinds) || "None"} />
          <Detail label="Input modalities" value={modalities} />
          <Detail label="Route ID" value={route.inferenceRouteId} />
        </DetailList>
      </section>
    </div>
  );
}

/**
 * One tier of the routing flow: its tier name after a dot for its status, and its member
 * deployments. The dot carries the status as its accessible name, and a tier without a member in
 * service also reveals it on hover or focus, as member rows do. Tiers after the first hang from a
 * thin connector, so position alone gives the order.
 */
function RouteTier({
  deploymentMentions,
  group,
  index,
  observed,
  serving,
}: {
  deploymentMentions: ReadonlyMap<string, RouteMemberMention>;
  group: FallbackGroup;
  index: number;
  observed?: FallbackGroupAvailability;
  serving: boolean;
}) {
  const tier = tierLabel(index);
  const { hollow, status } = projectTierStatus(observed, serving);

  return (
    <li data-testid={`route-tier-${index}`}>
      {index > 0 ? <div aria-hidden="true" className="ml-5 h-4 w-px bg-border" /> : null}
      <div className="overflow-hidden rounded-lg border border-border bg-card">
        <div
          className={cn(
            "flex min-h-10 items-center justify-between gap-3 border-b border-border px-3 py-2",
            hollow && REVEAL_ROW,
          )}
          data-slot="tier-header"
          // Focus is the only way keyboard users reveal the label, so the header takes it.
          // eslint-disable-next-line jsx-a11y/no-noninteractive-tabindex
          tabIndex={hollow ? 0 : undefined}
        >
          <span className="flex items-center gap-1.5 text-sm font-semibold">
            <span className="flex" role="img" aria-label={status.label}>
              <StatusDot hollow={hollow} status={status} tooltip={false} />
            </span>
            {tier}
          </span>
          {hollow ? <StatusLabel slot="tier-status" status={status} /> : null}
        </div>
        {group.modelDeploymentIds.length === 0 ? (
          <p className="px-3 py-2.5 text-[12.5px] text-muted-foreground">No member deployments.</p>
        ) : (
          <ul aria-label={`${tier} members`}>
            {group.modelDeploymentIds.map((id) => {
              const mention = deploymentMentions.get(id);
              const member = observed?.members.find(
                (candidate) => candidate.modelDeploymentId === id,
              );

              return <RouteMember id={id} key={id} member={member} mention={mention} />;
            })}
          </ul>
        )}
      </div>
    </li>
  );
}

/**
 * One member deployment of a tier with a dot for its status: solid green when ready, hollow when
 * not ready or not reported. A not-ready row reveals its status label while hovered or focused, or
 * always on devices that cannot hover, and is focusable so keyboard users can reveal it; the label
 * is always part of the accessible name.
 */
function RouteMember({
  id,
  member,
  mention,
}: {
  id: string;
  member?: InferenceRouteMemberAvailability;
  mention?: RouteMemberMention;
}) {
  const name = mention?.name ?? id;
  const status = projectMemberStatus(member, mention?.status);
  const hollow = status.tone !== "success";

  return (
    <li
      aria-label={`${name}, ${status.label}`}
      className={cn(MEMBER_ROW, hollow && REVEAL_ROW)}
      // Focus is the only way keyboard users reveal the label, so the row takes it.
      // eslint-disable-next-line jsx-a11y/no-noninteractive-tabindex
      tabIndex={hollow ? 0 : undefined}
    >
      <span className="min-w-0 truncate">
        {mention ? (
          <PanelMention colorKey={mention.colorKey} href={mention.href}>
            {name}
          </PanelMention>
        ) : (
          name
        )}
      </span>
      <span className="flex shrink-0 items-center gap-2">
        {hollow ? <StatusLabel slot="member-status" status={status} /> : null}
        <StatusDot hollow={hollow} status={status} tooltip={false} />
      </span>
    </li>
  );
}

/**
 * A tier or member status label in its dot's tone, hidden until its row is hovered or focused, and
 * always shown on devices that cannot hover. It is hidden from assistive technology too, since the
 * row or its dot already carries the label in its name.
 */
function StatusLabel({ slot, status }: { slot: string; status: StatusProjection }) {
  return (
    <span
      aria-hidden="true"
      className="flex shrink-0 opacity-0 transition-opacity group-hover:opacity-100 group-focus:opacity-100 motion-reduce:transition-none [@media(hover:none)]:opacity-100"
      data-slot={slot}
    >
      <StatusText status={status} />
    </span>
  );
}

export function RouteConnectionView({ name }: { name: string }) {
  const origin = consoleInferenceOrigin();
  const examples = buildModelConnectionExamples(origin, name);

  return (
    <div className="inspector-sections" data-testid="route-connection-view">
      <section aria-labelledby="route-connect-endpoint-heading">
        <InspectorSectionHeader headingId="route-connect-endpoint-heading" title="Endpoint" />
        <DetailList>
          <Detail label="Route name" value={name} />
          <Detail label="Inference API origin" value={origin} />
        </DetailList>
      </section>
      <ConnectionExample label="OpenCode" value={examples.opencodeCommand} />
      <ConnectionExample label="Pi" value={examples.piCommand} />
      <ConnectionExample label="curl example" value={examples.curl} />
    </div>
  );
}

/** One copyable client example as a section, its copy control in the section's action slot. */
function ConnectionExample({ label, value }: { label: string; value: string }) {
  const headingId = `route-connect-${headingSlug(label)}-heading`;

  return (
    <section aria-labelledby={headingId}>
      <InspectorSectionHeader
        action={<CodeCopyControl label={label} value={value} />}
        headingId={headingId}
        title={label}
      />
      <pre
        aria-label={label}
        className="mt-3 overflow-x-auto whitespace-pre-wrap break-words rounded-md border bg-muted/40 p-3 text-xs leading-5"
      >
        <code>{value}</code>
      </pre>
    </section>
  );
}

/** A DOM-id fragment for a heading, since a label may carry spaces an id must not. */
function headingSlug(label: string): string {
  return label.toLowerCase().replace(/[^a-z0-9]+/g, "-");
}

/** A row that reveals its status label on hover or focus, and shows where focus is. */
const REVEAL_ROW =
  "group outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/50";

/** A member row's geometry, matching a static `InspectorRow` so tiers read like other row lists. */
const MEMBER_ROW =
  "flex min-h-11 items-center justify-between gap-3 border-b border-border px-3 py-2.5 text-sm font-medium last:border-b-0";
