/* eslint-disable react-refresh/only-export-components, jsx-a11y/label-has-associated-control */
import { create } from "@bufbuild/protobuf";
import { Pencil, Trash2 } from "lucide-react";
import { type RefObject, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useLocation } from "react-router-dom";

import type { MetricsClient } from "@/api/metrics-client";
import { InspectorActions } from "@/components/inspector-actions/inspector-actions";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Detail, DetailList } from "@/components/ui/detail-list";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { FieldError } from "@/components/ui/field-error";
import { Input } from "@/components/ui/input";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { Notice } from "@/components/ui/notice";
import { ResourceMarker, ResourceTitle } from "@/components/ui/resource-marker";
import type { ChatTarget } from "@/features/chat/chattable-route";
import { useChatAction } from "@/features/chat/use-chat-action";
import { AdvancedToggle } from "@/features/deployments/advanced-toggle";
import { DeploymentNameSection } from "@/features/deployments/deployment-editor";
import { DeploymentStepHeading } from "@/features/deployments/deployment-form-layout";
import { DeploymentSummaryStep } from "@/features/deployments/deployment-summary-step";
import { DeploymentWizardShell } from "@/features/deployments/deployment-wizard-shell";
import { type RouteMention, RouteMentions } from "@/features/deployments/panel-mention";
import { useAdvancedMode } from "@/features/deployments/use-advanced-mode";
import { useWizardSteps } from "@/features/deployments/use-wizard-steps";
import { deploymentRequestCards } from "@/features/metrics/request-card-specs";
import { RequestCards } from "@/features/metrics/request-cards";
import { type UpstreamModelTemplate } from "@/gen/dotcore/catalog/v1/upstream_model_template_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
  OpenAiCompatibleEndpointConfigurationSchema,
  UpstreamModelDesiredConfigurationSchema,
  UpstreamModelInputCapabilitiesSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { useDirtyFields } from "@/hooks/use-dirty-fields";
import type { DeploymentMetadata, DeploymentMetadataUpdate } from "@/lib/deployment-metadata";
import { validateDeploymentName } from "@/lib/deployment-name";
import { type FieldErrors, focusFirstInvalidField, toFieldErrors } from "@/lib/form-validation";
import { UPSTREAM_MODEL_NAME } from "@/lib/upstream-presentation";
import { usePanelDirtyGuard, usePanelNavigation } from "@/panels/panel-navigation";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";
import { popPanelLocation } from "@/panels/panel-stack";

import {
  type DeploymentExposureValues,
  deriveRouteName,
  effectiveRouteName,
  routeBlockingMessage,
  type RouteNameConflict,
  validateDeploymentExposure,
} from "./deployment-exposure";
import { DeploymentExposureFields } from "./deployment-exposure-fields";
import { modelInspectorPanelId } from "./model-inspector-panel";

export interface UpstreamFormValues extends DeploymentExposureValues {
  name?: string;
  upstreamModelId: string;
  baseUrl: string;
  apiKey: string;
  // These values are intentionally not exposed by the minimal endpoint form.
  supportsText: boolean;
  supportsImage: boolean;
  locality: number;
  trustDomain: string;
  dataEgressClass: number;
  allowInsecureHttp: boolean;
}

export function deriveUpstreamDeploymentName(value: string): string {
  return [...value.trim()].slice(0, 128).join("");
}

export function validateUpstreamBaseUrl(value: string): string | undefined {
  const baseUrl = value.trim();
  if (!baseUrl) return "Base URL is required.";
  if (/^https?:\/\/(?:\/|$)/i.test(baseUrl)) return "Base URL must include a host.";

  let url: URL;
  try {
    url = new URL(baseUrl);
  } catch {
    return "Enter a valid Base URL.";
  }

  if (url.protocol !== "http:" && url.protocol !== "https:")
    return "Base URL must use HTTP or HTTPS.";
  if (!url.hostname) return "Base URL must include a host.";
  if (url.username || url.password) return "Base URL must not include credentials.";
  if (url.search) return "Base URL must not include a query.";
  if (url.hash) return "Base URL must not include a fragment.";
  return undefined;
}

export function validateModelId(value: string): string | undefined {
  return value.trim() ? undefined : "Upstream model ID is required.";
}

export function defaultUpstreamForm(deployment?: ModelDeployment): UpstreamFormValues {
  const config = deployment?.upstreamDesiredConfiguration;
  return {
    exposure: "direct",
    name: deployment?.modelDeploymentName ?? UPSTREAM_MODEL_NAME,
    upstreamModelId: config?.upstreamModelId ?? "",
    baseUrl: config?.endpoint?.baseUrl ?? "",
    // The stored credential is never exposed through a console draft. A blank edit retains it.
    apiKey: "",
    supportsText: config?.inputCapabilities?.supportsText ?? true,
    supportsImage: config?.inputCapabilities?.supportsImage ?? false,
    locality: config?.boundary?.locality ?? 1,
    trustDomain: config?.boundary?.trustDomain ?? "",
    dataEgressClass: config?.boundary?.dataEgressClass ?? 1,
    allowInsecureHttp: config?.endpoint?.allowInsecureHttp ?? false,
  };
}

export function toUpstreamConfiguration(values: UpstreamFormValues) {
  const baseUrl = values.baseUrl.trim();
  return create(UpstreamModelDesiredConfigurationSchema, {
    upstreamModelId: values.upstreamModelId.trim(),
    apiKey: values.apiKey,
    inputCapabilities: create(UpstreamModelInputCapabilitiesSchema, {
      supportsText: values.supportsText,
      supportsImage: values.supportsImage,
    }),
    endpoint: create(OpenAiCompatibleEndpointConfigurationSchema, {
      baseUrl,
      allowInsecureHttp: baseUrl.toLowerCase().startsWith("http://"),
    }),
    boundary: {
      locality: values.locality,
      trustDomain: values.trustDomain.trim() || upstreamTrustDomain(baseUrl),
      dataEgressClass: values.dataEgressClass,
    },
  });
}

function upstreamTrustDomain(baseUrl: string): string {
  try {
    return new URL(baseUrl).hostname;
  } catch {
    return "";
  }
}

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
      <UpstreamForm
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

export function UpstreamEditInspector({
  deployment,
  error,
  onBack,
  onOpenChange,
  metadata,
  onSaveEndpoint,
  onSaveMetadata,
  onSubmit,
  open,
  openerRef,
  pending: initiallyPending = false,
}: {
  deployment: ModelDeployment;
  error?: string;
  onBack: () => void;
  onOpenChange: (open: boolean) => void;
  metadata: DeploymentMetadata;
  onSaveEndpoint?: (values: UpstreamFormValues) => Promise<boolean>;
  onSaveMetadata: (update: DeploymentMetadataUpdate) => Promise<DeploymentMetadata>;
  /** Compatibility callback for isolated endpoint rendering tests. */
  onSubmit?: (values: UpstreamFormValues) => void;
  open: boolean;
  pending?: boolean;
  openerRef: RefObject<HTMLElement | null>;
}) {
  const location = useLocation();
  const panelNavigation = usePanelNavigation();
  const upstreamConfiguration = JSON.stringify(deployment.upstreamDesiredConfiguration);
  const initialName = metadata.displayName;
  const initialValues = useMemo(
    () => defaultUpstreamForm(deployment),
    // Projection equality, rather than object identity, preserves drafts across parent renders.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [upstreamConfiguration],
  );
  const [endpointBaseline, setEndpointBaseline] = useState(initialValues);
  const [endpointDraft, setEndpointDraft] = useState(initialValues);
  const [nameBaseline, setNameBaseline] = useState(initialName);
  const [nameDraft, setNameDraft] = useState(nameBaseline);
  const [endpointError, setEndpointError] = useState<string>();
  const [endpointFieldErrors, setEndpointFieldErrors] = useState<FieldErrors>(new Map());
  const [nameError, setNameError] = useState<string>();
  const [savedSection, setSavedSection] = useState<"name" | "endpoint">();
  const [pending, setPending] = useState(initiallyPending);
  const initializedProjection = useRef(false);
  const nameRef = useRef<HTMLInputElement>(null);
  const baseUrlRef = useRef<HTMLInputElement>(null);
  const upstreamModelIdRef = useRef<HTMLInputElement>(null);
  const endpointDirty = JSON.stringify(endpointDraft) !== JSON.stringify(endpointBaseline);
  const nameDirty = nameDraft !== nameBaseline;
  const dirty = endpointDirty || nameDirty;
  const terminal = deployment.desiredState === ModelDeploymentDesiredState.TEARDOWN;
  const terminalRef = useRef(terminal);

  useLayoutEffect(() => {
    terminalRef.current = terminal;
  }, [terminal]);

  useEffect(() => {
    if (!initializedProjection.current) {
      initializedProjection.current = true;
      return;
    }
    const timer = window.setTimeout(() => {
      // Projection changes must advance accepted baselines without replacing unsaved drafts.
      if (
        endpointValuesEqual(endpointDraft, endpointBaseline) &&
        !endpointValuesEqual(endpointDraft, initialValues)
      ) {
        setEndpointDraft(initialValues);
      }
      if (!endpointValuesEqual(endpointBaseline, initialValues)) {
        setEndpointBaseline(initialValues);
      }
      if (nameDraft === nameBaseline && nameDraft !== metadata.displayName) {
        setNameDraft(metadata.displayName);
      }
      if (nameBaseline !== metadata.displayName) {
        setNameBaseline(metadata.displayName);
      }
    });
    return () => window.clearTimeout(timer);
  }, [
    endpointBaseline,
    endpointDraft,
    initialValues,
    metadata.displayName,
    nameBaseline,
    nameDraft,
  ]);

  usePanelDirtyGuard(dirty && !terminal, () => {
    setEndpointDraft(endpointBaseline);
    setNameDraft(nameBaseline);
    setEndpointError(undefined);
    setNameError(undefined);
  });
  usePanelPendingNavigation(pending, "Saving changes. Inspector navigation is unavailable.");

  const returnToParent = () => {
    panelNavigation.navigatePanel(popPanelLocation(location.pathname, location.search), {
      bypassDirtyGuard: true,
      bypassPendingGuard: true,
      replace: true,
    });
  };
  const saveName = async () => {
    const validationError = validateDeploymentName(nameDraft);
    if (validationError) {
      setNameError(validationError);
      nameRef.current?.focus();
      return;
    }
    setPending(true);
    setNameError(undefined);
    try {
      const accepted = await onSaveMetadata({
        fields: ["displayName"],
        metadata: { displayName: nameDraft.trim() },
      });
      if (terminalRef.current) return;
      setNameBaseline(accepted.displayName);
      setNameDraft(accepted.displayName);
      setSavedSection("name");
      if (endpointDirty) {
        setEndpointError(undefined);
        return;
      }
      returnToParent();
    } catch (saveError) {
      setNameError(saveError instanceof Error ? saveError.message : "Name could not be saved.");
    } finally {
      setPending(false);
    }
  };
  const saveEndpoint = async () => {
    const fieldErrors = new Map<string, string>();
    const baseUrlError = validateUpstreamBaseUrl(endpointDraft.baseUrl);
    if (baseUrlError) fieldErrors.set("baseUrl", baseUrlError);
    const upstreamError = validateModelId(endpointDraft.upstreamModelId.trim());
    if (upstreamError) {
      fieldErrors.set("upstreamModelId", upstreamError);
    }
    if (fieldErrors.size) {
      setEndpointFieldErrors(fieldErrors);
      (fieldErrors.has("baseUrl") ? baseUrlRef : upstreamModelIdRef).current?.focus();
      return;
    }
    setPending(true);
    setEndpointError(undefined);
    setEndpointFieldErrors(new Map());
    try {
      const accepted = onSaveEndpoint
        ? await onSaveEndpoint(endpointDraft)
        : (onSubmit?.(endpointDraft), true);
      if (!accepted) {
        setEndpointError(
          error ?? "Endpoint could not be saved. Check the deployment status and try again.",
        );
        return;
      }
      if (terminalRef.current) return;
      setEndpointBaseline(endpointDraft);
      setSavedSection("endpoint");
      if (nameDirty) {
        setEndpointError(undefined);
        return;
      }
      returnToParent();
    } finally {
      setPending(false);
    }
  };

  return (
    <InspectorShell
      body={
        <div className="inspector-sections">
          <DetailList>
            <Detail label="Deployment ID" value={deployment.modelDeploymentId} />
          </DetailList>
          {terminal ? (
            <Notice role="status">
              This deployment is tearing down and can no longer be edited.
            </Notice>
          ) : null}
          {savedSection === "name" && endpointDirty && !pending ? (
            <Notice role="status">Name saved. Endpoint changes are still unsaved.</Notice>
          ) : null}
          {savedSection === "endpoint" && nameDirty && !pending ? (
            <Notice role="status">Endpoint saved. Name changes are still unsaved.</Notice>
          ) : null}
          <DeploymentNameSection
            draft={nameDraft}
            error={nameError}
            inputRef={nameRef}
            onChange={(value) => {
              setNameDraft(value);
              setNameError(undefined);
            }}
            onSave={() => void saveName()}
            pending={pending}
            terminal={terminal}
          />
          <section aria-labelledby="endpoint-heading">
            <h3 className="text-sm font-semibold" id="endpoint-heading">
              Endpoint
            </h3>
            <p className="mt-3 text-sm text-muted-foreground">
              Replace the endpoint configuration. Leave the API key blank to retain the stored key.
            </p>
            <form
              aria-labelledby="endpoint-heading"
              className="mt-4"
              noValidate
              onSubmit={(event) => {
                event.preventDefault();
                void saveEndpoint();
              }}
            >
              <UpstreamForm
                editing
                error={error ?? endpointError}
                baseUrlRef={baseUrlRef}
                fieldErrors={endpointFieldErrors}
                onChange={(next) => {
                  setEndpointDraft(next);
                  setEndpointError(undefined);
                  setEndpointFieldErrors(new Map());
                }}
                upstreamModelIdRef={upstreamModelIdRef}
                values={endpointDraft}
              />
              <Button className="mt-4" disabled={pending || terminal} size="console" type="submit">
                {pending ? "Saving endpoint…" : "Save endpoint"}
              </Button>
            </form>
          </section>
        </div>
      }
      focusKey={`edit:${deployment.modelDeploymentId}`}
      id="deployment-edit-panel"
      navigationDisabledReason={
        pending ? "Saving changes. Inspector navigation is unavailable." : undefined
      }
      onBack={onBack}
      onOpenChange={onOpenChange}
      open={open}
      openerRef={openerRef}
      title={
        <ResourceTitle
          colorKey={deployment.modelDeploymentId}
          name={deployment.modelDeploymentName || UPSTREAM_MODEL_NAME}
          screen="Edit"
        />
      }
    />
  );
}

function endpointValuesEqual(left: UpstreamFormValues, right: UpstreamFormValues): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function UpstreamForm({
  values,
  onChange,
  error,
  editing = false,
  fieldErrors = new Map(),
  onFieldBlur,
  baseUrlRef,
  upstreamModelIdRef,
}: {
  values: UpstreamFormValues;
  onChange: (values: UpstreamFormValues) => void;
  error?: string;
  editing?: boolean;
  fieldErrors?: FieldErrors;
  onFieldBlur?: (field: "baseUrl" | "upstreamModelId") => void;
  baseUrlRef?: RefObject<HTMLInputElement | null>;
  upstreamModelIdRef?: RefObject<HTMLInputElement | null>;
}) {
  const set = <K extends keyof UpstreamFormValues>(key: K, value: UpstreamFormValues[K]) =>
    onChange({ ...values, [key]: value });
  return (
    <div className="grid gap-4">
      {error ? (
        <Notice role="alert" tone="destructive">
          {error}
        </Notice>
      ) : null}
      <label className="grid gap-1">
        Base URL
        <Input
          ref={baseUrlRef}
          required
          aria-describedby={fieldErrors.get("baseUrl") ? "upstream-base-url-error" : undefined}
          aria-invalid={fieldErrors.get("baseUrl") ? "true" : undefined}
          type="url"
          value={values.baseUrl}
          onBlur={() => onFieldBlur?.("baseUrl")}
          onChange={(e) => set("baseUrl", e.target.value)}
        />
        {fieldErrors.get("baseUrl") ? (
          <FieldError id="upstream-base-url-error">{fieldErrors.get("baseUrl")}</FieldError>
        ) : null}
      </label>
      <label className="grid gap-1">
        Upstream model ID
        <Input
          ref={upstreamModelIdRef}
          required
          aria-label="Upstream model ID"
          value={values.upstreamModelId}
          aria-describedby={
            fieldErrors.get("upstreamModelId") ? "upstream-model-id-error" : undefined
          }
          aria-invalid={fieldErrors.get("upstreamModelId") ? "true" : undefined}
          onBlur={() => onFieldBlur?.("upstreamModelId")}
          onChange={(e) => set("upstreamModelId", e.target.value)}
        />
        {fieldErrors.get("upstreamModelId") ? (
          <FieldError id="upstream-model-id-error">{fieldErrors.get("upstreamModelId")}</FieldError>
        ) : null}
      </label>
      <label className="grid gap-1">
        API key
        <Input
          type="password"
          autoComplete="new-password"
          value={values.apiKey}
          onChange={(e) => set("apiKey", e.target.value)}
          placeholder={editing ? "Optional; blank retains stored key" : "Optional"}
        />
      </label>
    </div>
  );
}

export function UpstreamInspector({
  chatTarget,
  deployment,
  open,
  onBack,
  onOpenChange,
  openerRef,
  onEdit,
  onUnload,
  error,
  teardownError,
  containingRoutes,
  initialRange,
  metricsClient,
  pending = false,
}: {
  /** Where the Chat action takes the operator; computed from the routes listing the deployment. */
  chatTarget?: ChatTarget;
  deployment: ModelDeployment;
  open: boolean;
  onBack?: () => void;
  onOpenChange: (open: boolean) => void;
  openerRef: RefObject<HTMLElement | null>;
  onEdit: () => void;
  onUnload: () => Promise<boolean>;
  error?: string;
  teardownError?: string;
  containingRoutes?: readonly RouteMention[];
  /** The range label the request cards start at, such as the range of the page it opened from. */
  initialRange?: string;
  metricsClient: MetricsClient;
  pending?: boolean;
}) {
  const requestSpecs = useMemo(
    () => deploymentRequestCards(deployment.modelDeploymentId),
    [deployment.modelDeploymentId],
  );
  const [confirmingTeardown, setConfirmingTeardown] = useState(false);
  const cancelTeardownRef = useRef<HTMLButtonElement>(null);
  const teardownButtonRef = useRef<HTMLElement>(null);
  const config = deployment.upstreamDesiredConfiguration;
  const presentationName = deployment.modelDeploymentName || UPSTREAM_MODEL_NAME;
  const chat = useChatAction(chatTarget);

  return (
    <>
      <InspectorShell
        body={
          <div className="inspector-sections">
            {error && !(confirmingTeardown && teardownError) ? (
              <Notice role="alert" tone="destructive">
                {error}
              </Notice>
            ) : null}
            <section aria-labelledby="upstream-deployment-heading">
              <InspectorSectionHeader headingId="upstream-deployment-heading" title="Deployment" />
              <DetailList>
                <Detail label="Deployment ID" value={deployment.modelDeploymentId} />
                <Detail label="Routes" value={<RouteMentions routes={containingRoutes} />} />
              </DetailList>
            </section>
            <section aria-labelledby="upstream-source-heading">
              <InspectorSectionHeader
                action={<Badge variant="info">External</Badge>}
                headingId="upstream-source-heading"
                title="Source"
              />
              <DetailList>
                <Detail label="Source" value={UPSTREAM_MODEL_NAME} />
                <Detail label="Upstream model ID" value={config?.upstreamModelId ?? "Unknown"} />
                <Detail label="Base URL" value={config?.endpoint?.baseUrl ?? "Unknown"} />
              </DetailList>
            </section>
            <RequestCards
              client={metricsClient}
              headingId="upstream-requests-heading"
              initialRange={initialRange}
              key={deployment.modelDeploymentId}
              specs={requestSpecs}
            />
          </div>
        }
        footer={
          <InspectorActions
            more={[
              { disabled: pending, icon: Pencil, label: "Edit", onSelect: onEdit },
              {
                destructive: true,
                focusRef: teardownButtonRef,
                icon: Trash2,
                label: "Teardown",
                onSelect: () => setConfirmingTeardown(true),
                pending,
                pendingLabel: "Tearing down…",
              },
            ]}
            primary={chat && { ...chat, disabled: chat.disabled || pending }}
          />
        }
        focusKey={`deployment:${deployment.modelDeploymentId}`}
        id={modelInspectorPanelId}
        navigationDisabledReason={
          pending
            ? "An inspector operation is pending. Inspector navigation is unavailable."
            : undefined
        }
        title={
          <ResourceTitle
            colorKey={deployment.modelDeploymentId}
            name={deployment.modelDeploymentName || UPSTREAM_MODEL_NAME}
          />
        }
        open={open}
        onBack={onBack}
        onOpenChange={onOpenChange}
        openerRef={openerRef}
        suspendDismissal={confirmingTeardown}
      />
      <Dialog
        open={confirmingTeardown}
        modal
        disablePointerDismissal
        onOpenChange={(nextOpen) => {
          if (pending) return;
          setConfirmingTeardown(nextOpen);
        }}
      >
        <DialogContent
          aria-describedby="upstream-teardown-description"
          finalFocus={teardownButtonRef}
          initialFocus={cancelTeardownRef}
          showCloseButton={false}
        >
          <DialogHeader>
            <DialogTitle>Teardown {presentationName}?</DialogTitle>
            {teardownError ? (
              <Notice role="alert" tone="destructive">
                {teardownError}
              </Notice>
            ) : null}
            <DialogDescription id="upstream-teardown-description">
              This removes upstream model deployment{" "}
              <ResourceMarker colorKey={deployment.modelDeploymentId} />
              {deployment.modelDeploymentId}.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              disabled={pending}
              ref={cancelTeardownRef}
              type="button"
              variant="outline"
              onClick={() => setConfirmingTeardown(false)}
            >
              Cancel
            </Button>
            <Button
              disabled={pending}
              type="button"
              variant="destructive"
              onClick={() => {
                void onUnload().then((accepted) => {
                  if (accepted) setConfirmingTeardown(false);
                });
              }}
            >
              {pending ? "Tearing down…" : teardownError ? "Try again" : "Teardown"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
