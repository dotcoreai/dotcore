import { useRef, useState } from "react";

import { configurationSpeculativeOptions } from "@/features/models/profiles/model-row";
import {
  configurationKvCachePrecision,
  currentKvCachePrecision,
  kvCachePrecisionCustomizations,
  kvCachePrecisionOverrides,
} from "@/features/models/runtime/kv-cache-precision";
import {
  modelOptionVisibility,
  mostVisible,
} from "@/features/models/runtime/model-option-visibility";
import {
  currentSpeculativeChoice,
  defaultSpeculativeChoice,
  type SpeculativeChoice,
  speculativeChoiceError,
  speculativeCustomizations,
  speculativeMethodChoice,
  speculativeOptionOverrides,
} from "@/features/models/runtime/speculative-choice";
import {
  configurationVisionOption,
  currentVisionEnabled,
  visionOverrides,
} from "@/features/models/runtime/vision-option";
import type {
  Model,
  ModelEngineConfiguration,
  ModelProfile,
} from "@/gen/dotcore/catalog/v1/model_pb";
import { ParameterVisibility } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { DirtyFields } from "@/hooks/use-dirty-fields";
import type { FieldErrors } from "@/lib/form-validation";

import type { ModelDeployField } from "./model-deploy-launch";
import type { ProfileSelection } from "./use-model-deploy-machine";

/** An option control the deploy dialog can show; each places by its catalog visibility. */
export type ModelOptionControl = "vision" | "kvCachePrecision" | "speculative";

/**
 * The deploy dialog's model options: vision, KV cache precision and speculative decoding, their
 * values over the profile's defaults, the overrides a launch sends, and where each control shows:
 * basic ones on Capabilities, advanced ones on Performance & memory, hidden ones nowhere. A new
 * profile selection starts them from its values.
 */
export function useModelDeployOptions({
  configuration,
  dirtyFields,
  fieldErrors,
  model,
  profile,
  selection,
  setFieldError,
}: {
  configuration?: ModelEngineConfiguration;
  dirtyFields: DirtyFields<ModelDeployField>;
  fieldErrors: FieldErrors;
  model: Model;
  profile?: ModelProfile;
  selection: ProfileSelection;
  setFieldError: (field: string, error: string | undefined) => void;
}) {
  const [storedSpeculativeChoice, setSpeculativeChoice] = useState<SpeculativeChoice>(
    selection.values.speculativeChoice,
  );
  // The operator's KV cache precision pick; empty while the profile's default stands.
  const [pickedKvCachePrecision, setPickedKvCachePrecision] = useState(
    selection.values.kvCachePrecision,
  );
  // The operator's vision choice; undefined while the profile's default stands.
  const [pickedVision, setPickedVision] = useState(selection.values.vision);
  const [shownSelection, setShownSelection] = useState(selection);
  const speculativeCountRef = useRef<HTMLInputElement>(null);

  // Adjusting state during render starts a new selection's values before the old ones render.
  if (shownSelection !== selection) {
    setShownSelection(selection);
    setSpeculativeChoice(selection.values.speculativeChoice);
    setPickedKvCachePrecision(selection.values.kvCachePrecision);
    setPickedVision(selection.values.vision);
  }

  const speculativeOptions = configurationSpeculativeOptions(model, configuration, profile);
  const speculativeModelOption = model.options.find(
    (option) => option.optionId === speculativeOptions[0]?.optionId,
  );
  const kvCachePrecisionOption = configurationKvCachePrecision(model, configuration, profile);
  const visionOption = configurationVisionOption(model, configuration, profile);
  // Each control the configuration declares, by the visibility its option declares; speculative
  // decoding is one choice over its options, shown as its most visible one.
  const visibilities: Partial<Record<ModelOptionControl, ParameterVisibility>> = {
    kvCachePrecision:
      kvCachePrecisionOption && modelOptionVisibility(model, kvCachePrecisionOption.optionId),
    speculative: speculativeOptions.length
      ? mostVisible(
          model,
          speculativeOptions.map(({ optionId }) => optionId),
        )
      : undefined,
    vision: visionOption && modelOptionVisibility(model, visionOption.optionId),
  };
  // A hidden option is not the operator's to set, so it keeps the profile's value whatever was
  // restored from an earlier configuration.
  const isHidden = (control: ModelOptionControl) =>
    visibilities[control] === ParameterVisibility.HIDDEN;
  const speculativeChoice = isHidden("speculative")
    ? defaultSpeculativeChoice(speculativeOptions)
    : currentSpeculativeChoice(speculativeOptions, storedSpeculativeChoice);
  // Undefined while the speculative count is invalid, which holds back estimates like other values.
  const speculativeOverrides = speculativeOptionOverrides(speculativeOptions, speculativeChoice);
  const kvCachePrecision = currentKvCachePrecision(
    kvCachePrecisionOption,
    isHidden("kvCachePrecision") ? "" : pickedKvCachePrecision,
  );
  const visionEnabled = currentVisionEnabled(
    visionOption,
    isHidden("vision") ? undefined : pickedVision,
  );
  const optionOverrides = speculativeOverrides && [
    ...speculativeOverrides,
    ...kvCachePrecisionOverrides(kvCachePrecisionOption, kvCachePrecision),
    ...visionOverrides(visionOption, visionEnabled),
  ];

  const controlsAt = (visibility: ParameterVisibility): ModelOptionControl[] =>
    controlOrder.filter((control) => visibilities[control] === visibility);
  const speculativeError = () => speculativeChoiceError(speculativeOptions, speculativeChoice);
  // Each option that differs from the profile's, in a few words, with where it shows.
  const changes = [
    ...speculativeCustomizations(speculativeOptions, speculativeChoice).map((text) => ({
      text,
      visibility: visibilities.speculative,
    })),
    ...kvCachePrecisionCustomizations(kvCachePrecisionOption, kvCachePrecision).map((text) => ({
      text,
      visibility: visibilities.kvCachePrecision,
    })),
    ...(visionOption && visionEnabled !== visionOption.defaultEnabled
      ? [
          {
            text: `${visionOption.displayName} ${onOff(visionEnabled).toLowerCase()}`,
            visibility: visibilities.vision,
          },
        ]
      : []),
  ];

  return {
    /** The controls Capabilities shows. */
    basicControls: controlsAt(ParameterVisibility.BASIC),
    /** The controls Performance & memory shows. */
    advancedControls: controlsAt(ParameterVisibility.ADVANCED),
    /** The errors of the controls shown at a visibility. */
    errorsAt: (visibility: ParameterVisibility): [string, string | undefined][] =>
      visibilities.speculative === visibility ? [["speculativeCount", speculativeError()]] : [],
    /** Whether any option differs from the profile's, which a profile change resets. */
    changed: changes.length > 0,
    /** The advanced options that differ from the profile's, in a few words each. */
    customizations: changes
      .filter(({ visibility }) => visibility === ParameterVisibility.ADVANCED)
      .map(({ text }) => text),
    fieldControls: (): [string, HTMLElement | null][] => [
      ["speculativeCount", speculativeCountRef.current],
    ],
    kvCachePrecision: {
      onChange: setPickedKvCachePrecision,
      option: kvCachePrecisionOption,
      precision: kvCachePrecision,
    },
    /** Undefined while an option value is invalid. */
    optionOverrides,
    /** Returns the advanced controls to the profile's values, as when Advanced turns off. */
    resetAdvanced: () => {
      const defaults = configurationSpeculativeOptions(model, configuration, profile);
      if (visibilities.speculative === ParameterVisibility.ADVANCED) {
        setSpeculativeChoice(defaultSpeculativeChoice(defaults));
        setFieldError("speculativeCount", undefined);
      }
      if (visibilities.kvCachePrecision === ParameterVisibility.ADVANCED)
        setPickedKvCachePrecision("");
      if (visibilities.vision === ParameterVisibility.ADVANCED) setPickedVision(undefined);
    },
    speculative: {
      choice: speculativeChoice,
      countRef: speculativeCountRef,
      description: speculativeModelOption?.description,
      // Validates at once: a deferred check would run after a method change that the blur
      // precedes and overwrite its error with the previous method's verdict.
      onCountBlur: () => {
        if (dirtyFields.isDirty("speculativeCount"))
          setFieldError("speculativeCount", speculativeError());
      },
      onCountChange: (count: string) => {
        const nextChoice = { ...speculativeChoice, count };
        dirtyFields.markDirty("speculativeCount");
        setSpeculativeChoice(nextChoice);
        if (fieldErrors.has("speculativeCount"))
          setFieldError("speculativeCount", speculativeChoiceError(speculativeOptions, nextChoice));
      },
      // A kept count can fall outside the new method's range, so the method change validates it
      // at once rather than waiting for a blur the count input never received.
      onMethodChange: (optionId: string) => {
        const nextChoice = speculativeMethodChoice(
          speculativeOptions,
          configurationSpeculativeOptions(model, configuration),
          speculativeChoice,
          optionId,
        );
        dirtyFields.markDirty("speculativeCount");
        setSpeculativeChoice(nextChoice);
        setFieldError("speculativeCount", speculativeChoiceError(speculativeOptions, nextChoice));
      },
      options: speculativeOptions,
      summary: speculativeModelOption?.summary,
    },
    /** The basic choices for the summary, such as "Vision: On". */
    summary: controlsAt(ParameterVisibility.BASIC).flatMap((control) => {
      switch (control) {
        case "vision":
          return visionOption
            ? [{ label: visionOption.displayName, value: onOff(visionEnabled) }]
            : [];
        case "kvCachePrecision":
          return kvCachePrecisionOption
            ? [{ label: kvCachePrecisionOption.displayName, value: kvCachePrecision }]
            : [];
        case "speculative": {
          const chosen = speculativeOptions.find(
            ({ optionId }) => optionId === speculativeChoice.optionId,
          );
          return [
            {
              label: "Speculative decoding",
              value: chosen ? `${chosen.displayName}, ${speculativeChoice.count} tokens` : "Off",
            },
          ];
        }
      }
    }),
    vision: { enabled: visionEnabled, onChange: setPickedVision, option: visionOption },
  };
}

// The order controls show in on a step.
const controlOrder: readonly ModelOptionControl[] = ["vision", "kvCachePrecision", "speculative"];

function onOff(enabled: boolean): string {
  return enabled ? "On" : "Off";
}
