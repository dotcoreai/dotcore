import { Fragment, type ReactNode, type RefObject } from "react";

import { CountStepper } from "@/components/ui/count-stepper";
import { Detail } from "@/components/ui/detail-list";
import { Field } from "@/components/ui/field";
import { FormGroup, SwitchedFormGroup } from "@/components/ui/form-group";
import { Input } from "@/components/ui/input";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import { SecretValue } from "@/components/ui/secret-value";
import { Switch } from "@/components/ui/switch";
import { parameterLayout, type ParameterLayoutField } from "@/domain/parameter-layout";
import {
  integerBounds,
  lengthBounds,
  parameterDisplayValue,
  type ParameterGroupView,
  parameterInputValues,
  type ParameterView,
  type ParameterViewType,
  type ParameterViewValue,
} from "@/domain/parameter-view";
import type { FieldErrors } from "@/lib/form-validation";
import { isSizeUnit } from "@/lib/format";

interface ParametersFormProps {
  /** Errors keyed by parameter ID. */
  errors: FieldErrors;
  /** The owner's parameter groups; a parameter naming none of them is ungrouped. */
  groups: readonly ParameterGroupView[];
  /** Prefix of each control's element ID, so several forms can share a page. */
  idPrefix: string;
  inputs: ReadonlyMap<string, string>;
  inputRefs: RefObject<Map<string, HTMLElement | null>>;
  onBlur: (parameterId: string) => void;
  onChange: (parameterId: string, input: string, settled?: boolean) => void;
  /** The step's parameters, in parameter order. */
  parameters: readonly ParameterView[];
  /**
   * The owner's own field for a parameter, used in place of the typed one when it returns one, such
   * as a richer control for some numbers. The field keeps its place in the layout.
   */
  renderField?: (parameter: ParameterView) => ReactNode;
}

/**
 * Edits typed parameters as text inputs keyed by parameter ID: a switch holds `true` or `false`,
 * a number field the typed digits, and a select the chosen value. Fields are laid out by their
 * groups and conditions: a group's members sit under its heading, a switched group shows its other members
 * only while on, and a dependent shows indented under its switch only while that switch is on.
 */
export function ParametersForm(props: ParametersFormProps) {
  const { groups, inputs, parameters } = props;
  const items = parameterLayout(parameters, groups, parameterInputValues(parameters, inputs));
  // Dependents follow as a sibling, so the switch keeps its element, and focus, as they appear.
  const fieldWithDependents = ({ dependents, parameter }: ParameterLayoutField) => (
    <Fragment key={parameter.id}>
      <ParameterField parameter={parameter} props={props} />
      {dependents.length ? (
        <div className="ml-1.5 grid min-w-0 gap-3 border-l-2 border-border pl-3">
          {dependents.map((dependent) => (
            <ParameterField key={dependent.id} parameter={dependent} props={props} />
          ))}
        </div>
      ) : null}
    </Fragment>
  );

  return items.map((item) => {
    if (item.kind === "field") return fieldWithDependents(item);

    const { group, members, switchParameter } = item;
    if (switchParameter)
      return (
        <SwitchedFormGroup
          header={<ParameterField parameter={switchParameter} props={props} />}
          key={`group:${group.id}`}
          label={switchParameter.label}
        >
          {members.length ? members.map(fieldWithDependents) : null}
        </SwitchedFormGroup>
      );

    return (
      <FormGroup help={group.description} key={`group:${group.id}`} title={group.label}>
        {members.map(fieldWithDependents)}
      </FormGroup>
    );
  });
}

/** The accepted range of a number, open-ended without a maximum, with its unit when it has one. */
export function RangeFact({ maximum, minimum, unit }: RangeBounds) {
  return (
    <p>
      Range {integerBounds({ maximum, minimum })}
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

// One parameter's field, its control chosen by the parameter's type.
function ParameterField({
  parameter,
  props: { errors, idPrefix, inputRefs, inputs, onBlur, onChange, renderField },
}: {
  parameter: ParameterView;
  props: ParametersFormProps;
}) {
  const ownField = renderField?.(parameter);
  if (ownField) return ownField;

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
      <Field {...field} layout="inline">
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
      <Field {...field}>
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
    <Field {...field} facts={parameterFacts(parameter.type)}>
      {(control) => {
        const input = isCount(parameter.type) ? (
          <CountStepper
            {...control}
            inputRef={register}
            label={parameter.label}
            maximum={
              parameter.type.maximum === undefined ? undefined : Number(parameter.type.maximum)
            }
            minimum={Number(parameter.type.minimum)}
            onBlur={() => onBlur(parameter.id)}
            onChange={(input, change) => onChange(parameter.id, input, change?.settled)}
            value={inputs.get(parameter.id) ?? ""}
          />
        ) : (
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
}

// Whether a type is a small count, which the stepper edits; byte sizes and tokens keep text input.
function isCount(
  type: ParameterViewType,
): type is Extract<ParameterViewType, { kind: "unsignedInteger" }> {
  return type.kind === "unsignedInteger" && !type.memoryPool && !isSizeUnit(type.unit);
}

// The facts under a typed input field: a number's range, or a text's accepted length.
function parameterFacts(type: ParameterViewType) {
  if (type.kind === "unsignedInteger")
    return <RangeFact maximum={type.maximum} minimum={type.minimum} unit={type.unit} />;
  if (type.kind === "text" || type.kind === "secret") return <LengthFact type={type} />;
  return null;
}

// The input attributes that differ by type: a numeric keyboard for numbers, an email keyboard for
// emails, and a hint that an empty secret is generated.
function textInputProps(type: ParameterViewType) {
  if (type.kind === "unsignedInteger") return { inputMode: "numeric" as const };
  if (type.kind === "text" && type.format === "email") return { inputMode: "email" as const };
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
