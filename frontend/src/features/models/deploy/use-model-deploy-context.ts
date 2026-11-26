import type { Dispatch, SetStateAction } from "react";

import { executionTargetName } from "@/domain/execution-target";
import type { CompatibilityVerdict } from "@/features/models/profiles/profile-compatibility";
import { requestCount } from "@/features/models/runtime/context-slider-ends";
import {
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  engineParameterError,
  PARALLEL_REQUESTS_PARAMETER_ID,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "@/features/models/runtime/engine-parameter-values";
import type { ParameterDefinition } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";
import type { DirtyFields } from "@/hooks/use-dirty-fields";
import type { FieldErrors } from "@/lib/form-validation";
import { formatCompactCount, formatCount } from "@/lib/format";

import {
  followingContext,
  type LaunchContext,
  type ModelDeployField,
  operatorSentence,
  scenarioInput,
  settledContext,
  totalFollows,
} from "./model-deploy-launch";
import type { ResourceEstimate } from "./use-resource-estimate";

/**
 * The deploy dialog's Context step: edits to the launch's shared context, and what the step says
 * about it on the selected target, its largest fits and a launch rejection. A settled edit sizes
 * the context's read, which starts the other values from it.
 */
export function useModelDeployContext({
  contextParameters,
  derivedParameterIds,
  dirtyFields,
  engineParameterInputs,
  engineParameterInputsRef,
  estimate,
  fieldErrors,
  context,
  placement,
  profileId,
  selectedExecutionTarget,
  setContext,
  setFieldError,
  setFieldErrors,
  targetVerdict,
}: {
  context: LaunchContext;
  /** The context parameters the profile exposes. */
  contextParameters: readonly ParameterDefinition[];
  derivedParameterIds: ReadonlySet<string>;
  dirtyFields: DirtyFields<ModelDeployField>;
  engineParameterInputs: ReadonlyMap<string, string>;
  /** The latest inputs, for checks that run after the render a change causes. */
  engineParameterInputsRef: { current: ReadonlyMap<string, string> };
  estimate: ResourceEstimate;
  fieldErrors: FieldErrors;
  placement: ExactPlacement | null;
  profileId: string;
  selectedExecutionTarget?: ConfiguredExecutionTarget;
  setContext: Dispatch<SetStateAction<LaunchContext>>;
  setFieldError: (field: string, error: string | undefined) => void;
  setFieldErrors: Dispatch<SetStateAction<FieldErrors>>;
  /** The selected target's verdict from the context's read. */
  targetVerdict?: CompatibilityVerdict;
}) {
  // The largest fits with the dialog's values, from the estimate of the selected profile on the
  // selected target; the read's values for the target stand in until that estimate lands. An
  // estimate without a fit, such as with custom arguments, offers none.
  const estimateMatches =
    placement !== null &&
    estimate.profileId === profileId &&
    estimate.placement?.siteNodeId === placement.siteNodeId &&
    estimate.placement.executionTargetId === placement.executionTargetId;
  const largestTotalContext = estimateMatches
    ? estimate.largestTotalContext
    : targetVerdict?.largestTotalContextTokens;
  const largestContextPerRequest = estimateMatches
    ? estimate.largestContextPerRequest
    : targetVerdict?.largestContextTokensPerRequest;
  const targetName = selectedExecutionTarget && executionTargetName(selectedExecutionTarget);
  const parallelRequests = engineParameterInputs.get(PARALLEL_REQUESTS_PARAMETER_ID);
  // A rejection spans the context values, so it shows as soon as it's known, on the context per
  // request, which leads the step, else on the first context field. A profile that sets its
  // context itself has no field to fix.
  const launchRejectionParameter =
    contextParameters.find(({ parameterId }) => parameterId === CONTEXT_PER_REQUEST_PARAMETER_ID) ??
    contextParameters[0];
  const launchRejectionMessage =
    estimate.launchRejection && launchRejectionParameter
      ? operatorSentence(estimate.launchRejection)
      : undefined;
  const revalidate = (nextInputs: ReadonlyMap<string, string>) => {
    for (const shownParameter of contextParameters)
      if (fieldErrors.has(shownParameter.parameterId))
        setFieldError(
          shownParameter.parameterId,
          engineParameterError(shownParameter, nextInputs, derivedParameterIds),
        );
  };

  return {
    // A typed context settles once left, as a settled change does.
    blurContext: (parameter: ParameterDefinition) => {
      if (!dirtyFields.isDirty(parameter.parameterId)) return;
      const shownPerRequest = engineParameterInputs.get(CONTEXT_PER_REQUEST_PARAMETER_ID) ?? "";
      setContext((previous) => settledContext(previous, shownPerRequest));
      // Runs after a change the blur preceded, such as a total context preset clicked while the
      // context per request had focus, and re-validates any context field the settling moved.
      setTimeout(() =>
        setFieldErrors((errors) => {
          const nextErrors = new Map(errors);
          for (const shownParameter of contextParameters) {
            if (shownParameter !== parameter && !errors.has(shownParameter.parameterId)) continue;
            const error = engineParameterError(
              shownParameter,
              engineParameterInputsRef.current,
              derivedParameterIds,
            );
            if (error) nextErrors.set(shownParameter.parameterId, error);
            else nextErrors.delete(shownParameter.parameterId);
          }
          return nextErrors;
        }),
      );
    },
    // A context change edits the shared context: the edited value becomes the operator's, and a
    // total context left alone keeps following. A settled change, from a slider or preset, also
    // sizes the context's read when valid, so it starts the rest from it; typing waits until the
    // input is left.
    changeContext: (parameter: ParameterDefinition, nextInput: string, settled?: boolean) => {
      const edited = scenarioInput(parameter.parameterId);
      if (!edited) return;

      dirtyFields.markDirty(parameter.parameterId);
      const shownPerRequest = engineParameterInputs.get(CONTEXT_PER_REQUEST_PARAMETER_ID) ?? "";
      setContext((previous) => {
        const next = { ...previous, edits: { ...previous.edits, [edited]: nextInput } };
        return settled ? settledContext(next, shownPerRequest) : next;
      });
      // A shown error re-validates, including one the changed value causes or clears on another
      // field, such as a context per request above a new total context.
      revalidate(new Map(engineParameterInputs).set(parameter.parameterId, nextInput));
    },
    // Each is offered as its field's last chip, and choosing it sets the shown value like a size
    // chip; its hint names the target, and the parallel requests the context per request fits with.
    contextFits: {
      perRequest:
        largestContextPerRequest === undefined ? undefined : { value: largestContextPerRequest },
      total: largestTotalContext === undefined ? undefined : { value: largestTotalContext },
    },
    contextHints: {
      perRequest:
        largestContextPerRequest !== undefined && targetName
          ? `Up to ${formatCompactCount(largestContextPerRequest)} with ${formatCount(requestCount(parallelRequests), "parallel request")} on ${targetName}`
          : undefined,
      total:
        largestTotalContext !== undefined && targetName
          ? `Up to ${formatCompactCount(largestTotalContext)} on ${targetName}`
          : undefined,
    },
    /** The step's field errors, with a launch rejection on its field. */
    contextFieldErrors: launchRejectionMessage
      ? new Map(fieldErrors).set(launchRejectionParameter.parameterId, launchRejectionMessage)
      : fieldErrors,
    /** The total context follows the context per request and parallel requests again. */
    followTotal: () => {
      setContext(followingContext);
      setFieldError(TOTAL_CONTEXT_PARAMETER_ID, undefined);
    },
    launchRejectionMessage,
    launchRejectionParameter,
    totalFollows: totalFollows(context),
  };
}
