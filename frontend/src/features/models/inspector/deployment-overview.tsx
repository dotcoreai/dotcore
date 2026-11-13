import { ArrowRightIcon } from "lucide-react";
import type { ReactNode, RefObject } from "react";

import { Button } from "@/components/ui/button";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { Notice } from "@/components/ui/notice";
import { StatusBadge } from "@/components/ui/status-badge";
import { StepProgress, type StepProgressDetail } from "@/components/ui/step-progress";
import {
  deploymentStatus,
  isModelDesiredStateVisible,
  isTransitioningModelStatus,
  localModelSnapshot,
  modelDeploymentDesiredStateLabel,
  modelDeploymentDesiredStateTone,
  modelStatusMessage,
  projectModelDeploymentStatus,
} from "@/domain/model-projection";
import { placementLabel } from "@/domain/placement";
import {
  PanelMention,
  type RouteMention,
  RouteMentions,
} from "@/features/deployments/panel-mention";
import { deploymentSpeculativeLabel } from "@/features/models/runtime/deployment-speculative";
import {
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "@/features/models/runtime/engine-parameter-values";
import { DeploymentParametersSection } from "@/features/models/runtime/engine-parameters";
import { deploymentKvCachePrecisionLabel } from "@/features/models/runtime/kv-cache-precision";
import { type Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ParameterValues } from "@/gen/dotcore/catalog/v1/parameter_pb";
import {
  type ModelDeployment,
  type ResolvedLocalModelDeploymentSnapshot,
} from "@/gen/dotcore/model/v1/deployment_pb";
import {
  type ModelDeploymentProgress,
  ModelDeploymentStep,
  type ModelDeploymentSteps,
} from "@/gen/dotcore/model/v1/state_pb";
import { type ConfiguredExecutionTarget, type Machine } from "@/gen/dotcore/site/v1/service_pb";
import { formatBytes, formatCompactCount, formatRelativeTime } from "@/lib/format";

/**
 * The model deployment inspector root. It answers whether the deployment is healthy, how it is
 * configured, what it is and what it uses, then how it serves requests, as a stack of titled
 * sections, keeping the full resolved configuration one click away.
 */
export function DeploymentOverview({
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
  const contextPerRequestSized = Boolean(
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
        {state?.steps?.steps.length ? (
          <DeploymentSteps progress={state.progress} steps={state.steps} />
        ) : null}
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
                sized={contextPerRequestSized}
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
      {model ? <DeploymentParametersSection snapshot={model} /> : null}
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

/**
 * The step bar of a starting deployment. The download step also shows the artifacts' byte progress,
 * with a percentage once the total size is known.
 */
function DeploymentSteps({
  progress,
  steps,
}: {
  progress: ModelDeploymentProgress | undefined;
  steps: ModelDeploymentSteps;
}) {
  const downloading = steps.steps[steps.current] === ModelDeploymentStep.DOWNLOADING_ARTIFACTS;

  return (
    <StepProgress
      className="mt-3"
      current={steps.current}
      currentProgress={downloading && progress ? downloadProgress(progress) : undefined}
      steps={steps.steps.map(modelStepLabel)}
    />
  );
}

function downloadProgress(progress: ModelDeploymentProgress): StepProgressDetail {
  const { completed, percent, total } = progress;
  if (total === undefined || percent === undefined) {
    return { text: `${formatBytes(completed)} downloaded` };
  }

  return { percent, text: `${formatBytes(completed)} / ${formatBytes(total)} · ${percent}%` };
}

function modelStepLabel(step: ModelDeploymentStep): string {
  switch (step) {
    case ModelDeploymentStep.CHECKING_ARTIFACTS:
      return "Checking artifacts";
    case ModelDeploymentStep.DOWNLOADING_ARTIFACTS:
      return "Downloading artifacts";
    case ModelDeploymentStep.PREPARING_ENGINE:
      return "Preparing engine";
    case ModelDeploymentStep.STARTING:
      return "Starting";
    case ModelDeploymentStep.LOADING_MODEL:
      return "Loading model";
    default:
      return "Starting";
  }
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

/**
 * A context detail showing the compact token count, with the exact count on hover. A value that
 * resolution sized rather than the deployment supplied is marked as sized.
 */
function ContextDetail({
  sized = false,
  label,
  tokens,
}: {
  sized?: boolean;
  label: string;
  tokens: bigint;
}) {
  return (
    <Detail
      label={label}
      title={`${tokens.toLocaleString("en-US")} tokens`}
      value={
        sized ? (
          <>
            {formatCompactCount(Number(tokens))}{" "}
            <span className="font-normal text-muted-foreground">sized</span>
          </>
        ) : (
          formatCompactCount(Number(tokens))
        )
      }
    />
  );
}

function resolvedEngineParameterValue(
  values: ParameterValues | undefined,
  parameterId: string,
): bigint | undefined {
  const parameter = values?.values.find(
    (candidate) =>
      candidate.parameterId === parameterId && candidate.value.case === "unsignedInteger",
  );
  return parameter?.value.case === "unsignedInteger" ? parameter.value.value : undefined;
}
