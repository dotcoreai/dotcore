import type { RefObject } from "react";

import { Detail } from "@/components/ui/detail-list";
import { FieldError } from "@/components/ui/field-error";
import { Input } from "@/components/ui/input";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
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
    const error = errors.get(parameter.id);
    const controlId = `${idPrefix}-${parameter.id}`;
    const hintId = `${controlId}-description`;
    const errorId = `${controlId}-error`;
    const unitId = `${controlId}-unit`;
    const unit = parameter.type.kind === "unsignedInteger" ? parameter.type.unit : "";
    const describedBy =
      [parameter.hint ? hintId : "", unit ? unitId : "", error ? errorId : ""]
        .filter(Boolean)
        .join(" ") || undefined;
    const register = (control: HTMLElement | null) => {
      inputRefs.current.set(parameter.id, control);
    };
    const hint = parameter.hint ? (
      <span className="text-xs text-muted-foreground" id={hintId}>
        {parameter.hint}
      </span>
    ) : null;
    const fieldError = error ? <FieldError id={errorId}>{error}</FieldError> : null;

    if (parameter.type.kind === "boolean") {
      return (
        <div className="grid gap-1" key={parameter.id}>
          <div className="flex items-center justify-between gap-3">
            <label htmlFor={controlId}>{parameter.label}</label>
            <Switch
              aria-describedby={describedBy}
              aria-invalid={error ? "true" : undefined}
              checked={inputs.get(parameter.id) === "true"}
              id={controlId}
              onBlur={() => onBlur(parameter.id)}
              onCheckedChange={(checked) => onChange(parameter.id, String(checked))}
              ref={register}
            />
          </div>
          {hint}
          {fieldError}
        </div>
      );
    }

    if (parameter.type.kind === "stringEnum") {
      return (
        <div className="grid gap-1" key={parameter.id}>
          <label htmlFor={controlId}>{parameter.label}</label>
          <NativeSelect
            aria-describedby={describedBy}
            aria-invalid={error ? "true" : undefined}
            id={controlId}
            onBlur={() => onBlur(parameter.id)}
            onChange={(event) => onChange(parameter.id, event.target.value)}
            ref={register}
            value={inputs.get(parameter.id) ?? ""}
          >
            {parameter.type.values.map((value) => (
              <NativeSelectOption key={value} value={value}>
                {value}
              </NativeSelectOption>
            ))}
          </NativeSelect>
          {hint}
          {fieldError}
        </div>
      );
    }

    const input = (
      <Input
        aria-describedby={describedBy}
        aria-invalid={error ? "true" : undefined}
        id={controlId}
        inputMode="numeric"
        onBlur={() => onBlur(parameter.id)}
        onChange={(event) => onChange(parameter.id, event.target.value)}
        ref={register}
        value={inputs.get(parameter.id) ?? ""}
      />
    );
    return (
      <div className="grid gap-1" key={parameter.id}>
        <label htmlFor={controlId}>{parameter.label}</label>
        {unit ? (
          <div className="flex items-center gap-2">
            {input}
            <span className="shrink-0 text-sm text-muted-foreground" id={unitId}>
              {unit}
            </span>
          </div>
        ) : (
          input
        )}
        {hint}
        {fieldError}
      </div>
    );
  });
}

/** Detail rows for retained parameter values, in view order; a missing or mistyped value is skipped. */
export function ParametersDetails({
  parameters,
  values,
}: {
  parameters: readonly ParameterView[];
  values: ReadonlyMap<string, ParameterViewValue>;
}) {
  return parameters.map((parameter) => {
    const displayValue = parameterDisplayValue(parameter, values.get(parameter.id));
    return displayValue === undefined ? null : (
      <Detail key={parameter.id} label={parameter.label} value={displayValue} />
    );
  });
}
