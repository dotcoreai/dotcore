import { Link2Icon, Unlink2Icon } from "lucide-react";
import type { RefObject } from "react";

import { Button } from "@/components/ui/button";
import { FieldError } from "@/components/ui/field-error";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";

import type { DeploymentExposureValues } from "./deployment-exposure";

/**
 * The exposure fields of a Deploy step: the Direct / Hidden choice, in advanced mode or whenever
 * the deployment is Hidden so basic mode never hides that choice, then the route name input. While
 * `values.routeName` is undefined the input shows `defaultRouteName` in muted text and follows the
 * deployment name; typing makes it a custom value. A Hidden deployment keeps the route name,
 * disabled and without its follow helper.
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
  const helperId = "deployment-route-name-helper";
  const describedBy = [
    routeNameError ? "deployment-route-name-error" : undefined,
    direct ? helperId : undefined,
  ]
    .filter(Boolean)
    .join(" ");
  const setExposure = (exposure: DeploymentExposureValues["exposure"]) =>
    onChange({ ...values, exposure });
  const followDeploymentName = () => {
    const next = { ...values, routeName: undefined };
    onChange(next);
    onRouteNameBlur?.(next);
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
        <label htmlFor="deployment-route-name">Route name</label>
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
        {direct ? (
          <p
            className="flex flex-wrap items-center gap-x-1.5 text-sm text-muted-foreground"
            id={helperId}
          >
            {following ? (
              <>
                <Link2Icon aria-hidden="true" className="size-3.5" />
                Follows the deployment name.
              </>
            ) : (
              <>
                <Unlink2Icon aria-hidden="true" className="size-3.5" />
                Custom route name.
                <Button
                  className="h-auto p-0"
                  disabled={routeNameDisabled}
                  onClick={followDeploymentName}
                  type="button"
                  variant="link"
                >
                  Use deployment name
                </Button>
              </>
            )}
          </p>
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
