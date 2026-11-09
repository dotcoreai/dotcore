import { Collapsible } from "@base-ui/react/collapsible";
import { ChevronRightIcon } from "lucide-react";
import type { ReactNode, RefObject } from "react";
import { useState } from "react";

import { Button } from "@/components/ui/button";
import { Combobox } from "@/components/ui/combobox";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { FieldError } from "@/components/ui/field-error";
import { Notice } from "@/components/ui/notice";
import { weightQuantizationLabel } from "@/domain/model-projection";
import {
  DeploymentForm,
  DeploymentFormSection,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import type { EngineParameterDefinition } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type {
  Model,
  ModelEngineConfiguration,
  ModelProfile,
} from "@/gen/dotcore/catalog/v1/model_pb";
import type { FieldErrors } from "@/lib/form-validation";

import type { SpeculativeOption } from "./catalog-model-row";
import { CustomArgumentTokenEditor } from "./custom-argument-token-editor";
import { runtimeParameterGroups } from "./engine-parameter-values";
import { EngineParametersForm } from "./engine-parameters";
import type { KvCachePrecisionOption } from "./kv-cache-precision";
import { KvCachePrecisionField } from "./kv-cache-precision-field";
import type { SpeculativeChoice } from "./speculative-choice";
import { SpeculativeDecodingField } from "./speculative-decoding-field";

interface ModelDeployRuntimeStepProps {
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
  exposedParameters: readonly EngineParameterDefinition[];
  fieldErrors: FieldErrors;
  inputs: ReadonlyMap<string, string>;
  /** Warns that host KV offload shares the accelerator's memory pool on this target. */
  hostKvOffloadSharesMemory: boolean;
  kvCachePrecision: {
    onChange: (precision: string) => void;
    /** The configuration's KV cache precision option; the field shows only when there is one. */
    option?: KvCachePrecisionOption;
    precision: string;
  };
  model: Model;
  onArtifactBlur: () => void;
  onArtifactChange: (bindingId: string) => void;
  onParameterBlur: (parameter: EngineParameterDefinition) => void;
  onParameterChange: (parameter: EngineParameterDefinition, input: string) => void;
  onPreview: () => void;
  previewButtonRef: RefObject<HTMLButtonElement | null>;
  previewDisabled: boolean;
  previewPending: boolean;
  profile?: ModelProfile;
  /** The errors of the latest blocked navigation; a new value opens the sections it flags. */
  revealedErrors: FieldErrors;
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
 * The advanced Runtime step of the model deploy dialog, driven by the selected profile's engine
 * parameter schema and options: parallel requests, a host memory group, the KV cache precision and
 * speculative decoding choices, and the remaining parameters, model artifact, and custom arguments
 * in collapsed sections. The context sizes belong to the Context step. A collapsed section opens
 * when a blocked navigation flags one of its fields, so the field can take focus.
 */
export function ModelDeployRuntimeStep({
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
  model,
  onArtifactBlur,
  onArtifactChange,
  onParameterBlur,
  onParameterChange,
  onPreview,
  previewButtonRef,
  previewDisabled,
  previewPending,
  profile,
  revealedErrors,
  speculative,
}: ModelDeployRuntimeStepProps) {
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );
  const binding = configuration?.artifactBindings.find(
    (candidate) => candidate.bindingId === bindingId,
  );
  const artifact = model.artifacts.find(
    (candidate) => candidate.artifactId === binding?.modelArtifactId,
  );
  const groups = runtimeParameterGroups(exposedParameters, configuration);
  const parametersForm = (parameters: readonly EngineParameterDefinition[]) => (
    <EngineParametersForm
      errors={fieldErrors}
      inputRefs={engineParameterRefs}
      inputs={inputs}
      onBlur={onParameterBlur}
      onChange={onParameterChange}
      parameters={parameters}
    />
  );

  return (
    <DeploymentForm className="gap-4">
      <DeploymentStepHeading>Runtime</DeploymentStepHeading>
      {exposedParameters.length ? null : (
        <Notice role="alert" tone="destructive">
          The selected engine release schema has no exposed parameters.
        </Notice>
      )}
      {groups.parallelRequests ? parametersForm([groups.parallelRequests]) : null}
      {groups.hostMemory.length ? (
        <DeploymentFormSection
          className="min-w-0 rounded-lg border border-border bg-card p-4"
          title="Host memory"
        >
          {hostKvOffloadSharesMemory ? null : (
            <p className="text-xs text-muted-foreground">
              Reserved in host memory. Accelerator memory is unchanged.
            </p>
          )}
          {parametersForm(groups.hostMemory)}
        </DeploymentFormSection>
      ) : null}
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
      {groups.other.length ? (
        <RuntimeSection
          count={groups.other.length}
          errors={fieldErrors}
          fields={groups.other.map((parameter) => parameter.parameterId)}
          revealedErrors={revealedErrors}
          title="Other parameters"
        >
          {parametersForm(groups.other)}
        </RuntimeSection>
      ) : null}
      <RuntimeSection
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
      </RuntimeSection>
      <RuntimeSection
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
      </RuntimeSection>
    </DeploymentForm>
  );
}

interface RuntimeSectionProps {
  children: ReactNode;
  /** The number of fields inside, shown after the title. */
  count?: number;
  errors: FieldErrors;
  /** The error keys of the fields inside. */
  fields?: readonly string[];
  revealedErrors: FieldErrors;
  title: string;
}

/**
 * The model artifact choices of a configuration, each named by the profile that launches it by
 * default, since the Runtime step shows the artifact's quantization beside the choice. An artifact
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

/**
 * A Runtime section, collapsed unless one of its fields is invalid when it mounts. A new blocked
 * navigation that flags one of its fields opens it, so the field renders before the dialog moves
 * focus to it; the operator can collapse it again.
 */
function RuntimeSection({
  children,
  count,
  errors,
  fields = [],
  revealedErrors,
  title,
}: RuntimeSectionProps) {
  const [open, setOpen] = useState(() => fields.some((field) => errors.has(field)));
  const [shownReveal, setShownReveal] = useState(revealedErrors);

  if (revealedErrors !== shownReveal) {
    setShownReveal(revealedErrors);
    if (fields.some((field) => revealedErrors.has(field))) setOpen(true);
  }

  return (
    <Collapsible.Root
      className="min-w-0 rounded-lg border border-border bg-card"
      onOpenChange={setOpen}
      open={open}
    >
      <Collapsible.Trigger className="group/section flex w-full items-center gap-2 rounded-lg px-4 py-3 text-left text-sm font-semibold outline-none focus-visible:ring-3 focus-visible:ring-ring/50">
        <ChevronRightIcon
          aria-hidden="true"
          className="size-4 shrink-0 text-muted-foreground transition-transform group-data-panel-open/section:rotate-90 motion-reduce:transition-none"
        />
        <span>{title}</span>
        {count === undefined ? null : (
          <>
            {" "}
            <span className="font-normal text-muted-foreground">({count})</span>
          </>
        )}
      </Collapsible.Trigger>
      <Collapsible.Panel className="grid min-w-0 gap-3 px-4 pb-4">{children}</Collapsible.Panel>
    </Collapsible.Root>
  );
}
