import type { RefObject } from "react";

import { Detail } from "@/components/ui/detail-list";
import { Field } from "@/components/ui/field";
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
    const controlId = `${idPrefix}-${parameter.id}`;
    const unitId = `${controlId}-unit`;
    const unit = parameter.type.kind === "unsignedInteger" ? parameter.type.unit : "";
    const field = {
      controlId,
      error: errors.get(parameter.id),
      help: parameter.hint,
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
      <Field {...field} describedBy={unit ? unitId : undefined} key={parameter.id}>
        {(control) => {
          const input = (
            <Input
              {...control}
              inputMode="numeric"
              onBlur={() => onBlur(parameter.id)}
              onChange={(event) => onChange(parameter.id, event.target.value)}
              ref={register}
              value={inputs.get(parameter.id) ?? ""}
            />
          );
          return unit ? (
            <div className="flex items-center gap-2">
              {input}
              <span className="shrink-0 text-sm text-muted-foreground" id={unitId}>
                {unit}
              </span>
            </div>
          ) : (
            input
          );
        }}
      </Field>
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
