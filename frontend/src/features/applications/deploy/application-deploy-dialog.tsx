import type { RefObject } from "react";
import { useEffect, useMemo, useRef, useState } from "react";

import { Notice } from "@/components/ui/notice";
import { inferenceRouteBindings } from "@/domain/application-projection";
import { validateDeploymentName } from "@/domain/deployment-name";
import {
  defaultExecutionTarget,
  executionTargetName,
  executionTargetPresentation,
} from "@/domain/execution-target";
import { parameterInputError } from "@/domain/parameter-view";
import { servesChat } from "@/domain/request-kinds";
import { deployWizardShellProps } from "@/features/deployments/deploy-wizard-shell-props";
import { DeploymentWizardShell } from "@/features/deployments/deployment-wizard-shell";
import {
  type ConfiguredExecutionTargetLoader,
  useConfiguredExecutionTargets,
} from "@/features/deployments/use-configured-execution-targets";
import { useDeployWizard } from "@/features/deployments/use-deploy-wizard";
import type { ApplicationStorageChoice } from "@/gen/dotcore/application/v1/service_pb";
import type { Application } from "@/gen/dotcore/catalog/v1/application_pb";
import type { ParameterValue } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import {
  type ConfiguredExecutionTarget,
  ExecutionTargetWorkloadKind,
} from "@/gen/dotcore/site/v1/service_pb";
import { useDirtyFields } from "@/hooks/use-dirty-fields";
import { type FieldErrors, toFieldErrors, withFieldError } from "@/lib/form-validation";

import {
  type ApplicationParameterField,
  applicationParameterFields,
  applicationParameterGroups,
  changedParameterValues,
  defaultParameterInputs,
  enabledParameterFields,
  parameterOverrides,
} from "../application-parameters";
import {
  changedStorageChoices,
  defaultStorageInputs,
  storageChoiceFields,
  storageChoiceKey,
  storageChoiceOverrides,
} from "../application-storage";
import { ApplicationDeployAdvancedStep } from "./application-deploy-advanced-step";
import { ApplicationDeployRouteStep } from "./application-deploy-route-step";
import {
  ApplicationDeploySettingsStep,
  type ApplicationParametersFormProps,
} from "./application-deploy-settings-step";
import { ApplicationDeploySummaryStep } from "./application-deploy-summary-step";
import { ApplicationDeployTargetStep } from "./application-deploy-target-step";

/** Every step in flow order; the wizard shows those with fields, and Target and Advanced per mode. */
const APPLICATION_DEPLOY_STEPS = [
  "Inference route",
  "Target",
  "Settings",
  "Advanced",
  "Deploy",
] as const;

type ApplicationDeployStep = (typeof APPLICATION_DEPLOY_STEPS)[number];

export interface DeploymentCreationOptions {
  applicationDeploymentName?: string;
  inferenceRouteId?: string;
  expectedInferenceRouteGeneration: bigint;
  /** Parameter values the operator changed from their catalog defaults. */
  parameterValues?: readonly ParameterValue[];
  /** Storage placements the operator moved off their catalog default. */
  storageChoices?: readonly ApplicationStorageChoice[];
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
 * Deploys a catalog application through the shared step shape: Inference route when the application
 * takes one, Target, Settings with the basic parameters, Advanced with the advanced parameters and
 * storage choices, and Deploy. Basic mode deploys on the default target unless several execution
 * targets can take the deployment; the Advanced checkbox adds Target and Advanced without
 * discarding entered values.
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
  const dirtyFields = useDirtyFields<DeployField>(open);
  const parameterFields = useMemo(
    () => applicationParameterFields(application.graph?.parameters),
    [application],
  );
  const parameterGroups = useMemo(
    () => applicationParameterGroups(application.graph?.parameterGroups),
    [application],
  );
  const basicParameterFields = parameterFields.filter((field) => !field.advanced);
  const advancedParameterFields = parameterFields.filter((field) => field.advanced);
  const [parameterInputs, setParameterInputs] = useState(() =>
    defaultParameterInputs(parameterFields),
  );
  // Disabled parameters keep their inputs, so switching them back on restores what was typed.
  const enabledParameterIds = new Set(
    enabledParameterFields(parameterFields, parameterGroups, parameterInputs).map(
      (field) => field.view.id,
    ),
  );
  const storageFields = useMemo(() => storageChoiceFields(application), [application]);
  const [storageInputs, setStorageInputs] = useState(() => defaultStorageInputs(storageFields));
  const parameterRefs = useRef(new Map<string, HTMLElement | null>());
  const parameterView = (parameterId: string) =>
    parameterFields.find((field) => field.view.id === parameterId)?.view;
  const [executionTargetId, setExecutionTargetId] = useState("");
  const [targetValidationPending, setTargetValidationPending] = useState(false);
  const [deploymentName, setDeploymentName] = useState(application.displayName);
  const [selectedRouteId, setSelectedRouteId] = useState("");
  const [fieldErrors, setFieldErrors] = useState<FieldErrors>(new Map());
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
  const creationOptions = buildCreationOptions(selectedRoute);

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
  const parameterErrors = (
    fields: readonly ApplicationParameterField[],
  ): [string, string | undefined][] =>
    fields
      .filter((field) => enabledParameterIds.has(field.view.id))
      .map((field) => [
        parameterErrorKey(field.view.id),
        parameterInputError(field.view, parameterInputs.get(field.view.id) ?? ""),
      ]);
  const stepErrors = (activeStep: ApplicationDeployStep): FieldErrors => {
    switch (activeStep) {
      case "Inference route":
        return toFieldErrors([["inferenceRoute", routeSelectionError]]);
      case "Target":
        return toFieldErrors([
          ["placement", targetIsEligible ? undefined : "Select an eligible execution target."],
        ]);
      case "Settings":
        return toFieldErrors(parameterErrors(basicParameterFields));
      case "Advanced":
        return toFieldErrors(parameterErrors(advancedParameterFields));
      case "Deploy":
        return toFieldErrors([["name", validateDeploymentName(deploymentName)]]);
    }
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
  const severalTargets = deployableExecutionTargets.length > 1;
  const wizard = useDeployWizard<ApplicationDeployStep>({
    advancedSteps: ["Advanced"],
    // Basic mode asks for the target only when several can take the deployment.
    basicShows: (candidate) => candidate !== "Target" || severalTargets,
    errorsFor: stepErrors,
    fieldControls,
    hasFields: (candidate) => {
      switch (candidate) {
        case "Inference route":
          return routeBindings.length > 0;
        case "Settings":
          return basicParameterFields.length > 0;
        case "Advanced":
          return advancedParameterFields.length > 0 || storageFields.length > 0;
        case "Target":
        case "Deploy":
          return true;
      }
    },
    onSubmit: () => void submit(),
    open,
    order: APPLICATION_DEPLOY_STEPS,
    setFieldErrors,
  });
  const { step } = wizard;
  // Converted once per blocked navigation, since a new map is what opens the groups.
  const revealedParameterErrors = useMemo(
    () => parameterIdErrors(wizard.revealedErrors),
    [wizard.revealedErrors],
  );

  const submit = async () => {
    if (wizard.revealFirstError()) return;
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
      parameterValues: parameterOverrides(parameterFields, parameterGroups, parameterInputs),
      storageChoices: storageChoiceOverrides(storageFields, storageInputs),
    });
  };
  const routeUnavailable =
    (Boolean(selectedRouteId) && !routesCurrent) || Boolean(routeSelectionError);
  const targetsUnavailable =
    !canCreate || !effectiveExecutionTargetsCurrent || !deployableExecutionTargets.length;
  // A linked route's generation must come from a current route view; no link needs none. An
  // ineligible target leaves Deploy enabled, so Deploy opens the Target step to choose another.
  const primaryDisabled =
    pending ||
    (step === "Inference route"
      ? routeUnavailable
      : step === "Target"
        ? targetsUnavailable
        : step === "Deploy" && (targetValidationPending || targetsUnavailable || routeUnavailable));
  const parametersForm: ApplicationParametersFormProps = {
    errors: parameterIdErrors(fieldErrors),
    groups: parameterGroups,
    inputRefs: parameterRefs,
    inputs: parameterInputs,
    onBlur: (parameterId) => {
      const view = parameterView(parameterId);
      const key = parameterErrorKey(parameterId);
      if (view && dirtyFields.isDirty(key))
        setTimeout(() =>
          setFieldError(key, parameterInputError(view, parameterInputs.get(parameterId) ?? "")),
        );
    },
    onChange: (parameterId, input) => {
      const key = parameterErrorKey(parameterId);
      dirtyFields.markDirty(key);
      setParameterInputs((current) => new Map(current).set(parameterId, input));
      const view = parameterView(parameterId);
      if (view && fieldErrors.has(key)) setFieldError(key, parameterInputError(view, input));
    },
    revealedErrors: revealedParameterErrors,
  };
  const targetNotice = !effectiveExecutionTargetsCurrent
    ? undefined
    : !orderedExecutionTargets.length
      ? "No application execution targets are configured."
      : !deployableExecutionTargets.length
        ? "Configured targets are visible, but none are deployable."
        : targetIsEligible
          ? undefined
          : `The selected execution target is no longer eligible. ${wizard.steps.includes("Target") ? "Return to Target and select an eligible execution target." : "Turn on Advanced to select an eligible execution target."}`;

  return (
    <DeploymentWizardShell
      {...deployWizardShellProps(wizard, { pending })}
      onOpenChange={onOpenChange}
      open={open}
      openerRef={openerRef}
      pending={pending}
      primaryDisabled={primaryDisabled}
      resourceColorKey={application.applicationId}
      resourceName={application.displayName}
      submitError={error}
      title="Deploy application"
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
      {step === "Inference route" ? (
        <ApplicationDeployRouteStep
          disabled={pending}
          error={routeSelectionError}
          onChange={setSelectedRouteId}
          routeRef={routeRef}
          routes={chatRoutes}
          routesCurrent={routesCurrent}
          selectedRoute={selectedRoute}
          selectedRouteId={selectedRouteId}
        />
      ) : null}
      {step === "Target" ? (
        <ApplicationDeployTargetStep
          disabled={pending}
          error={fieldErrors.get("placement")}
          executionTargetId={executionTargetId}
          executionTargets={orderedExecutionTargets}
          inputRef={executionTargetRef}
          onBlur={() => {
            if (dirtyFields.isDirty("placement"))
              setTimeout(() => setFieldError("placement", placementError(executionTargetId)));
          }}
          onChange={(nextExecutionTargetId) => {
            dirtyFields.markDirty("placement");
            setExecutionTargetId(nextExecutionTargetId);
            if (fieldErrors.has("placement"))
              setFieldError("placement", placementError(nextExecutionTargetId));
          }}
          selectedExecutionTarget={selectedExecutionTarget}
          targetsCurrent={effectiveExecutionTargetsCurrent}
        />
      ) : null}
      {step === "Settings" ? (
        <ApplicationDeploySettingsStep fields={basicParameterFields} form={parametersForm} />
      ) : null}
      {step === "Advanced" ? (
        <ApplicationDeployAdvancedStep
          fields={advancedParameterFields}
          form={parametersForm}
          onStorageChange={(field, placement) =>
            setStorageInputs((current) => new Map(current).set(storageChoiceKey(field), placement))
          }
          storageFields={storageFields}
          storageInputs={storageInputs}
        />
      ) : null}
      {step === "Deploy" ? (
        <ApplicationDeploySummaryStep
          changedParameters={changedParameterValues(
            parameterFields,
            parameterGroups,
            parameterInputs,
          )}
          changedStorage={changedStorageChoices(storageFields, storageInputs)}
          disabled={pending}
          name={deploymentName}
          nameError={fieldErrors.get("name")}
          nameRef={deploymentNameRef}
          onNameBlur={() => {
            if (dirtyFields.isDirty("name"))
              setTimeout(() => setFieldError("name", validateDeploymentName(deploymentName)));
          }}
          onNameChange={(nextName) => {
            dirtyFields.markDirty("name");
            setDeploymentName(nextName);
            if (fieldErrors.has("name")) setFieldError("name", validateDeploymentName(nextName));
          }}
          route={
            routeBindings.length
              ? { error: routeSelectionError, selected: selectedRoute, selectedId: selectedRouteId }
              : undefined
          }
          target={selectedExecutionTarget}
          targetLabel={
            selectedExecutionTarget
              ? executionTargetName(selectedExecutionTarget)
              : executionTargetId || "Unavailable"
          }
          targetNotice={targetNotice}
        />
      ) : null}
    </DeploymentWizardShell>
  );
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

// The parameter errors of the dialog's field-error map, keyed by parameter ID.
function parameterIdErrors(errors: FieldErrors): FieldErrors {
  return new Map(
    [...errors].flatMap(([key, error]): [string, string][] =>
      key.startsWith("parameter:") ? [[key.slice("parameter:".length), error]] : [],
    ),
  );
}

function buildCreationOptions(route: InferenceRouteView | undefined): DeploymentCreationOptions {
  return {
    inferenceRouteId: route?.route?.inferenceRouteId || undefined,
    expectedInferenceRouteGeneration: route?.route?.generation ?? 0n,
  };
}
