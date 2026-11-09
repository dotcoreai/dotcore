import type { ReactNode, RefObject } from "react";

import { Combobox } from "@/components/ui/combobox";
import { FieldError } from "@/components/ui/field-error";
import {
  configuredExecutionTargetOptionLabel,
  executionTargetKey,
  executionTargetName,
  executionTargetPresentation,
} from "@/domain/execution-target";
import { DeploymentStepHeading } from "@/features/deployments/deployment-form-layout";
import type { ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";

interface ModelDeployTargetStepProps {
  disabled: boolean;
  error?: string;
  executionTargets: readonly ConfiguredExecutionTarget[];
  inputRef: RefObject<HTMLInputElement | null>;
  /** Whether the first target read is still in flight. */
  loading: boolean;
  /** Whether the picker presents loading, which outlasts the read for a minimum visible time. */
  loadingPresentation: boolean;
  loadingMessageVisible: boolean;
  /** Site-level notices shown above the picker. */
  notices?: ReactNode;
  onBlur: () => void;
  onSelect: (target: ConfiguredExecutionTarget | undefined) => void;
  placement: ExactPlacement | null;
  /** Whether the listed targets reflect the latest successful read. */
  targetsCurrent: boolean;
  /** Why the selected profile can't run on a target, which disables it; undefined when it can. */
  unavailableReason?: (target: ConfiguredExecutionTarget) => string | undefined;
}

/**
 * The Target step of the model deploy dialog: a picker of the configured model execution targets,
 * named by host, with targets that can't deploy or can't run the profile disabled with their
 * reason. A single listed target is shown as a line.
 */
export function ModelDeployTargetStep({
  disabled,
  error,
  executionTargets,
  inputRef,
  loading,
  loadingPresentation,
  loadingMessageVisible,
  notices,
  onBlur,
  onSelect,
  placement,
  targetsCurrent,
  unavailableReason,
}: ModelDeployTargetStepProps) {
  const selectable = (target: ConfiguredExecutionTarget) =>
    executionTargetPresentation(target).deployable && !unavailableReason?.(target);
  const deployableCount = executionTargets.filter(
    (target) => executionTargetPresentation(target).deployable,
  ).length;
  const selectedTarget = executionTargets.find(
    (target) =>
      target.executionTargetId === placement?.executionTargetId &&
      target.siteNodeId === placement.siteNodeId,
  );
  const onlyTarget =
    !loadingPresentation && executionTargets.length === 1 ? executionTargets[0] : undefined;

  return (
    <div className="grid gap-4">
      <DeploymentStepHeading>Target</DeploymentStepHeading>
      {notices}
      {onlyTarget ? (
        <div className="grid gap-1">
          <OnlyTargetLine target={onlyTarget} />
          {error ? <FieldError>{error}</FieldError> : null}
        </div>
      ) : executionTargets.length || loadingPresentation ? (
        <div className="grid gap-1">
          <Combobox
            ariaInvalid={Boolean(error)}
            description={
              loadingPresentation
                ? loadingMessageVisible
                  ? "Loading configured execution targets…"
                  : undefined
                : deployableCount === 0
                  ? "Configured targets are visible, but none are deployable."
                  : placement
                    ? undefined
                    : "Select an execution target."
            }
            disabled={disabled || loading || !targetsCurrent}
            errorDescriptionId={error ? "model-execution-target-error" : undefined}
            inputRef={inputRef}
            label="Execution target"
            loading={loadingPresentation}
            loadingMessageVisible={loadingMessageVisible}
            onBlur={onBlur}
            onValueChange={(value) =>
              onSelect(
                executionTargets.find(
                  (candidate) => executionTargetKey(candidate) === value && selectable(candidate),
                ),
              )
            }
            options={[...executionTargets].sort(compareTargets).map((target) => {
              const reason = unavailableReason?.(target);
              return {
                detail: target.platformLabel || "Unknown platform",
                disabled: !targetsCurrent || !selectable(target),
                id: executionTargetKey(target),
                label: reason
                  ? `${executionTargetName(target)} — ${reason}`
                  : configuredExecutionTargetOptionLabel(target),
              };
            })}
            searchable={executionTargets.length >= searchableTargetCount}
            value={selectedTarget ? executionTargetKey(selectedTarget) : ""}
          />
          {error ? <FieldError id="model-execution-target-error">{error}</FieldError> : null}
        </div>
      ) : null}
    </div>
  );
}

/** From this many targets up the picker filters as you type; fewer read better as a plain list. */
const searchableTargetCount = 8;

/**
 * Orders targets as the machines list their GPUs: by node name, then the first GPU index the
 * target holds, then target ID. Target IDs are hashes, so they alone give no meaningful order.
 */
function compareTargets(left: ConfiguredExecutionTarget, right: ConfiguredExecutionTarget) {
  const nodeName = (target: ConfiguredExecutionTarget) =>
    target.siteNodeDisplayName || target.siteNodeId;
  return (
    nodeName(left).localeCompare(nodeName(right)) ||
    firstGpuIndex(left) - firstGpuIndex(right) ||
    left.executionTargetId.localeCompare(right.executionTargetId)
  );
}

// The site names a GPU target with the enumeration indices of its GPUs, such as "… · GPU 1" or
// "… · GPUs 0, 1", the only index the console receives; a target without one sorts last.
function firstGpuIndex(target: ConfiguredExecutionTarget): number {
  const match = / · GPUs? (\d+)/.exec(target.displayName);
  return match ? Number(match[1]) : Number.MAX_SAFE_INTEGER;
}

function OnlyTargetLine({ target }: { target: ConfiguredExecutionTarget }) {
  const gate = executionTargetPresentation(target).deploymentGate;

  return (
    <div className="grid gap-1 text-sm">
      <p className="font-medium">Execution target</p>
      <p>
        {executionTargetName(target)}
        {gate ? <span className="text-muted-foreground"> — {gate}</span> : null}
      </p>
    </div>
  );
}
