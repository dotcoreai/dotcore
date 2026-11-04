import { Fragment } from "react";

import { ResourceCard } from "@/components/ui/resource-card";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";

import type { RouteMemberMention } from "./route-inspector";
import { projectRouteHealth } from "./route-projection";

/**
 * One route in the Routes collection: its name, its status badge, and its routing flow on at most
 * two lines, `primary members → fallback members`. `deploymentMentions` maps the model deployment
 * IDs the console knows to their name; a member it cannot name shows its ID.
 */
export function RouteCard({
  deploymentMentions,
  onSelect,
  panelId,
  selected,
  view,
}: {
  deploymentMentions: ReadonlyMap<string, RouteMemberMention>;
  onSelect: (opener: HTMLElement) => void;
  panelId: string;
  selected: boolean;
  view: InferenceRouteView;
}) {
  const route = view.route;
  if (!route) return null;

  return (
    <li className="min-w-0" data-testid={`route-${route.name}`}>
      <ResourceCard
        accessibleName={route.name}
        colorKey={route.inferenceRouteId}
        description={<RouteFlow deploymentMentions={deploymentMentions} view={view} />}
        name={route.name}
        onSelect={onSelect}
        panelId={panelId}
        selected={selected}
        status={projectRouteHealth(view).status}
      />
    </li>
  );
}

/**
 * The route's groups in priority order, members joined by commas and groups by arrows. A member the
 * controller reported as not eligible is struck through, with its state kept for assistive
 * technology since a strike-through is visual only.
 */
function RouteFlow({
  deploymentMentions,
  view,
}: {
  deploymentMentions: ReadonlyMap<string, RouteMemberMention>;
  view: InferenceRouteView;
}) {
  const groups = view.route!.fallbackGroups.map((group, groupIndex) => {
    const observed = view.availability?.fallbackGroups.find(
      (candidate) => candidate.fallbackGroupIndex === groupIndex,
    );

    return group.modelDeploymentIds.map((id) => ({
      id,
      name: deploymentMentions.get(id)?.name ?? id,
      ready: observed?.members.find((member) => member.modelDeploymentId === id)?.eligible,
    }));
  });
  const text = groups.map((members) => members.map((member) => member.name).join(", ")).join(" → ");

  return (
    <span className="line-clamp-2" data-testid="route-flow" title={text}>
      {groups.map((members, groupIndex) => (
        <Fragment key={groupIndex}>
          {groupIndex > 0 ? " → " : null}
          {members.map((member, memberIndex) => (
            <Fragment key={member.id}>
              {memberIndex > 0 ? ", " : null}
              {member.ready === false ? (
                <span className="text-muted-foreground line-through" data-ready="false">
                  {member.name}
                  <span className="sr-only"> (not ready)</span>
                </span>
              ) : (
                <span>{member.name}</span>
              )}
            </Fragment>
          ))}
        </Fragment>
      ))}
    </span>
  );
}
