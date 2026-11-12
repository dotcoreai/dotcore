import { Link2Icon, Unlink2Icon } from "lucide-react";
import type { RefObject } from "react";

import { Button } from "@/components/ui/button";
import { FieldError } from "@/components/ui/field-error";
import { Input } from "@/components/ui/input";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

import type { DeploymentExposureValues } from "./deployment-exposure";

/**
 * The exposure fields of a Deploy step: the Direct / Hidden choice, in advanced mode or whenever
 * the deployment is Hidden so basic mode never hides that choice, then the route name input. While
 * `values.routeName` is undefined the input shows `defaultRouteName` in muted text and follows the
 * deployment name; typing makes it a custom value. A toggle beside the label shows that state and
 * switches between following and custom. A Hidden deployment keeps the route name, disabled and
 * without the toggle.
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
    <>
      {advanced || !direct ? (
        <fieldset className="grid gap-2 text-sm">
          <legend className="mb-1">Exposure</legend>
          <ExposureRadio
            checked={values.exposure === "direct"}
            disabled={disabled}
            label="Direct"
            onSelect={() => setExposure("direct")}
          />
          <ExposureRadio
            checked={values.exposure === "hidden"}
            disabled={disabled}
            label="Hidden"
            onSelect={() => setExposure("hidden")}
          />
        </fieldset>
      ) : null}
      <div className="grid gap-1 text-sm">
        <div className="flex min-h-control-xs items-center gap-1">
          <label htmlFor="deployment-route-name">Route name</label>
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
        </div>
        <Input
          required
          ref={routeNameRef}
          aria-describedby={routeNameError ? "deployment-route-name-error" : undefined}
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
      </div>
    </>
  );
}

function ExposureRadio({
  checked,
  disabled,
  label,
  onSelect,
}: {
  checked: boolean;
  disabled: boolean;
  label: string;
  onSelect: () => void;
}) {
  return (
    <label className="flex items-start gap-2">
      <input
        aria-label={label}
        checked={checked}
        className="mt-0.5 size-4"
        disabled={disabled}
        name="deployment-exposure"
        onChange={onSelect}
        type="radio"
        value={label.toLowerCase()}
      />
      <span className="font-medium">{label}</span>
    </label>
  );
}
