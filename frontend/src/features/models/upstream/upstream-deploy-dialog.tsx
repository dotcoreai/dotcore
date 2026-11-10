import { type RefObject, useEffect, useRef, useState } from "react";

import { Detail } from "@/components/ui/detail-list";
import { Notice } from "@/components/ui/notice";
import { validateDeploymentName } from "@/domain/deployment-name";
import { UPSTREAM_MODEL_NAME } from "@/domain/upstream-presentation";
import { AdvancedToggle } from "@/features/deployments/advanced-toggle";
import { DeploymentStepHeading } from "@/features/deployments/deployment-form-layout";
import { DeploymentSummaryStep } from "@/features/deployments/deployment-summary-step";
import { DeploymentWizardShell } from "@/features/deployments/deployment-wizard-shell";
import { useAdvancedMode } from "@/features/deployments/use-advanced-mode";
import { useWizardSteps } from "@/features/deployments/use-wizard-steps";
import {
  deriveRouteName,
  effectiveRouteName,
  routeBlockingMessage,
  type RouteNameConflict,
  validateDeploymentExposure,
} from "@/features/models/deployment-exposure";
import { DeploymentExposureFields } from "@/features/models/deployment-exposure-fields";
import { type UpstreamModelTemplate } from "@/gen/dotcore/catalog/v1/upstream_model_template_pb";
import { useDirtyFields } from "@/hooks/use-dirty-fields";
import { type FieldErrors, focusFirstInvalidField, toFieldErrors } from "@/lib/form-validation";

import { UpstreamEndpointFields } from "./upstream-endpoint-fields";
import {
  defaultUpstreamForm,
  deriveUpstreamDeploymentName,
  type UpstreamFormValues,
  validateModelId,
  validateUpstreamBaseUrl,
} from "./upstream-form";

interface DeployDialogProps {
  error?: string;
  existingRouteNames?: readonly string[];
  onOpenChange: (open: boolean) => void;
  onSubmit: (values: UpstreamFormValues) => void;
  open: boolean;
  pending?: boolean;
  /** The route name the backend rejected as taken on the last deploy attempt. */
  routeConflict?: RouteNameConflict;
  template: UpstreamModelTemplate;
}

/**
 * Deploys an upstream endpoint as a model deployment. Basic mode asks for the connection and the
 * deployment name; the Advanced checkbox adds the Direct / Hidden choice without discarding entered values.
 */
export function UpstreamDeployDialog({
  error,
  existingRouteNames = [],
  onOpenChange,
  onSubmit,
  open,
  pending = false,
  routeConflict,
  template,
}: DeployDialogProps) {
  const [values, setValues] = useState(() => defaultUpstreamForm());
  const [nameFollowsModelId, setNameFollowsModelId] = useState(true);
  const [fieldErrors, setFieldErrors] = useState<FieldErrors>(new Map());
  const dirtyFields = useDirtyFields<UpstreamDeployField>(open);
  const advancedMode = useAdvancedMode(open);
  const { advanced } = advancedMode;
  const steps = UPSTREAM_DEPLOY_STEPS;
  const wizard = useWizardSteps(steps);
  const step = wizard.current as UpstreamDeployStep;
  const pendingFocusRef = useRef<FieldErrors | undefined>(undefined);
  const nameRef = useRef<HTMLInputElement>(null);
  const baseUrlRef = useRef<HTMLInputElement>(null);
  const upstreamModelIdRef = useRef<HTMLInputElement>(null);
  const routeNameRef = useRef<HTMLInputElement>(null);
  const deploymentName = values.name ?? "";
  const routeName = effectiveRouteName(values, deploymentName);
  const validateRoute = (routeValues: UpstreamFormValues) =>
    validateDeploymentExposure(routeValues, routeValues.name ?? "", existingRouteNames);
  const routeBlocking = routeBlockingMessage({
    conflict: routeConflict,
    deploymentName,
    existingRouteNames,
    values,
  });

  const setFieldError = (field: string, nextError: string | undefined) => {
    setFieldErrors((errors) => {
      const nextErrors = new Map(errors);
      if (nextError) nextErrors.set(field, nextError);
      else nextErrors.delete(field);
      return nextErrors;
    });
  };
  const updateValues = (next: UpstreamFormValues) => {
    const upstreamChanged = next.upstreamModelId !== values.upstreamModelId;
    if (!upstreamChanged) {
      if (next.name !== values.name) setNameFollowsModelId(false);
    }
    const normalizedNext = upstreamChanged
      ? {
          ...next,
          name: nameFollowsModelId
            ? deriveUpstreamDeploymentName(next.upstreamModelId)
            : values.name,
        }
      : next;
    setValues(normalizedNext);
    // A name derived from the model ID is not an operator change to the name.
    if (!upstreamChanged && next.name !== values.name) dirtyFields.markDirty("name");
    if (next.baseUrl !== values.baseUrl) dirtyFields.markDirty("baseUrl");
    if (upstreamChanged) dirtyFields.markDirty("upstreamModelId");
    if (next.routeName !== values.routeName) dirtyFields.markDirty("routeName");
    setFieldErrors((errors) => {
      const nextErrors = new Map(errors);
      const updateExposedError = (field: string, nextError: string | undefined) => {
        if (!nextErrors.has(field)) return;
        if (nextError) nextErrors.set(field, nextError);
        else nextErrors.delete(field);
      };

      if (normalizedNext.name !== values.name)
        updateExposedError("name", validateDeploymentName(normalizedNext.name ?? ""));
      if (normalizedNext.baseUrl !== values.baseUrl)
        updateExposedError("baseUrl", validateUpstreamBaseUrl(normalizedNext.baseUrl));
      if (normalizedNext.upstreamModelId !== values.upstreamModelId)
        updateExposedError("upstreamModelId", validateModelId(normalizedNext.upstreamModelId));
      if (normalizedNext.routeName !== values.routeName || normalizedNext.name !== values.name)
        updateExposedError("routeName", validateRoute(normalizedNext));
      return nextErrors;
    });
  };
  const validateField = (field: "name" | "baseUrl" | "upstreamModelId") => {
    const fieldError =
      field === "name"
        ? validateDeploymentName(deploymentName)
        : field === "baseUrl"
          ? validateUpstreamBaseUrl(values.baseUrl)
          : validateModelId(values.upstreamModelId);
    setFieldError(field, fieldError);
  };
  const validateFieldOnBlur = (field: "name" | "baseUrl" | "upstreamModelId") => {
    if (dirtyFields.isDirty(field)) setTimeout(() => validateField(field), 0);
  };
  const stepErrors = (activeStep: UpstreamDeployStep): FieldErrors => {
    switch (activeStep) {
      case "Connection":
        return toFieldErrors([
          ["baseUrl", validateUpstreamBaseUrl(values.baseUrl)],
          ["upstreamModelId", validateModelId(values.upstreamModelId)],
        ]);
      case "Deploy": {
        const nameError = validateDeploymentName(deploymentName);
        // A route following an invalid name is explained by the name's own error.
        const routeError =
          values.routeName === undefined && nameError ? undefined : validateRoute(values);
        return toFieldErrors([
          ["name", nameError],
          ["routeName", routeError],
        ]);
      }
    }
  };
  const fieldControls = () =>
    new Map([
      ["name", nameRef.current],
      ["baseUrl", baseUrlRef.current],
      ["upstreamModelId", upstreamModelIdRef.current],
      ["routeName", routeNameRef.current],
    ]);
  const exposeErrors = (errors: FieldErrors): boolean => {
    setFieldErrors(errors);
    if (!errors.size) return true;
    focusFirstInvalidField(errors, fieldControls());
    return false;
  };
  // Opens the step that owns the errors and focuses the first invalid field once it renders.
  const revealErrors = (owner: UpstreamDeployStep, errors: FieldErrors) => {
    setFieldErrors(errors);
    pendingFocusRef.current = errors;
    if (owner !== step) wizard.goTo(owner);
  };
  const submit = () => {
    for (const owner of UPSTREAM_DEPLOY_STEPS) {
      const errors = stepErrors(owner);
      if (errors.size) {
        revealErrors(owner, errors);
        return;
      }
    }
    if (!pending) onSubmit({ ...values, routeName });
  };
  const toggleAdvanced = (nextAdvanced: boolean) => advancedMode.toggle(nextAdvanced, steps.length);
  const handleStepChange = (nextStep: string) => {
    const forward = steps.indexOf(nextStep as UpstreamDeployStep) > steps.indexOf(step);
    if (forward && !exposeErrors(stepErrors(step))) return;
    wizard.goTo(nextStep);
  };
  const handlePrimary = () => {
    if (step === "Deploy") {
      submit();
      return;
    }
    if (exposeErrors(stepErrors(step))) wizard.goTo(steps[steps.indexOf(step) + 1]);
  };

  // Runs after the shell focuses a new step heading, so the invalid field keeps focus.
  useEffect(() => {
    const errors = pendingFocusRef.current;
    if (!errors) return;
    pendingFocusRef.current = undefined;
    focusFirstInvalidField(errors, fieldControls());
  });

  return (
    <DeploymentWizardShell
      announcement={advancedMode.announcement}
      currentStep={step}
      footerStart={
        <AdvancedToggle checked={advanced} disabled={pending} onChange={toggleAdvanced} />
      }
      onOpenChange={onOpenChange}
      onPrimary={handlePrimary}
      onStepChange={handleStepChange}
      open={open}
      pending={pending}
      primaryDisabled={step === "Deploy" && Boolean(routeBlocking)}
      primaryLabel={step === "Deploy" ? (pending ? "Deploying…" : "Deploy") : "Next"}
      resourceColorKey={template.upstreamModelTemplateId}
      resourceName={UPSTREAM_MODEL_NAME}
      steps={steps}
      submitError={step === "Deploy" ? error : undefined}
      title="Deploy model"
      visitedSteps={wizard.visited}
    >
      {step === "Connection" ? (
        <UpstreamConnectionStep
          baseUrlRef={baseUrlRef}
          fieldErrors={fieldErrors}
          onChange={updateValues}
          onFieldBlur={validateFieldOnBlur}
          upstreamModelIdRef={upstreamModelIdRef}
          values={values}
        />
      ) : null}
      {step === "Deploy" ? (
        <DeploymentSummaryStep
          disabled={pending}
          name={deploymentName}
          nameError={fieldErrors.get("name")}
          nameInputId="upstream-deployment-name"
          nameRef={nameRef}
          onNameBlur={() => validateFieldOnBlur("name")}
          onNameChange={(name) => updateValues({ ...values, name })}
          nameFields={
            <DeploymentExposureFields
              advanced={advanced}
              defaultRouteName={deriveRouteName(deploymentName)}
              disabled={pending}
              onChange={(next) => updateValues({ ...values, ...next })}
              onRouteNameBlur={(next) => {
                if (dirtyFields.isDirty("routeName"))
                  setFieldError("routeName", validateRoute({ ...values, ...next }));
              }}
              routeNameError={
                values.exposure === "direct"
                  ? (routeBlocking ?? fieldErrors.get("routeName"))
                  : undefined
              }
              routeNameRef={routeNameRef}
              values={values}
            />
          }
        >
          <Detail label="Base URL" value={values.baseUrl.trim()} />
          <Detail label="Upstream model ID" value={values.upstreamModelId.trim()} />
          <Detail label="API key" value={values.apiKey ? "Configured" : "Not configured"} />
        </DeploymentSummaryStep>
      ) : null}
    </DeploymentWizardShell>
  );
}

type UpstreamDeployStep = (typeof UPSTREAM_DEPLOY_STEPS)[number];

/** The deploy dialog fields that validate on blur. */
type UpstreamDeployField = "name" | "baseUrl" | "upstreamModelId" | "routeName";

/** Every step in flow order. */
const UPSTREAM_DEPLOY_STEPS = ["Connection", "Deploy"] as const;

function UpstreamConnectionStep({
  baseUrlRef,
  fieldErrors,
  onChange,
  onFieldBlur,
  upstreamModelIdRef,
  values,
}: {
  baseUrlRef: RefObject<HTMLInputElement | null>;
  fieldErrors: FieldErrors;
  onChange: (values: UpstreamFormValues) => void;
  onFieldBlur: (field: "name" | "baseUrl" | "upstreamModelId") => void;
  upstreamModelIdRef: RefObject<HTMLInputElement | null>;
  values: UpstreamFormValues;
}) {
  const insecureHttp = values.baseUrl.trim().toLowerCase().startsWith("http://");

  return (
    <div className="grid gap-4">
      <DeploymentStepHeading>Connection</DeploymentStepHeading>
      <UpstreamEndpointFields
        baseUrlRef={baseUrlRef}
        fieldErrors={fieldErrors}
        onChange={onChange}
        onFieldBlur={onFieldBlur}
        upstreamModelIdRef={upstreamModelIdRef}
        values={values}
      />
      {insecureHttp ? (
        <Notice role="status">
          This endpoint uses unencrypted HTTP. Requests and credentials can be observed in transit.
        </Notice>
      ) : null}
    </div>
  );
}
