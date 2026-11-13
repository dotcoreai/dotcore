import { ArrowRightIcon, ExternalLink, Pause, Pencil, Play, Trash2 } from "lucide-react";
import type { ReactNode, RefObject } from "react";
import { useEffect, useRef, useState } from "react";

import type { DeploymentLogOperations } from "@/api/deployment-log-client";
import {
  type InspectorAction,
  InspectorActions,
} from "@/components/inspector-actions/inspector-actions";
import { ParametersDetails } from "@/components/parameters/parameters";
import { Button } from "@/components/ui/button";
import { CopyableValue } from "@/components/ui/copyable-value";
import { Detail, DetailList } from "@/components/ui/detail-list";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { Notice } from "@/components/ui/notice";
import { ResourceMarker, ResourceTitle } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import { StepProgress } from "@/components/ui/step-progress";
import { applicationDeploymentPresentationName } from "@/domain/application-deployment-name";
import {
  applicationDeploymentDesiredStateLabel,
  applicationDeploymentDesiredStateTone,
  applicationDeploymentEndpoint,
  applicationDeploymentLifecycleState,
  inferenceRouteBindings,
  isApplicationDesiredStateVisible,
  isPausableApplication,
  isRemovableApplication,
  isResumableApplication,
  isTransitioningApplicationState,
  projectApplicationDeploymentStatus,
} from "@/domain/application-projection";
import { executionTargetKey, nodeConnectionStatus } from "@/domain/execution-target";
import { placementLabel, placementMachineName } from "@/domain/placement";
import { PanelMention } from "@/features/deployments/panel-mention";
import {
  type ApplicationDeployment,
  ApplicationDeploymentDesiredState,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  ApplicationDeploymentStep,
  ApplicationLifecycleState,
} from "@/gen/dotcore/application/v1/state_pb";
import {
  type ConfiguredExecutionTarget,
  type Machine,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { formatCount, formatRelativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";

import {
  applicationAcceleratorMemory,
  formatUnitAcceleratorMemory,
} from "./application-accelerator-memory";
import { formatGpuDevices } from "./application-gpu-devices";
import { applicationInspectorPanelId } from "./application-inspector-panel";
import {
  applicationParameterFields,
  parameterDefaults,
  retainedParameterValues,
} from "./application-parameters";
import {
  storageBindingLabel,
  storagePlacementLabel,
  teardownConsequence,
} from "./application-storage";

interface InspectorProps {
  onOpenChange: (open: boolean) => void;
  open: boolean;
  openerRef: RefObject<HTMLElement | null>;
}

export interface DeploymentApplicationInspectorProps extends InspectorProps {
  /** Link to the catalog application's inspector, when the catalog lists it. */
  applicationHref?: string;
  deployment: ApplicationDeployment;
  error?: string;
  /** The site's configured execution targets, undefined until read; they describe the placement. */
  executionTargets?: readonly ConfiguredExecutionTarget[];
  floatingLayer?: ReactNode;
  /** Link to the linked inference route's inspector. */
  inferenceRouteHref?: string;
  logOpenerRef?: RefObject<HTMLButtonElement | null>;
  logs?: DeploymentLogOperations;
  onBack?: () => void;
  onOpenLogs?: () => void;
  onPause?: () => void;
  onEdit?: () => void;
  onRemove: () => void;
  onResume?: () => void;
  pendingAction?: "pause" | "resume" | "remove";
  pending: boolean;
  /** The machine the deployment is placed on, when the site still lists it. */
  placementMachine?: Machine;
  /** Link to the placement machine's inspector, when the site still lists that machine. */
  placementMachineHref?: string;
  suspendDismissal?: boolean;
}

export function DeploymentApplicationInspector({
  applicationHref,
  deployment,
  error,
  executionTargets,
  floatingLayer,
  inferenceRouteHref,
  logOpenerRef,
  logs,
  onBack,
  onOpenLogs,
  onPause,
  onEdit,
  onRemove,
  onOpenChange,
  open,
  openerRef,
  onResume,
  pendingAction,
  pending,
  placementMachine,
  placementMachineHref,
  suspendDismissal = false,
}: DeploymentApplicationInspectorProps) {
  const [confirmingRemoval, setConfirmingRemoval] = useState(false);
  const cancelRemovalRef = useRef<HTMLButtonElement>(null);
  const removeButtonRef = useRef<HTMLElement>(null);
  const lifecycleState = applicationDeploymentLifecycleState(deployment);
  const application = deployment.snapshot;
  const endpointUrl = applicationDeploymentEndpoint(deployment);
  const failure = deployment.state?.failure;
  const canPause = isPausableApplication(deployment);
  const canResume = isResumableApplication(deployment);
  const canRemove = isRemovableApplication(deployment);
  const tearingDown = deployment.desiredState === ApplicationDeploymentDesiredState.TEARDOWN;
  const actionDisabled = pending || isTransitioningApplicationState(lifecycleState);
  const pauseDisabled = pending || isPauseConflictingLifecycleState(lifecycleState);
  const observationStale = Boolean(deployment.state?.observationStale);
  const steps = deployment.state?.steps;
  const editAction: InspectorAction = {
    disabled: pending || tearingDown,
    icon: Pencil,
    label: "Edit",
    onSelect: () => onEdit?.(),
  };
  // Resume while paused, otherwise Open when the application has an endpoint; without either, Edit
  // stays visible instead of joining the menu.
  const primaryAction: InspectorAction | undefined = canResume
    ? {
        disabled: actionDisabled,
        icon: Play,
        label: "Resume",
        onSelect: () => onResume?.(),
        pending: pendingAction === "resume",
        pendingLabel: "Resuming…",
      }
    : endpointUrl
      ? { href: endpointUrl, icon: ExternalLink, label: "Open" }
      : undefined;
  const placement = deployment.placement;
  const placementTarget = placement
    ? executionTargets?.find(
        (target) => executionTargetKey(target) === executionTargetKey(placement),
      )
    : undefined;
  const remoteNodeId = placementNodeId(deployment, placementTarget);
  const remoteMachineName = remoteNodeId
    ? placementMachineName(remoteNodeId, placementMachine)
    : undefined;
  const machineLastReport = placementMachine?.observationFreshness?.receivedAt;
  const lastReport = machineLastReport ? formatRelativeTime(machineLastReport) : undefined;
  const pinnedImages = deployment.nodeResolvedLaunchIdentities;
  const placementName = placementLabel(deployment.placement, placementMachine, executionTargets);
  // The deployment's per-unit accelerator memory from its service observations; units the
  // projection does not cover render nothing.
  const acceleratorMemory = applicationAcceleratorMemory(deployment);
  const parameterFields = applicationParameterFields(deployment.snapshot?.graph?.parameters);
  const retainedParameters = parameterFields.map((field) => field.view);

  useEffect(() => {
    if (!canRemove && confirmingRemoval) {
      queueMicrotask(() => setConfirmingRemoval(false));
    }
  }, [canRemove, confirmingRemoval]);

  return (
    <>
      <InspectorShell
        onBack={onBack}
        body={
          <div className="inspector-sections">
            {error ? (
              <Notice role="alert" tone="destructive">
                {error}
              </Notice>
            ) : null}
            <section aria-labelledby="application-deployment-status-heading">
              <InspectorSectionHeader
                action={
                  logs ? (
                    <Button
                      onClick={() => onOpenLogs?.()}
                      ref={logOpenerRef}
                      size="sm"
                      type="button"
                      variant="link"
                    >
                      Logs
                      <ArrowRightIcon aria-hidden="true" className="size-3.5" />
                    </Button>
                  ) : undefined
                }
                headingId="application-deployment-status-heading"
                title="Status"
              />
              <DetailList>
                <Detail
                  label="State"
                  value={<StatusBadge status={projectApplicationDeploymentStatus(deployment)} />}
                />
                {isApplicationDesiredStateVisible(deployment) ? (
                  <Detail
                    label="Desired"
                    value={
                      <StatusBadge
                        status={{
                          label: applicationDeploymentDesiredStateLabel(deployment.desiredState),
                          tone: applicationDeploymentDesiredStateTone(deployment.desiredState),
                        }}
                      />
                    }
                  />
                ) : null}
                {remoteNodeId ? (
                  <Detail
                    label="Machine connection"
                    value={
                      <StatusBadge
                        status={nodeConnectionStatus(
                          placementTarget?.availability?.connectionState,
                        )}
                      />
                    }
                  />
                ) : null}
                {failure?.code ? <Detail label="Failure code" value={failure.code} /> : null}
              </DetailList>
              {steps?.steps.length && !observationStale ? (
                <StepProgress
                  className="mt-3"
                  current={steps.current}
                  steps={steps.steps.map(applicationStepLabel)}
                />
              ) : null}
              {observationStale ? (
                <Notice className="mt-3" role="status" tone="warning">
                  Machine stale. Showing the last state {remoteMachineName ?? "the machine"}{" "}
                  reported
                  {lastReport ? (
                    <>
                      ; last report <span title={lastReport.title}>{lastReport.text}</span>
                    </>
                  ) : null}
                  . The state updates when the machine reports again.
                </Notice>
              ) : null}
              {failure?.message ? (
                <div className="mt-3">
                  {/* Notice renders a `p`, so the diagnostics stay inline content rather than block
                   * children. */}
                  <Notice role="alert" tone="destructive">
                    {failure.message}
                    {failure.diagnostics.map((diagnostic) => (
                      <span className="mt-1.5 block break-words" key={diagnostic}>
                        {diagnostic}
                      </span>
                    ))}
                  </Notice>
                </div>
              ) : null}
            </section>
            <section aria-labelledby="application-deployment-identity-heading">
              <InspectorSectionHeader
                headingId="application-deployment-identity-heading"
                title="Deployment"
              />
              <DetailList>
                <Detail label="Deployment ID" value={deployment.applicationDeploymentId} />
                <Detail label="Name" value={deployment.applicationDeploymentName} />
                <Detail
                  label="Placement"
                  value={
                    deployment.placement ? (
                      <PanelMention
                        colorKey={deployment.placement.siteNodeId}
                        href={placementMachineHref}
                      >
                        {placementName}
                      </PanelMention>
                    ) : (
                      placementName
                    )
                  }
                />
                {deployment.snapshot ? (
                  <Detail
                    label="GPU devices"
                    value={formatGpuDevices(deployment.snapshot.acceleratorDeviceIds)}
                  />
                ) : null}
                {deployment.selectedInferenceRoute ? (
                  <Detail
                    label="Inference route"
                    value={
                      <PanelMention
                        colorKey={deployment.selectedInferenceRoute.inferenceRouteId}
                        href={inferenceRouteHref}
                      >
                        {deployment.selectedInferenceRoute.routeName}
                      </PanelMention>
                    }
                  />
                ) : application && inferenceRouteBindings(application).length ? (
                  <Detail label="Inference route" value="Not linked" />
                ) : null}
              </DetailList>
            </section>

            {retainedParameters.length || deployment.storageAttachments.length ? (
              <section aria-labelledby="application-deployment-parameters-heading">
                <InspectorSectionHeader
                  headingId="application-deployment-parameters-heading"
                  title="Parameters"
                />
                <DetailList>
                  <ParametersDetails
                    defaults={parameterDefaults(parameterFields)}
                    generatedIds={new Set(deployment.snapshot?.generatedParameterIds)}
                    parameters={retainedParameters}
                    values={retainedParameterValues(deployment.snapshot)}
                  />
                  {deployment.storageAttachments.map((attachment) => (
                    <Detail
                      key={`${attachment.serviceName}/${attachment.bindingName}`}
                      label={storageBindingLabel(attachment.bindingName)}
                      value={storagePlacementLabel(attachment.placement)}
                    />
                  ))}
                </DetailList>
              </section>
            ) : null}
            {acceleratorMemory.length ? (
              <section aria-labelledby="application-accelerator-memory-heading">
                <InspectorSectionHeader
                  headingId="application-accelerator-memory-heading"
                  title="Accelerator memory"
                />
                <DetailList>
                  {acceleratorMemory.map((unit) => {
                    const formatted = formatUnitAcceleratorMemory(unit);
                    return (
                      <Detail
                        key={unit.serviceName}
                        label={unit.serviceName}
                        title={formatted.title}
                        value={
                          <span
                            className={cn(
                              formatted.muted && "font-normal text-muted-foreground",
                              formatted.stale && "opacity-60",
                            )}
                            data-stale={formatted.stale}
                          >
                            {formatted.lines.map((line) => (
                              <span className="block" key={line}>
                                {line}
                              </span>
                            ))}
                          </span>
                        }
                      />
                    );
                  })}
                </DetailList>
              </section>
            ) : null}
            {application ? (
              <section aria-labelledby="application-snapshot-heading">
                <InspectorSectionHeader
                  headingId="application-snapshot-heading"
                  meta={formatCount(application.graph?.services.length ?? 0, "service")}
                  title="Application"
                />
                <DetailList>
                  <Detail
                    label="Application"
                    value={
                      <PanelMention colorKey={application.applicationId} href={applicationHref}>
                        {application.displayName}
                      </PanelMention>
                    }
                  />
                  <Detail label="Application ID" value={application.applicationId} />
                  <Detail label="Version" value={application.version} />
                </DetailList>
              </section>
            ) : null}
            {pinnedImages.length ? (
              <section aria-labelledby="application-pinned-images-heading">
                <InspectorSectionHeader
                  headingId="application-pinned-images-heading"
                  meta={
                    deployment.placement
                      ? placementMachineName(deployment.placement.siteNodeId, placementMachine)
                      : undefined
                  }
                  title="Pinned images"
                />
                <DetailList>
                  {pinnedImages.map((pin) => (
                    <Detail
                      key={pin.selectionKey}
                      label={pin.selectionKey}
                      value={<CopyableValue head={19} tail={4} value={pin.launchIdentity} />}
                    />
                  ))}
                </DetailList>
              </section>
            ) : null}
            <section aria-labelledby="application-endpoint-heading">
              <InspectorSectionHeader headingId="application-endpoint-heading" title="Access" />
              <DetailList>
                <Detail label="Endpoint" value={endpointUrl || "Not assigned"} />
              </DetailList>
            </section>
            {application?.notes ? (
              <section aria-labelledby="application-notes-heading">
                <InspectorSectionHeader headingId="application-notes-heading" title="Notes" />
                <p className="mt-2 whitespace-pre-wrap text-[12.5px] text-muted-foreground [overflow-wrap:anywhere]">
                  {application.notes}
                </p>
              </section>
            ) : null}
          </div>
        }
        floatingLayer={floatingLayer}
        footer={
          <InspectorActions
            more={[
              ...(primaryAction ? [editAction] : []),
              ...(canPause
                ? [
                    {
                      disabled: pauseDisabled,
                      icon: Pause,
                      label: "Pause",
                      onSelect: () => onPause?.(),
                      pending: pendingAction === "pause",
                      pendingLabel: "Pausing…",
                    },
                  ]
                : []),
              ...(canRemove
                ? [
                    {
                      destructive: true,
                      disabled: actionDisabled,
                      focusRef: removeButtonRef,
                      icon: Trash2,
                      label: "Teardown",
                      onSelect: () => setConfirmingRemoval(true),
                      pending: pendingAction === "remove" || (pending && !pendingAction),
                      pendingLabel: "Tearing down…",
                    },
                  ]
                : []),
            ]}
            primary={primaryAction}
            secondary={primaryAction ? [] : [editAction]}
          />
        }
        focusKey={`deployment:${deployment.applicationDeploymentId}`}
        id={applicationInspectorPanelId}
        navigationDisabledReason={
          pending
            ? "An inspector operation is pending. Inspector navigation is unavailable."
            : undefined
        }
        onOpenChange={onOpenChange}
        open={open}
        openerRef={openerRef}
        suspendDismissal={confirmingRemoval || suspendDismissal}
        title={
          <ResourceTitle
            colorKey={deployment.applicationDeploymentId}
            name={applicationDeploymentPresentationName(deployment)}
          />
        }
      />
      {canRemove ? (
        <Dialog
          open={canRemove && confirmingRemoval}
          modal
          disablePointerDismissal
          onOpenChange={(nextOpen) => {
            setConfirmingRemoval(nextOpen);
          }}
        >
          <DialogContent
            aria-describedby="remove-deployment-description"
            finalFocus={removeButtonRef}
            initialFocus={cancelRemovalRef}
            showCloseButton={false}
          >
            <DialogHeader>
              <DialogTitle>Teardown deployment?</DialogTitle>
              <DialogDescription id="remove-deployment-description">
                Teardown deployment <ResourceMarker colorKey={deployment.applicationDeploymentId} />
                {deployment.applicationDeploymentName || deployment.applicationDeploymentId}?{" "}
                {teardownConsequence(deployment.storageAttachments)} The deployment remains visible
                as Tearing down until cleanup completes.
              </DialogDescription>
            </DialogHeader>
            <DialogFooter>
              <DialogClose
                render={
                  <Button ref={cancelRemovalRef} type="button" variant="outline">
                    Cancel
                  </Button>
                }
              />
              <Button
                disabled={pending}
                onClick={() => {
                  setConfirmingRemoval(false);
                  onRemove();
                }}
                type="button"
                variant="destructive"
              >
                {pending ? "Tearing down…" : "Teardown"}
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      ) : null}
    </>
  );
}

/**
 * The SiteNode of a remote deployment, or `undefined` for one on the site manager's machine.
 * A stale observation names the node even when its target is not reported.
 */
function placementNodeId(
  deployment: ApplicationDeployment,
  placementTarget: ConfiguredExecutionTarget | undefined,
): string | undefined {
  const connectionState = placementTarget?.availability?.connectionState;
  const remote =
    deployment.state?.observationStale ||
    (connectionState !== undefined &&
      connectionState !== NodeObservationState.UNSPECIFIED &&
      !placementTarget?.siteManager);
  return remote ? deployment.placement?.siteNodeId : undefined;
}

function applicationStepLabel(step: ApplicationDeploymentStep): string {
  switch (step) {
    case ApplicationDeploymentStep.PREPARING_IMAGE:
      return "Preparing image";
    case ApplicationDeploymentStep.STARTING:
      return "Starting";
    case ApplicationDeploymentStep.WAITING_FOR_ENDPOINT:
      return "Waiting for endpoint";
    default:
      return "Starting";
  }
}

function isPauseConflictingLifecycleState(state: ApplicationLifecycleState): boolean {
  return (
    state === ApplicationLifecycleState.STOPPING ||
    state === ApplicationLifecycleState.PAUSING ||
    state === ApplicationLifecycleState.PAUSED
  );
}
