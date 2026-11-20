import type { RefObject } from "react";

import { Button } from "@/components/ui/button";
import { Combobox } from "@/components/ui/combobox";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { FieldError } from "@/components/ui/field-error";
import { FormGroup } from "@/components/ui/form-group";
import { Notice } from "@/components/ui/notice";
import { weightQuantizationLabel } from "@/domain/model-projection";
import {
  DeploymentForm,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import type { SpeculativeOption } from "@/features/models/profiles/model-row";
import { CustomArgumentTokenEditor } from "@/features/models/runtime/custom-argument-token-editor";
import { runtimeParameterGroups } from "@/features/models/runtime/engine-parameter-values";
import { EngineParametersForm } from "@/features/models/runtime/engine-parameters";
import type { KvCachePrecisionOption } from "@/features/models/runtime/kv-cache-precision";
import { KvCachePrecisionField } from "@/features/models/runtime/kv-cache-precision-field";
import type { MemoryParameterSizes } from "@/features/models/runtime/memory-parameter-sizes";
import type { SpeculativeChoice } from "@/features/models/runtime/speculative-choice";
import { SpeculativeDecodingField } from "@/features/models/runtime/speculative-decoding-field";
import type {
  Model,
  ModelEngineConfiguration,
  ModelProfile,
} from "@/gen/dotcore/catalog/v1/model_pb";
import type { ParameterDefinition, ParameterGroup } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { FieldErrors } from "@/lib/form-validation";

interface ModelDeployAdvancedStepProps {
  artifactRef: RefObject<HTMLInputElement | null>;
  bindingId: string;
  customArgumentTokens: readonly string[];
  /** Custom argument token edits, by the editor's operations. */
  customArguments: {
    add: () => void;
    move: (index: number, direction: -1 | 1) => void;
    remove: (index: number) => void;
    update: (index: number, value: string) => void;
  };
  disabled: boolean;
  engineParameterRefs: RefObject<Map<string, HTMLElement | null>>;
  /** The engine releases the deployment may run: the target's selected one, or every approved one. */
  engineReleases: readonly string[];
  /** The advanced engine parameters outside the context. */
  exposedParameters: readonly ParameterDefinition[];
  fieldErrors: FieldErrors;
  inputs: ReadonlyMap<string, string>;
  /** Warns that host KV offload shares the accelerator's memory pool on this target. */
  hostKvOffloadSharesMemory: boolean;
  /** The latest estimate's memory parameter sizes, for the memory parameters' sliders. */
  memoryParameterSizes?: MemoryParameterSizes;
  kvCachePrecision: {
    onChange: (precision: string) => void;
    /** The configuration's KV cache precision option; the field shows only when there is one. */
    option?: KvCachePrecisionOption;
    precision: string;
  };
  model: Model;
  onArtifactBlur: () => void;
  onArtifactChange: (bindingId: string) => void;
  onParameterBlur: (parameter: ParameterDefinition) => void;
  onParameterChange: (parameter: ParameterDefinition, input: string) => void;
  onPreview: () => void;
  /** The engine's parameter groups the configuration keeps. */
  parameterGroups: readonly ParameterGroup[];
  previewButtonRef: RefObject<HTMLButtonElement | null>;
  previewDisabled: boolean;
  previewPending: boolean;
  profile?: ModelProfile;
  /** The errors of the latest blocked navigation; a new value opens the sections and groups it flags. */
  revealedErrors: FieldErrors;
  /** Whether the profile shows any engine parameter, on this step or the Settings step. */
  schemaExposesParameters: boolean;
  speculative: {
    choice: SpeculativeChoice;
    countRef: RefObject<HTMLInputElement | null>;
    onCountBlur: () => void;
    onCountChange: (count: string) => void;
    onMethodChange: (optionId: string) => void;
    /** The configuration's speculative options; the field shows only when there is one. */
    options: readonly SpeculativeOption[];
  };
}

/**
 * The Advanced step of the model deploy dialog, driven by the selected profile's advanced engine
 * parameters and options: the parameters laid out by the engine's parameter groups, the KV cache
 * precision and speculative decoding choices, then the model artifact and custom arguments in
 * collapsed sections. The context parameters and basic parameters
 * belong to the Settings step. A collapsed section or group opens when a blocked navigation flags
 * one of its fields, so the field can take focus.
 */
export function ModelDeployAdvancedStep({
  artifactRef,
  bindingId,
  customArgumentTokens,
  customArguments,
  disabled,
  engineParameterRefs,
  engineReleases,
  exposedParameters,
  fieldErrors,
  hostKvOffloadSharesMemory,
  inputs,
  kvCachePrecision,
  memoryParameterSizes,
  model,
  onArtifactBlur,
  onArtifactChange,
  onParameterBlur,
  onParameterChange,
  onPreview,
  parameterGroups,
  previewButtonRef,
  previewDisabled,
  previewPending,
  profile,
  revealedErrors,
  schemaExposesParameters,
  speculative,
}: ModelDeployAdvancedStepProps) {
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );
  const binding = configuration?.artifactBindings.find(
    (candidate) => candidate.bindingId === bindingId,
  );
  const artifact = model.artifacts.find(
    (candidate) => candidate.artifactId === binding?.modelArtifactId,
  );
  const { others } = runtimeParameterGroups(exposedParameters);
  const parametersForm = (parameters: readonly ParameterDefinition[]) => (
    <EngineParametersForm
      errors={fieldErrors}
      groups={parameterGroups}
      inputRefs={engineParameterRefs}
      inputs={inputs}
      memoryParameterSizes={memoryParameterSizes}
      onBlur={onParameterBlur}
      onChange={onParameterChange}
      parameters={parameters}
      revealedErrors={revealedErrors}
    />
  );

  return (
    <DeploymentForm className="gap-4">
      <DeploymentStepHeading>Advanced</DeploymentStepHeading>
      {schemaExposesParameters ? null : (
        <Notice role="alert" tone="destructive">
          The selected engine release schema has no exposed parameters.
        </Notice>
      )}
      {others.length ? parametersForm(others) : null}
      {hostKvOffloadSharesMemory ? (
        <Notice tone="warning">
          KV cache offload adds no capacity on this target. Its GPU already uses shared system
          memory, so offloaded cache takes memory from the same pool.
        </Notice>
      ) : null}
      {kvCachePrecision.option ? (
        <KvCachePrecisionField
          disabled={disabled}
          onChange={kvCachePrecision.onChange}
          option={kvCachePrecision.option}
          precision={kvCachePrecision.precision}
        />
      ) : null}
      {speculative.options.length ? (
        <SpeculativeDecodingField
          choice={speculative.choice}
          countRef={speculative.countRef}
          disabled={disabled}
          error={fieldErrors.get("speculativeCount")}
          onCountBlur={speculative.onCountBlur}
          onCountChange={speculative.onCountChange}
          onMethodChange={speculative.onMethodChange}
          options={speculative.options}
        />
      ) : null}
      <FormGroup
        defaultOpen={false}
        errors={fieldErrors}
        fields={["artifact"]}
        revealedErrors={revealedErrors}
        title="Engine and artifact"
      >
        <Combobox
          ariaInvalid={Boolean(fieldErrors.get("artifact"))}
          disabled={disabled || !configuration}
          errorDescriptionId={fieldErrors.get("artifact") ? "model-artifact-error" : undefined}
          inputRef={artifactRef}
          label="Model artifact"
          onBlur={onArtifactBlur}
          onValueChange={(value) => onArtifactChange(value ?? "")}
          options={artifactOptions(model, configuration)}
          value={bindingId}
        />
        {fieldErrors.get("artifact") ? (
          <FieldError id="model-artifact-error">{fieldErrors.get("artifact")}</FieldError>
        ) : null}
        <DetailList>
          <Detail label="Configuration" value={configuration?.configurationId ?? "Unavailable"} />
          <Detail label="Engine" value={engineReleases.join(", ") || "Unavailable"} />
          {artifact ? (
            <Detail
              label="Weight quantization"
              value={weightQuantizationLabel(artifact.weightQuantization)}
            />
          ) : null}
        </DetailList>
      </FormGroup>
      <FormGroup
        defaultOpen={false}
        errors={fieldErrors}
        revealedErrors={revealedErrors}
        title="Custom arguments and command"
      >
        <CustomArgumentTokenEditor
          disabled={disabled}
          onAdd={customArguments.add}
          onMove={customArguments.move}
          onRemove={customArguments.remove}
          onUpdate={customArguments.update}
          tokens={customArgumentTokens}
        />
        <Button
          disabled={previewDisabled}
          onClick={onPreview}
          ref={previewButtonRef}
          type="button"
          variant="outline"
        >
          {previewPending ? "Previewing…" : "Preview resolved command"}
        </Button>
      </FormGroup>
    </DeploymentForm>
  );
}

/**
 * The model artifact choices of a configuration, each named by the profile that launches it by
 * default, since the Advanced step shows the artifact's quantization beside the choice. An artifact
 * no profile defaults to, or one whose name another choice shares, is named by its artifact ID.
 */
function artifactOptions(model: Model, configuration: ModelEngineConfiguration | undefined) {
  const bindings = configuration?.artifactBindings ?? [];
  const names = bindings.map(
    (binding) =>
      model.profiles.find(
        (profile) =>
          profile.configurationId === configuration?.configurationId &&
          profile.defaultModelArtifactBindingId === binding.bindingId,
      )?.displayName || binding.modelArtifactId,
  );
  return bindings.map((binding, index) => ({
    id: binding.bindingId,
    label:
      names.indexOf(names[index]) === names.lastIndexOf(names[index])
        ? names[index]
        : binding.modelArtifactId,
  }));
}
