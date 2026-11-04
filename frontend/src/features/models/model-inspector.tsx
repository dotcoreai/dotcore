import { ArrowRightIcon, Pause, Pencil, Play, Trash2 } from "lucide-react";
import type { ReactNode, RefObject } from "react";
import { useRef, useState } from "react";

import type { DeploymentLogOperations } from "@/api/deployment-log-client";
import { DeploymentListSection, DeploymentRow } from "@/components/deployment-list/deployment-list";
import { InspectorActions } from "@/components/inspector-actions/inspector-actions";
import { DeployActionIcon } from "@/components/ui/action-icons";
import { Button } from "@/components/ui/button";
import { CodeCopyControl } from "@/components/ui/copy-control";
import { Detail, DetailList } from "@/components/ui/detail-list";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { Notice } from "@/components/ui/notice";
import { Progress, ProgressLabel, ProgressValue } from "@/components/ui/progress";
import { ResourceMarker, ResourceTitle } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import type { ChatTarget } from "@/features/chat/chattable-route";
import { useChatAction } from "@/features/chat/use-chat-action";
import {
  PanelMention,
  type RouteMention,
  RouteMentions,
} from "@/features/deployments/panel-mention";
import { placementLabel } from "@/features/deployments/placement";
import { type EngineParameterValues } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { type Model } from "@/gen/dotcore/catalog/v1/model_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
  type ResolvedLocalModelDeploymentSnapshot,
} from "@/gen/dotcore/model/v1/deployment_pb";
import {
  type ModelDeploymentProgress,
  ModelDeploymentStatus,
} from "@/gen/dotcore/model/v1/state_pb";
import { type ConfiguredExecutionTarget, type Machine } from "@/gen/dotcore/site/v1/service_pb";
import { formatBytes, formatCompactCount, formatRelativeTime } from "@/lib/format";
import { requestKindsLabel } from "@/lib/request-kinds";

import type { ModelRow } from "./catalog-model-row";
import {
  type CatalogProfileCompatibility,
  profileDeployBlockedReason,
} from "./catalog-profile-compatibility";
import { CatalogProfiles, CatalogProfileSections } from "./catalog-profile-sections";
import { deploymentSpeculativeLabel } from "./deployment-speculative";
import { ArgumentTokenList, DesiredArgumentList } from "./desired-arguments";
import { desiredArgv } from "./desired-argv";
import {
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "./engine-parameter-values";
import { EngineParametersDetails } from "./engine-parameters";
import { deploymentKvCachePrecisionLabel } from "./kv-cache-precision";
import { resolvedModelEngineDiagnostics } from "./model-engine-diagnostics";
import { modelInspectorPanelId } from "./model-inspector-panel";
import { ModelIntelligenceSection } from "./model-intelligence-section";
import { ModelLicenseDetail } from "./model-license-detail";
import {
  deploymentStatus,
  isModelDesiredStateVisible,
  isPausableModel,
  isResumableModel,
  isTransitioningModelStatus,
  isUnloadableModel,
  localModelSnapshot,
  modelDeploymentDesiredStateLabel,
  modelDeploymentDesiredStateTone,
  modelDeploymentPresentationName,
  modelStatusMessage,
  projectModelDeploymentStatus,
  weightQuantizationLabel,
} from "./model-projection";
import type { CatalogChips } from "./profile-table-seed";

interface InspectorProps {
  onOpenChange: (open: boolean) => void;
  open: boolean;
  openerRef: RefObject<HTMLElement | null>;
}

export type CatalogModelInspectorScreen = "overview" | "profile-detail";

export type DeploymentModelInspectorScreen = "overview" | "configuration";

interface CatalogModelInspectorProps extends InspectorProps {
  /** The catalog's chips the profile table starts from; the catalog's default chips without them. */
  catalogChips?: CatalogChips;
  /** The model's profile compatibility on the site; absent when the site can't report it. */
  compatibility?: CatalogProfileCompatibility;
  deployOpenerRef?: RefObject<HTMLButtonElement | null>;
  /** The model's deployments, listed on the overview when `onOpenDeployment` can open them. */
  deployments?: readonly ModelDeployment[];
  error?: string;
  /** The site's configured execution targets, undefined until read; they name placements. */
  executionTargets?: readonly ConfiguredExecutionTarget[];
  /** Whether profile memory estimates are still loading. */
  memoryPending: boolean;
  model: Model;
  onBack?: () => void;
  onDeploy: () => void;
  onOpenDeployment?: (modelDeploymentId: string) => void;
  onSelectProfile?: (profileId: string) => void;
  pending: boolean;
  profileId?: string;
  /** The model's table projection, the source of profile names, engines, and memory values. */
  row: ModelRow;
  screen?: CatalogModelInspectorScreen;
  suspendDismissal?: boolean;
}

interface DeploymentModelInspectorProps extends InspectorProps {
  catalogModelHref?: string;
  /** The deployment's catalog model, which names its speculative options; undefined until read. */
  catalogModel?: Model;
  /** Where the Chat action takes the operator; computed from the routes listing the deployment. */
  chatTarget?: ChatTarget;
  containingRoutes?: readonly RouteMention[];
  deployment: ModelDeployment;
  error?: string;
  /** The site's configured execution targets, undefined until read; they name the placement. */
  executionTargets?: readonly ConfiguredExecutionTarget[];
  teardownError?: string;
  floatingLayer?: ReactNode;
  logOpenerRef?: RefObject<HTMLButtonElement | null>;
  /** The deployment's memory row, shown in its Deployment section, supplied by the page that reads the site's memory. */
  memory?: ReactNode;
  /** The deployment's Requests section, supplied by the page that reads metrics. */
  requests?: ReactNode;
  logs?: DeploymentLogOperations;
  onBack?: () => void;
  onOpenConfiguration?: () => void;
  onOpenLogs?: () => void;
  onPause?: () => void;
  onResume?: () => void;
  onEdit?: () => void;
  onUnload: () => Promise<boolean>;
  pendingAction?: "pause" | "resume" | "unload";
  pending: boolean;
  /** The machine the deployment is placed on, when the site lists it. */
  placementMachine?: Machine;
  /** Link to the placement machine's inspector, when the site lists that machine. */
  placementMachineHref?: string;
  screen?: DeploymentModelInspectorScreen;
  suspendDismissal?: boolean;
}

function CatalogModelInspector({
  catalogChips,
  compatibility,
  deployOpenerRef,
  deployments = [],
  error,
  executionTargets,
  memoryPending,
  model,
  onBack,
  onDeploy,
  onOpenChange,
  onOpenDeployment,
  onSelectProfile,
  open,
  openerRef,
  pending,
  profileId,
  row,
  screen = "overview",
  suspendDismissal,
}: CatalogModelInspectorProps) {
  const profile = model.profiles.find((candidate) => candidate.profileId === profileId);
  const profileRow = row.profiles.find((candidate) => candidate.profileId === profileId);
  const deployBlocked =
    screen === "profile-detail" && profileId
      ? profileDeployBlockedReason(compatibility?.compatibility, model.modelId, profileId)
      : undefined;

  return (
    <InspectorShell
      body={
        <div className="inspector-sections">
          {error ? (
            <Notice role="alert" tone="destructive">
              {error}
            </Notice>
          ) : null}
          {screen === "overview" ? (
            <CatalogModelOverview
              catalogChips={catalogChips}
              compatibility={compatibility}
              deployments={deployments}
              executionTargets={executionTargets}
              memoryPending={memoryPending}
              model={model}
              onOpenDeployment={onOpenDeployment}
              onSelectProfile={onSelectProfile}
              row={row}
            />
          ) : null}
          {screen === "profile-detail" && profile && profileRow ? (
            <CatalogProfileSections
              compatibility={compatibility}
              memoryPending={memoryPending}
              model={model}
              profile={profile}
              row={profileRow}
            />
          ) : null}
        </div>
      }
      focusKey={`${screen}:${profileId ?? model.modelId}`}
      footer={
        <InspectorActions
          primary={{
            disabled: pending || Boolean(deployBlocked),
            disabledReason: deployBlocked,
            focusRef: deployOpenerRef,
            icon: DeployActionIcon,
            label: "Deploy",
            onSelect: onDeploy,
            pending,
            pendingLabel: "Deploying…",
          }}
        />
      }
      id={modelInspectorPanelId}
      onBack={onBack}
      onOpenChange={onOpenChange}
      open={open}
      openerRef={openerRef}
      suspendDismissal={suspendDismissal}
      title={
        <ResourceTitle
          colorKey={model.modelId}
          name={model.displayName}
          screen={screen === "profile-detail" ? profileId : undefined}
        />
      }
    />
  );
}

function CatalogModelOverview({
  catalogChips,
  compatibility,
  deployments,
  executionTargets,
  memoryPending,
  model,
  onOpenDeployment,
  onSelectProfile,
  row,
}: {
  catalogChips?: CatalogChips;
  compatibility?: CatalogProfileCompatibility;
  deployments: readonly ModelDeployment[];
  executionTargets?: readonly ConfiguredExecutionTarget[];
  memoryPending: boolean;
  model: Model;
  onOpenDeployment?: (modelDeploymentId: string) => void;
  onSelectProfile?: (profileId: string) => void;
  row: ModelRow;
}) {
  return (
    <>
      <section aria-labelledby="catalog-model-details-heading">
        <InspectorSectionHeader headingId="catalog-model-details-heading" title="Catalog model" />
        <DetailList>
          <Detail label="Version" value={model.version} />
          <Detail label="Model ID" value={model.modelId} />
          <Detail label="Request kinds" value={requestKindsLabel(row.requestKinds) || "None"} />
          <ModelLicenseDetail model={model} />
        </DetailList>
      </section>
      {onOpenDeployment ? (
        <DeploymentListSection
          count={deployments.length}
          headingId="catalog-model-deployments-heading"
          listLabel={`Deployments of ${model.displayName}`}
          title="Deployments"
        >
          {deployments.map((deployment) => (
            <DeploymentRow
              description={[
                localModelSnapshot(deployment.snapshot)?.profileId,
                placementLabel(deployment.intent?.placement, undefined, executionTargets),
              ]}
              key={deployment.modelDeploymentId}
              name={modelDeploymentPresentationName(deployment)}
              onSelect={() => onOpenDeployment(deployment.modelDeploymentId)}
              status={projectModelDeploymentStatus(deployment)}
            />
          ))}
        </DeploymentListSection>
      ) : null}
      <ModelIntelligenceSection model={model} />
      <CatalogProfiles
        catalogChips={catalogChips}
        compatibility={compatibility}
        memoryPending={memoryPending}
        onSelectProfile={onSelectProfile}
        row={row}
      />
    </>
  );
}

function DeploymentModelInspector({
  catalogModel,
  chatTarget,
  catalogModelHref,
  containingRoutes,
  deployment,
  error,
  executionTargets,
  teardownError,
  floatingLayer,
  logOpenerRef,
  logs,
  onBack,
  onOpenConfiguration,
  onOpenLogs,
  onPause,
  onResume,
  onEdit,
  onOpenChange,
  onUnload,
  open,
  openerRef,
  pendingAction,
  memory,
  requests,
  pending,
  placementMachine,
  placementMachineHref,
  screen = "overview",
  suspendDismissal = false,
}: DeploymentModelInspectorProps) {
  const [confirmingTeardown, setConfirmingTeardown] = useState(false);
  const cancelTeardownRef = useRef<HTMLButtonElement>(null);
  const teardownButtonRef = useRef<HTMLElement>(null);
  const model = localModelSnapshot(deployment.snapshot);
  const status = deploymentStatus(deployment);
  const actionDisabled = pending || isTransitioningModelStatus(status);
  const pauseDisabled = pending || isPauseConflictingStatus(status);
  const chat = useChatAction(chatTarget);
  const presentationName = modelDeploymentPresentationName(deployment);
  const screenTitle = deploymentScreenTitle(screen);

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
            {screen === "overview" ? (
              <DeploymentOverview
                catalogModel={catalogModel}
                catalogModelHref={catalogModelHref}
                containingRoutes={containingRoutes}
                deployment={deployment}
                executionTargets={executionTargets}
                logOpenerRef={logOpenerRef}
                onOpenConfiguration={onOpenConfiguration}
                memory={memory}
                requests={requests}
                onOpenLogs={onOpenLogs}
                placementMachine={placementMachine}
                placementMachineHref={placementMachineHref}
                showLogs={Boolean(logs)}
              />
            ) : null}
            {screen === "configuration" && model ? (
              <ResolvedDeploymentDetails snapshot={model} />
            ) : null}
          </div>
        }
        floatingLayer={floatingLayer}
        footer={
          <InspectorActions
            more={[
              {
                disabled:
                  deployment.desiredState === ModelDeploymentDesiredState.TEARDOWN || pending,
                icon: Pencil,
                label: "Edit",
                onSelect: () => onEdit?.(),
              },
              ...(isPausableModel(deployment)
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
              {
                destructive: true,
                disabled: !isUnloadableModel(deployment) || actionDisabled,
                focusRef: teardownButtonRef,
                icon: Trash2,
                label: "Teardown",
                onSelect: () => setConfirmingTeardown(true),
                pending: pendingAction === "unload" || (pending && !pendingAction),
                pendingLabel: "Tearing down…",
              },
            ]}
            primary={
              isResumableModel(deployment)
                ? {
                    disabled: actionDisabled,
                    icon: Play,
                    label: "Resume",
                    onSelect: () => onResume?.(),
                    pending: pendingAction === "resume",
                    pendingLabel: "Resuming…",
                  }
                : chat && { ...chat, disabled: chat.disabled || pending }
            }
          />
        }
        focusKey={`${screen}:${deployment.modelDeploymentId}`}
        id={modelInspectorPanelId}
        navigationDisabledReason={
          pending
            ? "An inspector operation is pending. Inspector navigation is unavailable."
            : undefined
        }
        onBack={onBack}
        onOpenChange={onOpenChange}
        open={open}
        openerRef={openerRef}
        title={
          <ResourceTitle
            colorKey={deployment.modelDeploymentId}
            name={presentationName}
            screen={screenTitle}
          />
        }
        suspendDismissal={confirmingTeardown || suspendDismissal}
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
          aria-describedby="teardown-deployment-description"
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
            <DialogDescription id="teardown-deployment-description">
              This removes model deployment{" "}
              <ResourceMarker colorKey={deployment.modelDeploymentId} />
              {deployment.modelDeploymentId} and its running workload resources.
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
              onClick={() => {
                void onUnload().then((accepted) => {
                  if (accepted) setConfirmingTeardown(false);
                });
              }}
              type="button"
              variant="destructive"
            >
              {pending ? "Tearing down…" : teardownError ? "Try again" : "Teardown"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}

function deploymentScreenTitle(screen: DeploymentModelInspectorScreen): string | undefined {
  if (screen === "configuration") return "Configuration";
  return undefined;
}

/**
 * The model deployment inspector root. It answers whether the deployment is healthy, how it is
 * configured, what it is and what it uses, then how it serves requests, as a stack of titled
 * sections, keeping the full resolved configuration one click away.
 */
function DeploymentOverview({
  catalogModel,
  catalogModelHref,
  containingRoutes,
  deployment,
  executionTargets,
  logOpenerRef,
  memory,
  requests,
  onOpenConfiguration,
  onOpenLogs,
  placementMachine,
  placementMachineHref,
  showLogs,
}: {
  catalogModel?: Model;
  catalogModelHref?: string;
  containingRoutes?: readonly RouteMention[];
  deployment: ModelDeployment;
  executionTargets?: readonly ConfiguredExecutionTarget[];
  logOpenerRef?: RefObject<HTMLButtonElement | null>;
  memory?: ReactNode;
  requests?: ReactNode;
  onOpenConfiguration?: () => void;
  onOpenLogs?: () => void;
  placementMachine?: Machine;
  placementMachineHref?: string;
  showLogs: boolean;
}) {
  const model = localModelSnapshot(deployment.snapshot);
  const status = deploymentStatus(deployment);
  const state = deployment.state;
  const statusMessage = modelStatusMessage(deployment);
  const transitioning = isTransitioningModelStatus(status);
  const startedAt = state?.startedAt ? formatRelativeTime(state.startedAt) : undefined;
  const updatedAt = state?.updatedAt ? formatRelativeTime(state.updatedAt) : undefined;
  const failure = state?.failure;
  const quantization = model?.modelArtifact?.weightQuantization;
  const totalContext = resolvedEngineParameterValue(
    model?.engineParameterValues,
    TOTAL_CONTEXT_PARAMETER_ID,
  );
  const contextPerRequest = resolvedEngineParameterValue(
    model?.engineParameterValues,
    CONTEXT_PER_REQUEST_PARAMETER_ID,
  );
  const contextPerRequestDerived = Boolean(
    model?.sizing?.derivedEngineParameterIds.includes(CONTEXT_PER_REQUEST_PARAMETER_ID),
  );
  const speculative = model ? deploymentSpeculativeLabel(model, catalogModel) : undefined;
  const kvCachePrecision = model ? deploymentKvCachePrecisionLabel(model, catalogModel) : undefined;
  const devices = state?.observedPlacements.flatMap((placement) => placement.acceleratorDeviceIds);
  const placement = deployment.intent?.placement;
  const placementName = placementLabel(placement, placementMachine, executionTargets);

  return (
    <>
      <section aria-labelledby="deployment-status-heading">
        <InspectorSectionHeader
          action={
            showLogs ? (
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
          headingId="deployment-status-heading"
          title="Status"
        />
        <DetailList>
          <Detail
            label="State"
            value={<StatusBadge status={projectModelDeploymentStatus(deployment)} />}
          />
          {isModelDesiredStateVisible(deployment) ? (
            <Detail
              label="Desired"
              value={
                <StatusBadge
                  status={{
                    label: modelDeploymentDesiredStateLabel(deployment.desiredState),
                    tone: modelDeploymentDesiredStateTone(deployment.desiredState),
                  }}
                />
              }
            />
          ) : null}
          {statusMessage ? <Detail label="Message" value={statusMessage} /> : null}
          {!transitioning && startedAt ? (
            <Detail label="Started" title={startedAt.title} value={startedAt.text} />
          ) : null}
          {updatedAt ? (
            <Detail label="Updated" title={updatedAt.title} value={updatedAt.text} />
          ) : null}
          {failure?.code ? <Detail label="Failure code" value={failure.code} /> : null}
        </DetailList>
        {state?.progress ? <DeploymentProgress progress={state.progress} /> : null}
        {failure?.message ? (
          <div className="mt-3">
            {/* Notice renders a `p`, so the diagnostics stay inline content rather than block children. */}
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
      {model ? (
        <section aria-labelledby="deployment-configuration-heading">
          <InspectorSectionHeader
            action={
              <Button
                onClick={() => onOpenConfiguration?.()}
                size="sm"
                type="button"
                variant="link"
              >
                View all
                <ArrowRightIcon aria-hidden="true" className="size-3.5" />
              </Button>
            }
            headingId="deployment-configuration-heading"
            title="Configuration"
          />
          <DetailList>
            {quantization ? <Detail label="Quantization" value={quantization} /> : null}
            {totalContext === undefined ? null : (
              <ContextDetail label="Total context" tokens={totalContext} />
            )}
            {contextPerRequest === undefined ? null : (
              <ContextDetail
                derived={contextPerRequestDerived}
                label="Context per request"
                tokens={contextPerRequest}
              />
            )}
            {kvCachePrecision ? (
              <Detail label="KV cache precision" value={kvCachePrecision} />
            ) : null}
            {speculative ? <Detail label="Speculative decoding" value={speculative} /> : null}
          </DetailList>
        </section>
      ) : null}
      <section aria-labelledby="deployment-identity-heading">
        <InspectorSectionHeader headingId="deployment-identity-heading" title="Deployment" />
        <DetailList>
          <Detail label="Deployment ID" value={deployment.modelDeploymentId} />
          <Detail
            label="Model"
            value={<DeploymentModelMention href={catalogModelHref} snapshot={model} />}
          />
          {model ? <Detail label="Profile" value={model.profileId} /> : null}
          <Detail
            label="Placement"
            value={
              placement ? (
                <PanelMention colorKey={placement.siteNodeId} href={placementMachineHref}>
                  {placementName}
                </PanelMention>
              ) : (
                placementName
              )
            }
          />
          {devices?.length ? <Detail label="Devices" value={devices.join(", ")} /> : null}
          <Detail label="Routes" value={<RouteMentions routes={containingRoutes} />} />
        </DetailList>
        {memory}
      </section>
      {requests}
    </>
  );
}

/** Download progress of a deployment's artifacts, with a percentage once the total size is known. */
function DeploymentProgress({ progress }: { progress: ModelDeploymentProgress }) {
  const total = progress.total;
  const percent = progress.percent;
  const knownTotal = total !== undefined && percent !== undefined;
  const value = knownTotal ? percent : null;
  const summary = knownTotal
    ? `${formatBytes(progress.completed)} / ${formatBytes(total)} · ${percent}%`
    : `${formatBytes(progress.completed)} downloaded`;

  return (
    <Progress className="mt-3" value={value}>
      <ProgressLabel>Download progress</ProgressLabel>
      <ProgressValue>{() => summary}</ProgressValue>
    </Progress>
  );
}

function DeploymentModelMention({
  href,
  snapshot,
}: {
  href?: string;
  snapshot: ResolvedLocalModelDeploymentSnapshot | undefined;
}) {
  if (!snapshot) return <>Unavailable</>;

  return (
    <PanelMention colorKey={snapshot.modelId} href={href}>
      {snapshot.modelDisplayName}
    </PanelMention>
  );
}

function isPauseConflictingStatus(state: ModelDeploymentStatus): boolean {
  return (
    state === ModelDeploymentStatus.UNLOADING ||
    state === ModelDeploymentStatus.PAUSING ||
    state === ModelDeploymentStatus.PAUSED
  );
}

function ResolvedDeploymentDetails({
  snapshot,
}: {
  snapshot: ResolvedLocalModelDeploymentSnapshot;
}) {
  const argv = desiredArgv(snapshot);
  const resolvedSettings = resolvedModelEngineDiagnostics(snapshot);

  return (
    <>
      <section aria-labelledby="resolved-configuration-heading">
        <InspectorSectionHeader headingId="resolved-configuration-heading" title="Resolved" />
        <DetailList>
          <Detail label="Configuration" value={snapshot.configurationId} />
          <Detail label="Model profile" value={snapshot.profileId} />
          <Detail label="Artifact" value={snapshot.modelArtifact?.artifactId ?? "Unavailable"} />
          <Detail
            label="Weight quantization"
            value={weightQuantizationLabel(snapshot.modelArtifact?.weightQuantization ?? "")}
          />
          <Detail
            label="Engine"
            value={
              snapshot.engineArtifact
                ? `${snapshot.engineArtifact.engineReleaseId} / ${snapshot.engineArtifact.engineVersion}`
                : "Unavailable"
            }
          />
          <EngineParametersDetails
            schema={snapshot.engineParameterSchema}
            values={snapshot.engineParameterValues}
          />
          {resolvedSettings ? (
            <>
              <Detail label="GPU layers" value={String(resolvedSettings.gpuLayers)} />
              <Detail
                label="Flash Attention"
                value={resolvedSettings.flashAttention ? "Enabled" : "Disabled"}
              />
            </>
          ) : null}
        </DetailList>
        {snapshot.customArgumentTokens.length ? (
          <div className="mt-4">
            <h3 className="text-xs font-medium text-muted-foreground">Custom arguments</h3>
            <div className="mt-2 rounded-md border border-border p-3">
              <ArgumentTokenList
                ariaLabel="Custom argument tokens"
                tokens={snapshot.customArgumentTokens.map((text) => ({ text, placeholder: false }))}
              />
            </div>
          </div>
        ) : null}
      </section>
      <section aria-labelledby="deployment-command-heading">
        <InspectorSectionHeader
          action={
            <CodeCopyControl label="command" value={argv.map((token) => token.text).join("\n")} />
          }
          headingId="deployment-command-heading"
          title="Command"
        />
        <DesiredArgumentList argv={argv} className="mt-3" />
      </section>
    </>
  );
}

/**
 * A context detail showing the compact token count, with the exact count on hover. A value that
 * resolution derived rather than the deployment supplied is marked as derived.
 */
function ContextDetail({
  derived = false,
  label,
  tokens,
}: {
  derived?: boolean;
  label: string;
  tokens: bigint;
}) {
  return (
    <Detail
      label={label}
      title={`${tokens.toLocaleString("en-US")} tokens`}
      value={
        derived ? (
          <>
            {formatCompactCount(Number(tokens))}{" "}
            <span className="font-normal text-muted-foreground">derived</span>
          </>
        ) : (
          formatCompactCount(Number(tokens))
        )
      }
    />
  );
}

function resolvedEngineParameterValue(
  values: EngineParameterValues | undefined,
  parameterId: string,
): bigint | undefined {
  const parameter = values?.values.find(
    (candidate) =>
      candidate.parameterId === parameterId && candidate.value.case === "unsignedInteger",
  );
  return parameter?.value.case === "unsignedInteger" ? parameter.value.value : undefined;
}

export { CatalogModelInspector, DeploymentModelInspector };
export type { CatalogModelInspectorProps, DeploymentModelInspectorProps };
