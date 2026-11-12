import type { RefObject } from "react";
import { useEffect, useMemo, useRef, useState } from "react";

import { parameterDisplayValue } from "@/components/parameters/parameter-view";
import { ParametersForm } from "@/components/parameters/parameters";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { Field } from "@/components/ui/field";
import { FieldError } from "@/components/ui/field-error";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import { Notice } from "@/components/ui/notice";
import { StatusBadge } from "@/components/ui/status-badge";
import { inferenceRouteBindings } from "@/domain/application-projection";
import { validateDeploymentName } from "@/domain/deployment-name";
import {
  configuredExecutionTargetOptionLabel,
  defaultExecutionTarget,
  executionTargetName,
  executionTargetPresentation,
} from "@/domain/execution-target";
import { servesChat } from "@/domain/request-kinds";
import { projectRouteHealth } from "@/domain/route-projection";
import { AdvancedToggle } from "@/features/deployments/advanced-toggle";
import {
  DeploymentForm,
  DeploymentFormSection,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import { DeploymentSummaryStep } from "@/features/deployments/deployment-summary-step";
import { DeploymentWizardShell } from "@/features/deployments/deployment-wizard-shell";
import { ExecutionTargetSummary } from "@/features/deployments/execution-target-presentation";
import { useAdvancedMode } from "@/features/deployments/use-advanced-mode";
import {
  type ConfiguredExecutionTargetLoader,
  useConfiguredExecutionTargets,
} from "@/features/deployments/use-configured-execution-targets";
import { useWizardSteps } from "@/features/deployments/use-wizard-steps";
import type { Application } from "@/gen/dotcore/catalog/v1/application_pb";
import type { ParameterValue } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import {
  type ConfiguredExecutionTarget,
  ExecutionTargetWorkloadKind,
} from "@/gen/dotcore/site/v1/service_pb";
import { useDirtyFields } from "@/hooks/use-dirty-fields";
import { type FieldErrors, focusFirstInvalidField, toFieldErrors } from "@/lib/form-validation";

import { formatGpuDevices } from "./application-gpu-devices";
import {
  applicationParameterFields,
  changedParameterValues,
  defaultParameterInputs,
  parameterInputError,
  parameterOverrides,
} from "./application-parameters";

export interface DeploymentCreationOptions {
  applicationDeploymentName?: string;
  inferenceRouteId?: string;
  expectedInferenceRouteGeneration: bigint;
  /** Parameter values the operator changed from their catalog defaults. */
  parameterValues?: readonly ParameterValue[];
}

export interface ApplicationDeployDialogProps {
  application: Application;
  error?: string;
  onOpenChange: (open: boolean) => void;
  onSubmit: (
    placement: { siteNodeId: string; executionTargetId: string },
    options: DeploymentCreationOptions,
  ) => void;
  open: boolean;
  pending: boolean;
  executionTargets: readonly ConfiguredExecutionTarget[];
  listConfiguredExecutionTargets?: ConfiguredExecutionTargetLoader;
  routes?: readonly InferenceRouteView[];
  canCreate?: boolean;
  executionTargetError?: string;
  executionTargetsCurrent?: boolean;
  openerRef?: RefObject<HTMLElement | null>;
  routeError?: string;
  routesCurrent?: boolean;
}

/**
 * Deploys a catalog application. Basic mode asks for the inference route link, when the application
 * takes one, and the deployment name, adding the Target step only when several execution targets
 * can take the deployment; otherwise it deploys on the default target. The Advanced checkbox always
 * adds the Target step without discarding entered values.
 */
export function ApplicationDeployDialog({
  application,
  error,
  onOpenChange,
  onSubmit,
  open,
  pending,
  executionTargets,
  listConfiguredExecutionTargets,
  routes = [],
  canCreate = true,
  executionTargetError,
  executionTargetsCurrent = true,
  openerRef,
  routeError,
  routesCurrent = true,
}: ApplicationDeployDialogProps) {
  const refreshedTargets = useConfiguredExecutionTargets(listConfiguredExecutionTargets, open, {
    initialCurrent: executionTargetsCurrent,
    initialExecutionTargets: executionTargets,
  });
  const effectiveExecutionTargets = listConfiguredExecutionTargets
    ? refreshedTargets.executionTargets
    : executionTargets;
  const effectiveExecutionTargetsCurrent = listConfiguredExecutionTargets
    ? refreshedTargets.current
    : executionTargetsCurrent;
  const effectiveExecutionTargetError = listConfiguredExecutionTargets
    ? (executionTargetError ?? refreshedTargets.error)
    : executionTargetError;
  const routeBindings = useMemo(() => inferenceRouteBindings(application), [application]);
  const orderedExecutionTargets = useMemo(
    () =>
      orderConfiguredExecutionTargets(
        effectiveExecutionTargets.filter(
          (target) => target.workloadKind === ExecutionTargetWorkloadKind.APPLICATION,
        ),
      ),
    [effectiveExecutionTargets],
  );
  const deployableExecutionTargets = useMemo(
    () =>
      orderedExecutionTargets.filter((target) => executionTargetPresentation(target).deployable),
    [orderedExecutionTargets],
  );
  const firstStep = routeBindings.length ? "Inference route" : "Quick deploy";
  const advancedMode = useAdvancedMode(open);
  const { advanced } = advancedMode;
  const dirtyFields = useDirtyFields<DeployField>(open);
  const parameterFields = useMemo(
    () => applicationParameterFields(application.graph?.parameters),
    [application],
  );
  const visibleParameterFields = parameterFields.filter((field) => advanced || !field.advanced);
  const [parameterInputs, setParameterInputs] = useState(() =>
    defaultParameterInputs(parameterFields),
  );
  const parameterRefs = useRef(new Map<string, HTMLElement | null>());
  const parameterView = (parameterId: string) =>
    parameterFields.find((field) => field.view.id === parameterId)?.view;
  // Whether the Target step shows, fixed when leaving the first step so the steps ahead stay put.
  const [lockedTargetStep, setLockedTargetStep] = useState<boolean>();
  // Basic mode asks for the target only when several can take the deployment.
  const liveTargetStep = deployableExecutionTargets.length > 1;
  const stepsFor = (advancedSteps: boolean) =>
    advancedSteps || (lockedTargetStep ?? liveTargetStep)
      ? [firstStep, "Target", "Deploy"]
      : [firstStep, "Deploy"];
  const steps = stepsFor(advanced);
  const wizard = useWizardSteps(steps, fallbackStep);
  const step = wizard.current;
  const targetStepShown = steps.includes("Target");
  const goToStep = (nextStep: string) => {
    const leavingFirstStep = step === firstStep && nextStep !== firstStep;
    setLockedTargetStep(
      nextStep === firstStep ? undefined : leavingFirstStep ? liveTargetStep : lockedTargetStep,
    );
    wizard.goTo(nextStep);
  };
  const [executionTargetId, setExecutionTargetId] = useState("");
  const [targetValidationPending, setTargetValidationPending] = useState(false);
  const [deploymentName, setDeploymentName] = useState(application.displayName);
  const [selectedRouteId, setSelectedRouteId] = useState("");
  const [fieldErrors, setFieldErrors] = useState<FieldErrors>(new Map());
  const pendingFocusRef = useRef<FieldErrors | undefined>(undefined);
  const targetDefaultAppliedRef = useRef(false);
  const deploymentNameRef = useRef<HTMLInputElement>(null);
  const executionTargetRef = useRef<HTMLSelectElement>(null);
  const routeRef = useRef<HTMLSelectElement>(null);
  const selectedExecutionTarget = orderedExecutionTargets.find(
    (target) => target.executionTargetId === executionTargetId,
  );
  const defaultTargetId = defaultExecutionTarget(orderedExecutionTargets)?.executionTargetId ?? "";
  const chatRoutes = routes.filter((view) => servesChat(view.requestKinds));
  const selectedRoute = routes.find((view) => view.route?.inferenceRouteId === selectedRouteId);
  const targetIsEligible = Boolean(
    selectedExecutionTarget && executionTargetPresentation(selectedExecutionTarget).deployable,
  );
  const routeSelectionError = validateApplicationRouteSelection(selectedRouteId, selectedRoute);
  const selectedRouteServingNotice = selectedRoute ? routeServingNotice(selectedRoute) : undefined;
  const creationOptions = buildCreationOptions(selectedRoute);
  const selectedExecutionTargetLabel = selectedExecutionTarget
    ? executionTargetName(selectedExecutionTarget)
    : executionTargetId || "Unavailable";

  useEffect(() => {
    if (
      !open ||
      !effectiveExecutionTargetsCurrent ||
      targetDefaultAppliedRef.current ||
      !defaultTargetId
    )
      return;
    targetDefaultAppliedRef.current = true;
    queueMicrotask(() => setExecutionTargetId(defaultTargetId));
  }, [defaultTargetId, effectiveExecutionTargetsCurrent, open]);

  useEffect(() => {
    if (!effectiveExecutionTargetsCurrent || !executionTargetId || targetIsEligible) return;
    queueMicrotask(() => {
      setExecutionTargetId("");
      setFieldErrors((current) =>
        withFieldError(current, "placement", "Select an eligible execution target."),
      );
    });
  }, [executionTargetId, targetIsEligible, effectiveExecutionTargetsCurrent]);

  const setFieldError = (field: string, nextError: string | undefined) =>
    setFieldErrors((current) => withFieldError(current, field, nextError));
  const placementError = (targetId: string) =>
    orderedExecutionTargets.some(
      (target) =>
        target.executionTargetId === targetId && executionTargetPresentation(target).deployable,
    )
      ? undefined
      : "Select an eligible execution target.";
  const stepErrors = (activeStep: string): FieldErrors => {
    if (activeStep === firstStep)
      return toFieldErrors([
        ["inferenceRoute", routeBindings.length ? routeSelectionError : undefined],
        ...parameterFields.map((field): [string, string | undefined] => [
          parameterErrorKey(field.view.id),
          parameterInputError(field.view, parameterInputs.get(field.view.id) ?? ""),
        ]),
      ]);
    if (activeStep === "Target")
      return toFieldErrors([
        ["placement", targetIsEligible ? undefined : "Select an eligible execution target."],
      ]);
    return toFieldErrors([["name", validateDeploymentName(deploymentName)]]);
  };
  const fieldControls = () =>
    new Map<string, HTMLElement | null>([
      ["inferenceRoute", routeRef.current],
      ...parameterFields.map((field): [string, HTMLElement | null] => [
        parameterErrorKey(field.view.id),
        parameterRefs.current.get(field.view.id) ?? null,
      ]),
      ["name", deploymentNameRef.current],
      ["placement", executionTargetRef.current],
    ]);
  // Turns Advanced on when an error belongs to a hidden advanced parameter, deferring the focus
  // until the field renders.
  const revealHiddenParameters = (errors: FieldErrors): boolean => {
    const hidden =
      !advanced &&
      parameterFields.some(
        (field) => field.advanced && errors.has(parameterErrorKey(field.view.id)),
      );
    if (hidden) {
      advancedMode.enable("Advanced on to fix parameters");
      pendingFocusRef.current = errors;
    }
    return hidden;
  };
  const exposeErrors = (errors: FieldErrors): boolean => {
    setFieldErrors(errors);
    if (!errors.size) return true;
    if (!revealHiddenParameters(errors)) focusFirstInvalidField(errors, fieldControls());
    return false;
  };
  // Opens the step that owns the errors, turning Advanced on when that step is hidden, and focuses
  // the first invalid field once the step renders.
  const revealErrors = (owner: string, errors: FieldErrors) => {
    setFieldErrors(errors);
    revealHiddenParameters(errors);
    if (!steps.includes(owner))
      advancedMode.enable(`Advanced on to fix ${owner.toLocaleLowerCase()} settings`);
    pendingFocusRef.current = errors;
    if (owner !== step) goToStep(owner);
  };
  // Runs after the shell focuses a new step heading, so the invalid field keeps focus.
  useEffect(() => {
    const errors = pendingFocusRef.current;
    if (!errors) return;
    pendingFocusRef.current = undefined;
    focusFirstInvalidField(errors, fieldControls());
  });

  const submit = async () => {
    for (const owner of stepsFor(true)) {
      const errors = stepErrors(owner);
      if (errors.size) {
        revealErrors(owner, errors);
        return;
      }
    }
    if (pending || !selectedExecutionTarget || !targetIsEligible) return;

    const selectedPlacement = {
      siteNodeId: selectedExecutionTarget.siteNodeId,
      executionTargetId: selectedExecutionTarget.executionTargetId,
    };
    if (listConfiguredExecutionTargets) {
      setTargetValidationPending(true);
      const freshTargets = await refreshedTargets.refresh();
      setTargetValidationPending(false);
      const freshTarget = freshTargets?.find(
        (target) =>
          target.workloadKind === ExecutionTargetWorkloadKind.APPLICATION &&
          target.siteNodeId === selectedPlacement.siteNodeId &&
          target.executionTargetId === selectedPlacement.executionTargetId,
      );
      if (!freshTarget || !executionTargetPresentation(freshTarget).deployable) {
        setExecutionTargetId("");
        setFieldError("placement", "Select an eligible execution target.");
        return;
      }
    }

    onSubmit(selectedPlacement, {
      ...creationOptions,
      applicationDeploymentName: deploymentName.trim(),
      parameterValues: parameterOverrides(parameterFields, parameterInputs),
    });
  };
  const toggleAdvanced = (nextAdvanced: boolean) =>
    advancedMode.toggle(nextAdvanced, stepsFor(nextAdvanced).length);
  const handleStepChange = (nextStep: string) => {
    if (steps.indexOf(nextStep) > steps.indexOf(step) && !exposeErrors(stepErrors(step))) return;
    goToStep(nextStep);
  };
  const handlePrimary = () => {
    if (step === "Deploy") {
      void submit();
      return;
    }
    if (exposeErrors(stepErrors(step))) goToStep(steps[steps.indexOf(step) + 1]);
  };
  const routeUnavailable =
    (Boolean(selectedRouteId) && !routesCurrent) || Boolean(routeSelectionError);
  const targetsUnavailable =
    !canCreate || !effectiveExecutionTargetsCurrent || !deployableExecutionTargets.length;
  // A linked route's generation must come from a current route view; no link needs none. An
  // ineligible target leaves Deploy enabled, so Deploy opens the Target step to choose another.
  const primaryDisabled =
    pending ||
    (step === firstStep
      ? routeUnavailable
      : step === "Target"
        ? targetsUnavailable
        : targetValidationPending || targetsUnavailable || routeUnavailable);
  const parametersSection = visibleParameterFields.length ? (
    <DeploymentFormSection title="Parameters">
      <ParametersForm
        errors={
          new Map(
            parameterFields.flatMap((field) => {
              const fieldError = fieldErrors.get(parameterErrorKey(field.view.id));
              return fieldError ? [[field.view.id, fieldError]] : [];
            }),
          )
        }
        idPrefix="application-parameter"
        inputRefs={parameterRefs}
        inputs={parameterInputs}
        onBlur={(parameterId) => {
          const view = parameterView(parameterId);
          const key = parameterErrorKey(parameterId);
          if (view && dirtyFields.isDirty(key))
            setTimeout(() =>
              setFieldError(key, parameterInputError(view, parameterInputs.get(parameterId) ?? "")),
            );
        }}
        onChange={(parameterId, input) => {
          const key = parameterErrorKey(parameterId);
          dirtyFields.markDirty(key);
          setParameterInputs((current) => new Map(current).set(parameterId, input));
          const view = parameterView(parameterId);
          if (view && fieldErrors.has(key)) setFieldError(key, parameterInputError(view, input));
        }}
        parameters={visibleParameterFields.map((field) => field.view)}
      />
    </DeploymentFormSection>
  ) : null;
  const targetNotice = !effectiveExecutionTargetsCurrent
    ? undefined
    : !orderedExecutionTargets.length
      ? "No application execution targets are configured."
      : !deployableExecutionTargets.length
        ? "Configured targets are visible, but none are deployable."
        : targetIsEligible
          ? undefined
          : `The selected execution target is no longer eligible. ${targetStepShown ? "Return to Target and select an eligible execution target." : "Turn on Advanced to select an eligible execution target."}`;

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
      openerRef={openerRef}
      pending={pending}
      primaryDisabled={primaryDisabled}
      primaryLabel={step === "Deploy" ? (pending ? "Deploying…" : "Deploy") : "Next"}
      resourceColorKey={application.applicationId}
      resourceName={application.displayName}
      steps={steps}
      submitError={error}
      title="Deploy application"
      visitedSteps={wizard.visited}
    >
      {effectiveExecutionTargetError ? (
        <Notice aria-label="Execution target status" role="alert" tone="destructive">
          {effectiveExecutionTargetError}
        </Notice>
      ) : null}
      {routeError && routeBindings.length ? (
        <Notice aria-label="Inference routes status" role="alert" tone="destructive">
          {routeError}
        </Notice>
      ) : null}
      {step === firstStep && !routeBindings.length ? (
        <DeploymentForm>
          <DeploymentFormSection focusTarget title="Quick deploy">
            <DetailList>
              <Detail label="Execution target" value={selectedExecutionTargetLabel} />
            </DetailList>
          </DeploymentFormSection>
          {parametersSection}
        </DeploymentForm>
      ) : null}
      {step === firstStep && routeBindings.length ? (
        <DeploymentForm>
          <DeploymentFormSection focusTarget title="Inference route">
            <Field
              controlId="application-inference-route"
              describedBy={
                selectedRouteServingNotice ? "application-inference-route-serving" : undefined
              }
              error={routeSelectionError}
              help="Optional. A linked route passes its current name to the application. Without one, the application still reaches the inference API."
              label="Inference route"
            >
              {(control) => (
                <NativeSelect
                  {...control}
                  ref={routeRef}
                  className="w-full"
                  disabled={!routesCurrent || pending}
                  onChange={(event) => setSelectedRouteId(event.target.value)}
                  value={selectedRouteId}
                >
                  <NativeSelectOption value="">No inference route</NativeSelectOption>
                  {chatRoutes.map((view) =>
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
              {selectedRouteServingNotice ? (
                <Notice className="mt-3" id="application-inference-route-serving" tone="warning">
                  {selectedRouteServingNotice}
                </Notice>
              ) : null}
              {!routesCurrent ? (
                <p className="mt-2 text-sm text-muted-foreground" role="status">
                  Inference route choices are unavailable until the current route view succeeds.
                </p>
              ) : null}
            </div>
          </DeploymentFormSection>
          {parametersSection}
        </DeploymentForm>
      ) : null}
      {step === "Target" ? (
        <div className="grid gap-4">
          <DeploymentStepHeading>Target</DeploymentStepHeading>
          <div>
            <label className="block text-xs font-medium" htmlFor="application-execution-target">
              Execution target
            </label>
            <NativeSelect
              ref={executionTargetRef}
              aria-describedby={
                fieldErrors.get("placement") ? "application-execution-target-error" : undefined
              }
              aria-invalid={fieldErrors.get("placement") ? "true" : undefined}
              className="mt-1 w-full"
              disabled={
                !effectiveExecutionTargetsCurrent || !orderedExecutionTargets.length || pending
              }
              id="application-execution-target"
              onBlur={() => {
                if (dirtyFields.isDirty("placement"))
                  setTimeout(() => setFieldError("placement", placementError(executionTargetId)));
              }}
              onChange={(event) => {
                const nextExecutionTargetId = event.target.value;
                dirtyFields.markDirty("placement");
                setExecutionTargetId(nextExecutionTargetId);
                if (fieldErrors.has("placement"))
                  setFieldError("placement", placementError(nextExecutionTargetId));
              }}
              required
              value={executionTargetId}
            >
              <NativeSelectOption value="">Select an eligible execution target</NativeSelectOption>
              {orderedExecutionTargets.map((target) => (
                <NativeSelectOption
                  disabled={
                    !effectiveExecutionTargetsCurrent ||
                    !executionTargetPresentation(target).deployable
                  }
                  key={target.executionTargetId}
                  value={target.executionTargetId}
                >
                  {applicationTargetOptionLabel(target)}
                </NativeSelectOption>
              ))}
            </NativeSelect>
            {!orderedExecutionTargets.length ? (
              <p className="mt-2 text-sm text-muted-foreground" role="status">
                No application execution targets are configured.
              </p>
            ) : !deployableExecutionTargets.length ? (
              <p className="mt-2 text-sm text-muted-foreground" role="status">
                Configured targets are visible, but none are deployable.
              </p>
            ) : null}
            {fieldErrors.get("placement") ? (
              <FieldError className="mt-2" id="application-execution-target-error">
                {fieldErrors.get("placement")}
              </FieldError>
            ) : null}
          </div>
          {selectedExecutionTarget ? (
            <div className="grid gap-2">
              <ExecutionTargetSummary target={selectedExecutionTarget} />
            </div>
          ) : null}
        </div>
      ) : null}
      {step === "Deploy" ? (
        <DeploymentSummaryStep
          disabled={pending}
          name={deploymentName}
          nameError={fieldErrors.get("name")}
          nameInputId="application-deployment-name"
          nameRef={deploymentNameRef}
          notices={
            <>
              {targetNotice ? (
                <Notice aria-label="Deploy execution target status" role="alert" tone="destructive">
                  {targetNotice}
                </Notice>
              ) : null}
              {routeBindings.length && routeSelectionError ? (
                <Notice aria-label="Deploy route status" role="alert" tone="destructive">
                  {routeSelectionError} Return to Inference route to change the link.
                </Notice>
              ) : null}
            </>
          }
          onNameBlur={() => {
            if (dirtyFields.isDirty("name"))
              setTimeout(() => setFieldError("name", validateDeploymentName(deploymentName)));
          }}
          onNameChange={(nextName) => {
            dirtyFields.markDirty("name");
            setDeploymentName(nextName);
            if (fieldErrors.has("name")) setFieldError("name", validateDeploymentName(nextName));
          }}
        >
          {routeBindings.length ? (
            <Detail
              label="Inference route"
              value={
                selectedRoute?.route ? (
                  <span className="inline-flex flex-wrap items-center justify-end gap-2">
                    {selectedRoute.route.name}
                    <StatusBadge status={projectRouteHealth(selectedRoute).status} />
                  </span>
                ) : selectedRouteId ? (
                  "Removed route"
                ) : (
                  "None"
                )
              }
            />
          ) : null}
          <Detail label="Execution target" value={selectedExecutionTargetLabel} />
          {selectedExecutionTarget ? (
            <Detail
              label="GPU devices"
              value={formatGpuDevices(selectedExecutionTarget.acceleratorDeviceIds)}
            />
          ) : null}
          {changedParameterValues(parameterFields, parameterInputs).map(({ field, value }) => (
            <Detail
              key={field.view.id}
              label={field.view.label}
              value={parameterDisplayValue(field.view, value)}
            />
          ))}
        </DeploymentSummaryStep>
      ) : null}
    </DeploymentWizardShell>
  );
}

// A vanished step falls forward: Target precedes Deploy.
function fallbackStep(): string {
  return "Deploy";
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

/**
 * A deployable choice also names its platform, since a remote machine can run a different platform than the
 * manager. A gated choice already lists its platform after the reason.
 */
function applicationTargetOptionLabel(target: ConfiguredExecutionTarget): string {
  const label = configuredExecutionTargetOptionLabel(target);
  return executionTargetPresentation(target).deployable
    ? `${label} — ${target.platformLabel || "Unknown platform"}`
    : label;
}

function orderConfiguredExecutionTargets(
  executionTargets: readonly ConfiguredExecutionTarget[],
): ConfiguredExecutionTarget[] {
  return [...executionTargets].sort(
    (left, right) =>
      left.siteNodeId.localeCompare(right.siteNodeId) ||
      left.executionTargetId.localeCompare(right.executionTargetId),
  );
}

function validateApplicationRouteSelection(
  routeId: string,
  route: InferenceRouteView | undefined,
): string | undefined {
  if (!routeId || route) return undefined;
  return "The selected inference route no longer exists. Select another route or no route.";
}

// Fields whose dirty state gates validation: the fixed fields and one key per parameter.
type DeployField = "name" | "placement" | `parameter:${string}`;

// Parameter error keys share the dialog's field-error map with its fixed fields.
function parameterErrorKey(parameterId: string): `parameter:${string}` {
  return `parameter:${parameterId}`;
}

function withFieldError(
  errors: FieldErrors,
  field: string,
  nextError: string | undefined,
): FieldErrors {
  const next = new Map(errors);
  if (nextError) next.set(field, nextError);
  else next.delete(field);
  return next;
}

function buildCreationOptions(route: InferenceRouteView | undefined): DeploymentCreationOptions {
  return {
    inferenceRouteId: route?.route?.inferenceRouteId || undefined,
    expectedInferenceRouteGeneration: route?.route?.generation ?? 0n,
  };
}
