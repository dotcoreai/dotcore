import type { RefObject } from "react";

import { Combobox } from "@/components/ui/combobox";
import { FieldError } from "@/components/ui/field-error";
import { Notice } from "@/components/ui/notice";
import {
  configuredExecutionTargetOptionLabel,
  executionTargetKey,
  executionTargetName,
  executionTargetPresentation,
} from "@/domain/execution-target";
import { DeploymentFormSection } from "@/features/deployments/deployment-form-layout";
import type { ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";

interface ModelDeployTargetPickerProps {
  disabled: boolean;
  error?: string;
  executionTargets: readonly ConfiguredExecutionTarget[];
  /** The control that takes focus for a blocked Next: the picker, or the notice of a single machine. */
  inputRef: RefObject<HTMLElement | null>;
  /** Whether the first target read is still in flight. */
  loading: boolean;
  /** Whether the picker presents loading, which outlasts the read for a minimum visible time. */
  loadingPresentation: boolean;
  loadingMessageVisible: boolean;
  onBlur: () => void;
  onSelect: (target: ConfiguredExecutionTarget | undefined) => void;
  placement: ExactPlacement | null;
  /** Whether the listed targets reflect the latest successful read. */
  targetsCurrent: boolean;
  /** Why the selected profile can't run on a target, which disables it; undefined when it can. */
  unavailableReason?: (target: ConfiguredExecutionTarget) => string | undefined;
}

/**
 * The deploy dialog's execution target choice: a picker of the configured model execution targets,
 * named by host, with targets that can't deploy or can't run the profile disabled with their
 * reason. A single listed target shows no picker, only a notice when it can't deploy.
 */
export function ModelDeployTargetPicker({
  disabled,
  error,
  executionTargets,
  inputRef,
  loading,
  loadingPresentation,
  loadingMessageVisible,
  onBlur,
  onSelect,
  placement,
  targetsCurrent,
  unavailableReason,
}: ModelDeployTargetPickerProps) {
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

  // A single machine needs no choice: the memory strip names it, and only what blocks it shows.
  if (onlyTarget) {
    const blocker =
      executionTargetPresentation(onlyTarget).deploymentGate ?? unavailableReason?.(onlyTarget);
    // The notice names the machine and why it can't take the profile, so it stands in for the
    // generic required error and takes the focus a blocked Next moves.
    return blocker || error ? (
      <div
        className="grid gap-2 outline-none"
        ref={(element) => {
          inputRef.current = element;
        }}
        tabIndex={-1}
      >
        {blocker ? (
          <Notice tone="warning">
            {executionTargetName(onlyTarget)} — {blocker}
          </Notice>
        ) : null}
        {error && !blocker ? <FieldError>{error}</FieldError> : null}
      </div>
    ) : null;
  }
  if (!executionTargets.length && !loadingPresentation) return null;

  return (
    <DeploymentFormSection title="Machine">
      <div className="grid min-w-0 gap-1">
        <Combobox
          ariaInvalid={Boolean(error)}
          status={
            loadingPresentation
              ? loadingMessageVisible
                ? "Loading configured execution targets…"
                : undefined
              : deployableCount === 0
                ? "Configured targets are visible, but none are deployable."
                : undefined
          }
          disabled={disabled || loading || !targetsCurrent}
          errorDescriptionId={error ? "model-execution-target-error" : undefined}
          hideLabel
          inputRef={(element) => {
            inputRef.current = element;
          }}
          label="Machine"
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
          placeholder="Choose a machine…"
          searchable={executionTargets.length >= searchableTargetCount}
          value={selectedTarget ? executionTargetKey(selectedTarget) : ""}
        />
        {error ? <FieldError id="model-execution-target-error">{error}</FieldError> : null}
      </div>
    </DeploymentFormSection>
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
