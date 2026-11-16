import type { RefObject } from "react";

import { Detail } from "@/components/ui/detail-list";
import { Field } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import { SecretValue } from "@/components/ui/secret-value";
import { Switch } from "@/components/ui/switch";
import {
  lengthBounds,
  parameterDisplayValue,
  type ParameterView,
  type ParameterViewType,
  type ParameterViewValue,
} from "@/domain/parameter-view";

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

    const inputProps = textInputProps(parameter.type);
    return (
      <Field {...field} facts={parameterFacts(parameter.type)} key={parameter.id}>
        {(control) => {
          const input = (
            <Input
              {...control}
              {...inputProps}
              autoComplete="off"
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

/** The accepted range of a number, with its unit when it has one. */
export function RangeFact({ maximum, minimum, unit }: RangeBounds) {
  return (
    <p>
      Range {`${minimum}–${maximum}`}
      {unit ? ` ${unit}` : ""}
    </p>
  );
}

/**
 * Detail rows for retained parameter values, in view order; a missing or mistyped value is skipped.
 * A secret is masked until revealed and empty text reads as Empty.
 */
export function ParametersDetails({
  parameters,
  values,
}: {
  parameters: readonly ParameterView[];
  values: ReadonlyMap<string, ParameterViewValue>;
}) {
  return parameters.map((parameter) => {
    const value = values.get(parameter.id);
    const displayValue = parameterDisplayValue(parameter, value);
    if (displayValue === undefined) return null;

    if (parameter.type.kind === "secret") {
      return (
        <Detail
          key={parameter.id}
          label={parameter.label}
          value={<SecretValue label={parameter.label} value={displayValue} />}
        />
      );
    }
    return (
      <Detail
        key={parameter.id}
        label={parameter.label}
        value={
          parameter.type.kind === "text" && displayValue === "" ? (
            <span className="text-muted-foreground">Empty</span>
          ) : (
            displayValue
          )
        }
      />
    );
  });
}

type RangeBounds = Omit<Extract<ParameterViewType, { kind: "unsignedInteger" }>, "kind">;
type TextType = Extract<ParameterViewType, { kind: "secret" | "text" }>;

// The facts under a typed input field: a number's range, or a text's accepted length.
function parameterFacts(type: ParameterViewType) {
  if (type.kind === "unsignedInteger")
    return <RangeFact maximum={type.maximum} minimum={type.minimum} unit={type.unit} />;
  if (type.kind === "text" || type.kind === "secret") return <LengthFact type={type} />;
  return null;
}

// The input attributes that differ by type: a numeric keyboard for numbers, and a hint that an
// empty secret is generated.
function textInputProps(type: ParameterViewType) {
  if (type.kind === "unsignedInteger") return { inputMode: "numeric" as const };
  if (type.kind === "secret")
    return {
      className: "placeholder:text-muted-foreground placeholder:italic",
      placeholder: "Generated at deploy",
    };
  return {};
}

/** The accepted length of a text or secret parameter, when it has bounds. */
function LengthFact({ type }: { type: TextType }) {
  const bounds = lengthBounds(type);
  if (!bounds) return null;
  return <p>{bounds.charAt(0).toUpperCase() + bounds.slice(1)}</p>;
}
