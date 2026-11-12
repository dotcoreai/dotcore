import type { RefObject } from "react";

import { Detail } from "@/components/ui/detail-list";
import { Field } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import { SecretValue, ValueTag } from "@/components/ui/secret-value";
import { Switch } from "@/components/ui/switch";

import {
  parameterDisplayValue,
  type ParameterView,
  type ParameterViewValue,
} from "./parameter-view";

interface ParametersFormProps {
  errors: ReadonlyMap<string, string>;
  /** Prefix of each control's element ID, so several forms can share a page. */
  idPrefix: string;
  inputs: ReadonlyMap<string, string>;
  inputRefs: RefObject<Map<string, HTMLElement | null>>;
  onBlur: (parameterId: string) => void;
  onChange: (parameterId: string, input: string) => void;
  parameters: readonly ParameterView[];
}

/**
 * Edits typed parameters as text inputs keyed by parameter ID: a switch holds `true` or `false`,
 * a number field the typed digits, and a select the chosen value.
 */
export function ParametersForm({
  errors,
  idPrefix,
  inputs,
  inputRefs,
  onBlur,
  onChange,
  parameters,
}: ParametersFormProps) {
  return parameters.map((parameter) => {
    const controlId = `${idPrefix}-${parameter.id}`;
    const unit = parameter.type.kind === "unsignedInteger" ? parameter.type.unit : "";
    const field = {
      controlId,
      error: errors.get(parameter.id),
      help: parameter.description || undefined,
      label: parameter.label,
    };
    const register = (control: HTMLElement | null) => {
      inputRefs.current.set(parameter.id, control);
    };

    if (parameter.type.kind === "boolean") {
      return (
        <Field {...field} key={parameter.id} layout="inline">
          {(control) => (
            <Switch
              {...control}
              checked={inputs.get(parameter.id) === "true"}
              onBlur={() => onBlur(parameter.id)}
              onCheckedChange={(checked) => onChange(parameter.id, String(checked))}
              ref={register}
            />
          )}
        </Field>
      );
    }

    if (parameter.type.kind === "stringEnum") {
      const { values } = parameter.type;
      return (
        <Field {...field} key={parameter.id}>
          {(control) => (
            <NativeSelect
              {...control}
              onBlur={() => onBlur(parameter.id)}
              onChange={(event) => onChange(parameter.id, event.target.value)}
              ref={register}
              value={inputs.get(parameter.id) ?? ""}
            >
              {values.map((value) => (
                <NativeSelectOption key={value} value={value}>
                  {value}
                </NativeSelectOption>
              ))}
            </NativeSelect>
          )}
        </Field>
      );
    }

    return (
      <Field
        {...field}
        facts={
          parameter.type.kind === "unsignedInteger" ? (
            <RangeFact type={parameter.type} />
          ) : parameter.type.kind === "text" || parameter.type.kind === "secret" ? (
            <LengthFact type={parameter.type} />
          ) : null
        }
        key={parameter.id}
      >
        {(control) => {
          const input = (
            <Input
              {...control}
              autoComplete="off"
              inputMode={parameter.type.kind === "unsignedInteger" ? "numeric" : undefined}
              placeholder={parameter.type.kind === "secret" ? "Generated at deploy" : undefined}
              spellCheck={false}
              onBlur={() => onBlur(parameter.id)}
              onChange={(event) => onChange(parameter.id, event.target.value)}
              ref={register}
              value={inputs.get(parameter.id) ?? ""}
            />
          );
          return unit ? (
            <div className="flex items-center gap-2">
              {input}
              <span className="shrink-0 text-sm text-muted-foreground">{unit}</span>
            </div>
          ) : (
            input
          );
        }}
      </Field>
    );
  });
}

/** The accepted length of a text or secret parameter. */
function LengthFact({ type }: { type: { maximumLength?: number; minimumLength: number } }) {
  return (
    <p>
      {type.maximumLength === undefined
        ? `At least ${type.minimumLength} characters`
        : `${type.minimumLength}–${type.maximumLength} characters`}
    </p>
  );
}

/** The accepted range of a number parameter, with its unit when it has one. */
function RangeFact({ type }: { type: { maximum: bigint; minimum: bigint; unit: string } }) {
  return (
    <p>
      Range {`${type.minimum}–${type.maximum}`}
      {type.unit ? ` ${type.unit}` : ""}
    </p>
  );
}

/**
 * Detail rows for retained parameter values, in view order; a missing or mistyped value is skipped.
 * A value equal to its default is tagged `default`, a secret is masked until revealed, and the IDs
 * in `generatedIds` are tagged `generated`.
 */
export function ParametersDetails({
  defaults,
  generatedIds,
  parameters,
  values,
}: {
  defaults?: ReadonlyMap<string, ParameterViewValue | undefined>;
  generatedIds?: ReadonlySet<string>;
  parameters: readonly ParameterView[];
  values: ReadonlyMap<string, ParameterViewValue>;
}) {
  return parameters.map((parameter) => {
    const value = values.get(parameter.id);
    const displayValue = parameterDisplayValue(parameter, value);
    if (displayValue === undefined) return null;

    const isDefault = value !== undefined && defaults?.get(parameter.id) === value;
    const tag = isDefault ? <ValueTag>default</ValueTag> : null;
    if (parameter.type.kind === "secret") {
      return (
        <Detail
          key={parameter.id}
          label={parameter.label}
          value={
            <SecretValue
              label={parameter.label}
              tags={generatedIds?.has(parameter.id) ? <ValueTag>generated</ValueTag> : null}
              value={displayValue}
            />
          }
        />
      );
    }
    return (
      <Detail
        key={parameter.id}
        label={parameter.label}
        value={
          <span className="inline-flex flex-wrap items-center justify-end gap-1.5">
            {tag}
            {parameter.type.kind === "text" && displayValue === "" ? (
              <span className="text-muted-foreground">Empty</span>
            ) : (
              displayValue
            )}
          </span>
        }
      />
    );
  });
}
