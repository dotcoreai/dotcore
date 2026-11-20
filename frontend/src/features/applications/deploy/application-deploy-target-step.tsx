import type { RefObject } from "react";

import { FieldError } from "@/components/ui/field-error";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import {
  configuredExecutionTargetOptionLabel,
  executionTargetPresentation,
} from "@/domain/execution-target";
import { DeploymentStepHeading } from "@/features/deployments/deployment-form-layout";
import { ExecutionTargetSummary } from "@/features/deployments/execution-target-presentation";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";

interface ApplicationDeployTargetStepProps {
  disabled: boolean;
  error?: string;
  executionTargetId: string;
  /** The site's application execution targets, deployable or not, in display order. */
  executionTargets: readonly ConfiguredExecutionTarget[];
  inputRef: RefObject<HTMLSelectElement | null>;
  onBlur: () => void;
  onChange: (executionTargetId: string) => void;
  selectedExecutionTarget?: ConfiguredExecutionTarget;
  /** Whether the target list is current; choices wait for it. */
  targetsCurrent: boolean;
}

/**
 * The Target step of the application deploy dialog: the execution target the deployment runs on,
 * with the reason a configured target can't take it, and the selected target's facts.
 */
export function ApplicationDeployTargetStep({
  disabled,
  error,
  executionTargetId,
  executionTargets,
  inputRef,
  onBlur,
  onChange,
  selectedExecutionTarget,
  targetsCurrent,
}: ApplicationDeployTargetStepProps) {
  const anyDeployable = executionTargets.some(
    (target) => executionTargetPresentation(target).deployable,
  );

  return (
    <div className="grid gap-4">
      <DeploymentStepHeading>Target</DeploymentStepHeading>
      <div>
        <label className="block text-xs font-medium" htmlFor="application-execution-target">
          Execution target
        </label>
        <NativeSelect
          ref={inputRef}
          aria-describedby={error ? "application-execution-target-error" : undefined}
          aria-invalid={error ? "true" : undefined}
          className="mt-1 w-full"
          disabled={!targetsCurrent || !executionTargets.length || disabled}
          id="application-execution-target"
          onBlur={onBlur}
          onChange={(event) => onChange(event.target.value)}
          required
          value={executionTargetId}
        >
          <NativeSelectOption value="">Select an eligible execution target</NativeSelectOption>
          {executionTargets.map((target) => (
            <NativeSelectOption
              disabled={!targetsCurrent || !executionTargetPresentation(target).deployable}
              key={target.executionTargetId}
              value={target.executionTargetId}
            >
              {applicationTargetOptionLabel(target)}
            </NativeSelectOption>
          ))}
        </NativeSelect>
        {!executionTargets.length ? (
          <p className="mt-2 text-sm text-muted-foreground" role="status">
            No application execution targets are configured.
          </p>
        ) : !anyDeployable ? (
          <p className="mt-2 text-sm text-muted-foreground" role="status">
            Configured targets are visible, but none are deployable.
          </p>
        ) : null}
        {error ? (
          <FieldError className="mt-2" id="application-execution-target-error">
            {error}
          </FieldError>
        ) : null}
      </div>
      {selectedExecutionTarget ? (
        <div className="grid gap-2">
          <ExecutionTargetSummary target={selectedExecutionTarget} />
        </div>
      ) : null}
    </div>
  );
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
