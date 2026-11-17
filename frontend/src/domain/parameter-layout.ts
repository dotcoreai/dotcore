import type { ParameterGroupView, ParameterView, ParameterViewValue } from "./parameter-view";

/** One entry of a step's parameter layout: a field at the step's top level, or a group box. */
export type ParameterLayoutItem = ParameterLayoutField | ParameterLayoutGroup;

/** A field and the enabled parameters it switches on, shown indented under it. */
export interface ParameterLayoutField {
  dependents: readonly ParameterView[];
  kind: "field";
  parameter: ParameterView;
}

/**
 * A group's members shown in one step. A switched group carries its switch, which titles the box,
 * and lists its other members only while the switch is on.
 */
export interface ParameterLayoutGroup {
  group: ParameterGroupView;
  kind: "group";
  members: readonly ParameterLayoutField[];
  switchParameter?: ParameterView;
}

/**
 * Lays out one step's parameters in parameter order: ungrouped fields at the top level, each
 * group's members in its box, and each enabled dependent under the field that enables it. Disabled
 * parameters are left out, and a group with nothing to show in this step gets no box.
 */
export function parameterLayout(
  parameters: readonly ParameterView[],
  groups: readonly ParameterGroupView[],
  values: ReadonlyMap<string, ParameterViewValue>,
): ParameterLayoutItem[] {
  const groupsById = new Map(groups.map((group) => [group.id, group]));
  const items: (BuiltField | BuiltGroup)[] = [];
  const fields = new Map<string, BuiltField>();

  for (const parameter of enabledParameters(parameters, groups, values)) {
    // An enabled dependent's enabler is shown, since it has no enabler of its own.
    if (parameter.enabledById) {
      fields.get(parameter.enabledById)?.dependents.push(parameter);
      continue;
    }

    const group = parameter.groupId ? groupsById.get(parameter.groupId) : undefined;
    if (group?.switchId === parameter.id) {
      items.push({ group, kind: "group", members: [], switchParameter: parameter });
      continue;
    }

    const field: BuiltField = { dependents: [], kind: "field", parameter };
    fields.set(parameter.id, field);
    const last = items.at(-1);
    if (!group) items.push(field);
    else if (last?.kind === "group" && last.group.id === group.id) last.members.push(field);
    else items.push({ group, kind: "group", members: [field] });
  }
  return items;
}

/**
 * The parameters that are enabled, in order. A parameter is enabled when nothing enables it, or
 * when its enabler is among `parameters` and its value is on; a missing enabler disables it.
 */
export function enabledParameters<T extends ParameterView>(
  parameters: readonly T[],
  groups: readonly ParameterGroupView[],
  values: ReadonlyMap<string, ParameterViewValue>,
): T[] {
  const ids = new Set(parameters.map((parameter) => parameter.id));
  return parameters.filter((parameter) => {
    const enabler = parameterEnabler(parameter, groups);
    return enabler === undefined || (ids.has(enabler) && values.get(enabler) === true);
  });
}

/**
 * The boolean parameter that enables `parameter`: its own enabler, else its switched group's switch
 * for every member but the switch itself. Undefined when nothing does.
 */
export function parameterEnabler(
  parameter: ParameterView,
  groups: readonly ParameterGroupView[],
): string | undefined {
  if (parameter.enabledById) return parameter.enabledById;

  const switchId = groups.find((group) => group.id === parameter.groupId)?.switchId;
  return switchId === parameter.id ? undefined : switchId;
}

// The layout's entries while they are built; members and dependents are appended in order.
interface BuiltField extends ParameterLayoutField {
  dependents: ParameterView[];
}

interface BuiltGroup extends ParameterLayoutGroup {
  members: BuiltField[];
}
