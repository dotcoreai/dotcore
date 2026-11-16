import { Link2Icon, Unlink2Icon } from "lucide-react";
import type { RefObject } from "react";

import { Button } from "@/components/ui/button";
import { FieldHelp } from "@/components/ui/field";
import { FieldError } from "@/components/ui/field-error";
import { Input } from "@/components/ui/input";
import { SegmentedControl, type SegmentedControlOption } from "@/components/ui/segmented-control";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

import type { DeploymentExposureValues } from "./deployment-exposure";

/**
 * The route name field of a Deploy step, with its help popover and the deployment's exposure choice
 * beside its label.
 * While `values.routeName` is undefined the input shows `defaultRouteName` in muted text and
 * follows the deployment name; typing makes it a custom value, and a toggle beside the label shows
 * that state and switches between following and custom. The Direct / Hidden choice sits at the
 * end of the label row, in advanced mode or whenever the deployment is Hidden so basic mode never
 * hides that choice. A Hidden deployment keeps the route name, disabled, without the toggle and
 * with a note that it has no route of its own.
 */
export function DeploymentExposureFields({
  advanced,
  values,
  defaultRouteName,
  onChange,
  onRouteNameBlur,
  routeNameError,
  routeNameRef,
  disabled = false,
}: {
  advanced: boolean;
  values: DeploymentExposureValues;
  defaultRouteName: string;
  onChange: (values: DeploymentExposureValues) => void;
  onRouteNameBlur?: (values: DeploymentExposureValues) => void;
  routeNameError?: string;
  routeNameRef?: RefObject<HTMLInputElement | null>;
  disabled?: boolean;
}) {
  const direct = values.exposure === "direct";
  const routeNameDisabled = disabled || !direct;
  const following = values.routeName === undefined;
  const showExposureChoice = advanced || !direct;
  const describedBy =
    [
      routeNameDescriptionId,
      routeNameError ? "deployment-route-name-error" : undefined,
      direct ? undefined : hiddenNoteId,
    ]
      .filter(Boolean)
      .join(" ") || undefined;
  const setExposure = (exposure: DeploymentExposureValues["exposure"]) =>
    onChange({ ...values, exposure });
  const followDeploymentName = () => {
    const next = { ...values, routeName: undefined };
    onChange(next);
    onRouteNameBlur?.(next);
  };
  // Unlinking keeps the shown name as the custom value, so the operator edits it in place.
  const customizeRouteName = () => {
    onChange({ ...values, routeName: defaultRouteName });
    routeNameRef?.current?.focus();
  };
  // An emptied field returns to following instead of submitting an empty route name.
  const handleBlur = () => {
    if (values.routeName !== undefined && !values.routeName.trim()) {
      followDeploymentName();
      return;
    }
    onRouteNameBlur?.(values);
  };

  return (
    <div className="grid gap-1 text-sm">
      <div className="flex min-h-control-xs flex-wrap items-center gap-x-1">
        <label htmlFor="deployment-route-name">Route name</label>
        <FieldHelp descriptionId={routeNameDescriptionId} label="Route name">
          The model name callers send in requests. It follows the deployment name until you change
          it.
        </FieldHelp>
        {direct ? (
          <Tooltip>
            <TooltipTrigger
              render={
                <Button
                  aria-label="Follow the deployment name"
                  aria-pressed={following}
                  className="text-muted-foreground"
                  disabled={disabled}
                  onClick={following ? customizeRouteName : followDeploymentName}
                  size="icon-xs"
                  type="button"
                  variant="ghost"
                />
              }
            >
              {following ? <Link2Icon aria-hidden="true" /> : <Unlink2Icon aria-hidden="true" />}
            </TooltipTrigger>
            <TooltipContent side="right">
              {following
                ? "Follows the deployment name. Click to edit"
                : "Custom name. Click to follow deployment name"}
            </TooltipContent>
          </Tooltip>
        ) : null}
        {showExposureChoice ? (
          <SegmentedControl
            aria-label="Exposure"
            className="ml-auto"
            disabled={disabled}
            onValueChange={setExposure}
            options={EXPOSURE_OPTIONS}
            value={values.exposure}
          />
        ) : null}
      </div>
      <Input
        required
        ref={routeNameRef}
        aria-describedby={describedBy}
        aria-invalid={routeNameError ? "true" : undefined}
        className={cn(following && "text-muted-foreground")}
        disabled={routeNameDisabled}
        id="deployment-route-name"
        onBlur={handleBlur}
        onChange={(event) => onChange({ ...values, routeName: event.target.value })}
        value={values.routeName ?? defaultRouteName}
      />
      {routeNameError ? (
        <FieldError id="deployment-route-name-error">{routeNameError}</FieldError>
      ) : null}
      {direct ? null : (
        <p className="text-sm text-muted-foreground" id={hiddenNoteId}>
          Hidden: no route of its own. Reachable only through other routes.
        </p>
      )}
    </div>
  );
}

const routeNameDescriptionId = "deployment-route-name-description";
const hiddenNoteId = "deployment-route-name-hidden-note";

const EXPOSURE_OPTIONS: readonly SegmentedControlOption<DeploymentExposureValues["exposure"]>[] = [
  { label: "Direct", value: "direct" },
  { label: "Hidden", value: "hidden" },
];
