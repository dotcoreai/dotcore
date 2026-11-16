import { type RefObject, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useLocation } from "react-router-dom";

import { Button } from "@/components/ui/button";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { Notice } from "@/components/ui/notice";
import { ResourceTitle } from "@/components/ui/resource-marker";
import type { DeploymentMetadata, DeploymentMetadataUpdate } from "@/domain/deployment-metadata";
import { validateDeploymentName } from "@/domain/deployment-name";
import { UPSTREAM_MODEL_NAME } from "@/domain/upstream-presentation";
import { DeploymentNameSection } from "@/features/deployments/deployment-editor";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
} from "@/gen/dotcore/model/v1/deployment_pb";
import type { FieldErrors } from "@/lib/form-validation";
import { usePanelDirtyGuard, usePanelNavigation } from "@/panels/panel-navigation";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";
import { popPanelLocation } from "@/panels/panel-stack";

import { UpstreamEndpointFields } from "./upstream-endpoint-fields";
import {
  defaultUpstreamForm,
  type UpstreamFormValues,
  validateModelId,
  validateUpstreamBaseUrl,
} from "./upstream-form";

/**
 * Edits an upstream model deployment: its display name and its endpoint, each saved on its own.
 * Drafts survive projection updates, and the panel returns to its parent once nothing is unsaved.
 */
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
              <UpstreamEndpointFields
                editing
                storedApiKey={deployment.upstreamDesiredConfiguration?.apiKey ?? ""}
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
