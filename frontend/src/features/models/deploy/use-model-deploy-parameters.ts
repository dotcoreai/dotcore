import { useEffect, useRef, useState } from "react";

import {
  CONTEXT_PARAMETER_IDS,
  effectiveVisibility,
  engineParameterError,
  engineParameterValues,
} from "@/features/models/runtime/engine-parameter-values";
import {
  changedEngineParameters,
  runtimeCustomizations,
} from "@/features/models/runtime/local-deployment-configuration";
import type { EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import {
  type ParameterDefinition,
  type ParameterValues,
  ParameterVisibility,
} from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { DirtyFields } from "@/hooks/use-dirty-fields";
import type { FieldErrors } from "@/lib/form-validation";

import {
  type ContextStart,
  type LaunchContext,
  type ModelDeployField,
  useLaunchEngineValues,
} from "./model-deploy-launch";
import type { ProfileSelection } from "./use-model-deploy-machine";

/**
 * The deploy dialog's engine parameters: the inputs the launch shows over its starting values, the
 * values it submits, and where each parameter shows by the profile's visibility: the context
 * parameters, then the other basic ones, on Context; advanced ones on Performance & memory. A new
 * profile selection starts the edits from its values.
 */
export function useModelDeployParameters({
  context,
  contextStart,
  derivedParameterIds,
  dirtyFields,
  engineRelease,
  exposedParameters,
  fieldErrors,
  profile,
  scenarioValues,
  selection,
  setFieldError,
  setFieldErrors,
}: {
  context: LaunchContext;
  contextStart?: ContextStart;
  derivedParameterIds: ReadonlySet<string>;
  dirtyFields: DirtyFields<ModelDeployField>;
  engineRelease?: EngineRelease;
  exposedParameters: readonly ParameterDefinition[];
  fieldErrors: FieldErrors;
  profile?: ModelProfile;
  /** The context's read of the profile's values, which replace its defaults. */
  scenarioValues?: ParameterValues;
  selection: ProfileSelection;
  setFieldError: (field: string, error: string | undefined) => void;
  setFieldErrors: (update: (errors: FieldErrors) => FieldErrors) => void;
}) {
  // The operator's engine parameter edits over the inputs the profile or scenario starts from.
  const [editedInputs, setEditedInputs] = useState<Map<string, string>>(selection.values.edits);
  const [shownSelection, setShownSelection] = useState(selection);
  const engineParameterRefs = useRef<Map<string, HTMLElement | null>>(new Map());

  // Adjusting state during render starts a new selection's edits before the old ones render.
  if (shownSelection !== selection) {
    setShownSelection(selection);
    setEditedInputs(selection.values.edits);
  }

  const { declaredValues, engineParameterInputs } = useLaunchEngineValues(
    scenarioValues,
    engineRelease,
    profile,
    editedInputs,
    context,
    contextStart,
  );
  // The latest inputs, for a deferred blur check that runs after a change the blur preceded, such
  // as a total context preset clicked while the context per request had focus.
  const engineParameterInputsRef = useRef(engineParameterInputs);
  useEffect(() => {
    engineParameterInputsRef.current = engineParameterInputs;
  });
  const values = engineParameterValues(
    declaredValues,
    exposedParameters,
    engineParameterInputs,
    derivedParameterIds,
  );
  const contextParameters = exposedParameters.filter((parameter) =>
    CONTEXT_PARAMETER_IDS.has(parameter.parameterId),
  );
  // The other parameters split by the profile's visibility. Hidden ones are not exposed.
  const overrides = profile?.engineParameterVisibilityOverrides ?? [];
  const nonContextParameters = (visibility: ParameterVisibility) =>
    exposedParameters.filter(
      (parameter) =>
        !CONTEXT_PARAMETER_IDS.has(parameter.parameterId) &&
        effectiveVisibility(parameter, overrides) === visibility,
    );
  const basicParameters = nonContextParameters(ParameterVisibility.BASIC);
  const advancedParameters = nonContextParameters(ParameterVisibility.ADVANCED);
  const launchEdits = { declaredValues, inputs: engineParameterInputs };

  return {
    advancedParameters,
    basicParameters,
    blurParameter: (parameter: ParameterDefinition) => {
      if (!dirtyFields.isDirty(parameter.parameterId)) return;
      setTimeout(
        () =>
          setFieldError(
            parameter.parameterId,
            engineParameterError(parameter, engineParameterInputsRef.current, derivedParameterIds),
          ),
        0,
      );
    },
    /** Each changed engine parameter outside the context, for the summary. */
    changedParameters: changedEngineParameters(profile, engineRelease, launchEdits),
    /** A change to a parameter outside the context, on Context or Performance & memory. */
    changeParameter: (parameter: ParameterDefinition, nextInput: string) => {
      dirtyFields.markDirty(parameter.parameterId);
      const nextInputs = new Map(engineParameterInputs).set(parameter.parameterId, nextInput);
      setEditedInputs((inputs) => new Map(inputs).set(parameter.parameterId, nextInput));
      if (fieldErrors.has(parameter.parameterId))
        setFieldError(
          parameter.parameterId,
          engineParameterError(parameter, nextInputs, derivedParameterIds),
        );
    },
    contextParameters,
    /** The parameters edited away from the profile's values, which a profile change resets. */
    customizations: runtimeCustomizations(profile, engineRelease, launchEdits),
    declaredValues,
    engineParameterInputs,
    engineParameterInputsRef,
    engineParameterRefs,
    fieldControls: (): [string, HTMLElement | null][] =>
      exposedParameters.map((parameter) => [
        parameter.parameterId,
        engineParameterRefs.current.get(parameter.parameterId) ?? null,
      ]),
    /** The current errors of the given parameters. */
    parameterErrors: (parameters: readonly ParameterDefinition[]): [string, string | undefined][] =>
      parameters.map((parameter) => [
        parameter.parameterId,
        engineParameterError(parameter, engineParameterInputs, derivedParameterIds),
      ]),
    /** Returns the advanced parameters to the profile's values, as when Advanced turns off. */
    resetAdvanced: () => {
      const advancedIds = new Set(advancedParameters.map(({ parameterId }) => parameterId));
      setEditedInputs(
        (inputs) => new Map([...inputs].filter(([parameterId]) => !advancedIds.has(parameterId))),
      );
      setFieldErrors((errors) => {
        const nextErrors = new Map(errors);
        for (const parameterId of advancedIds) nextErrors.delete(parameterId);
        return nextErrors;
      });
    },
    values,
  };
}
