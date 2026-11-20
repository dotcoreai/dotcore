import type { RefObject } from "react";

import { Field } from "@/components/ui/field";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import { Notice } from "@/components/ui/notice";
import { projectRouteHealth } from "@/domain/route-projection";
import {
  DeploymentForm,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";

interface ApplicationDeployRouteStepProps {
  disabled: boolean;
  error?: string;
  onChange: (routeId: string) => void;
  routeRef: RefObject<HTMLSelectElement | null>;
  /** The routes the application can link to. */
  routes: readonly InferenceRouteView[];
  /** Whether the route view is current; choices wait for it. */
  routesCurrent: boolean;
  /** The linked route's current view; absent when no route is linked or the route was removed. */
  selectedRoute?: InferenceRouteView;
  selectedRouteId: string;
}

/**
 * The Inference route step of the application deploy dialog, shown when the application takes a
 * route: an optional link to one of the site's chat routes, with a warning when the linked route
 * serves nothing right now.
 */
export function ApplicationDeployRouteStep({
  disabled,
  error,
  onChange,
  routeRef,
  routes,
  routesCurrent,
  selectedRoute,
  selectedRouteId,
}: ApplicationDeployRouteStepProps) {
  const servingNotice = selectedRoute ? routeServingNotice(selectedRoute) : undefined;

  return (
    <DeploymentForm>
      <DeploymentStepHeading>Inference route</DeploymentStepHeading>
      <div className="grid gap-3">
        <Field
          controlId="application-inference-route"
          describedBy={servingNotice ? "application-inference-route-serving" : undefined}
          error={error}
          help="Optional. A linked route passes its current name to the application. Without one, the application still reaches the inference API."
          label="Inference route"
        >
          {(control) => (
            <NativeSelect
              {...control}
              ref={routeRef}
              className="w-full"
              disabled={!routesCurrent || disabled}
              onChange={(event) => onChange(event.target.value)}
              value={selectedRouteId}
            >
              <NativeSelectOption value="">No inference route</NativeSelectOption>
              {routes.map((view) =>
                view.route ? (
                  <NativeSelectOption
                    key={view.route.inferenceRouteId}
                    value={view.route.inferenceRouteId}
                  >
                    {routeOptionLabel(view)}
                  </NativeSelectOption>
                ) : null,
              )}
              {selectedRouteId && !selectedRoute ? (
                <NativeSelectOption disabled value={selectedRouteId}>
                  Removed route
                </NativeSelectOption>
              ) : null}
            </NativeSelect>
          )}
        </Field>
        <div>
          {servingNotice ? (
            <Notice className="mt-3" id="application-inference-route-serving" tone="warning">
              {servingNotice}
            </Notice>
          ) : null}
          {!routesCurrent ? (
            <p className="mt-2 text-sm text-muted-foreground" role="status">
              Inference route choices are unavailable until the current route view succeeds.
            </p>
          ) : null}
        </div>
      </div>
    </DeploymentForm>
  );
}

/**
 * A route choice names the route and its current health, so a selectable route is never read as a
 * serving one: linking does not depend on availability.
 */
function routeOptionLabel(view: InferenceRouteView): string {
  const { attention, label } = projectRouteHealth(view).status;
  return `${view.route?.name ?? ""} — ${label}${attention ? ` (${attention})` : ""}`;
}

/** Explains that a linked route serving nothing still deploys, and that its requests fail until it serves. */
function routeServingNotice(view: InferenceRouteView): string | undefined {
  if (!view.route?.enabled)
    return "This route is disabled. The application deploys with the link, but its inference requests fail until the route is enabled.";
  if (!view.availability?.available)
    return "No model serves this route right now. The application deploys with the link, but its inference requests fail until a route member is ready.";
  return undefined;
}
