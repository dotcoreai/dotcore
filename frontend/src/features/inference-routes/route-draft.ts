import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import { type FieldErrors, toFieldErrors } from "@/lib/form-validation";

/**
 * One fallback group being edited. `key` is a stable draft-only identity, so reordering moves a
 * group's controls with it instead of swapping their contents. Its selection is derived from the
 * member count on save.
 */
export type RouteEditorGroup = { key: string; deploymentIds: string[] };
export type RouteEditorState = {
  route?: InferenceRouteView;
  name: string;
  groups: RouteEditorGroup[];
};

/** Route names: a lowercase letter, then up to 62 lowercase letters, digits, dashes, or periods. */
const ROUTE_NAME_PATTERN = /^[a-z][a-z0-9.-]{0,62}$/;
export const MAX_ROUTE_GROUPS = 8;
export const MAX_GROUP_MEMBERS = 32;

/**
 * Checks a route draft against the route contract before it is saved: name format and uniqueness
 * against the loaded routes, group count, members per group, and no deployment in two groups.
 * Errors are keyed by field (`name`, `groups`, `group-<index>-deployment`) in form order, so the
 * form can show each next to its field and focus the first.
 */
export function validateRouteEditor(
  editor: RouteEditorState,
  takenNames: ReadonlySet<string>,
  deploymentLabel: (id: string) => string,
): FieldErrors {
  const name = editor.name.trim();
  const entries: Array<readonly [string, string | undefined]> = [
    ["name", nameError(name, takenNames)],
  ];

  if (editor.groups.length < 1 || editor.groups.length > MAX_ROUTE_GROUPS) {
    entries.push(["groups", `Routes have one to ${MAX_ROUTE_GROUPS} groups.`]);
  }

  const seen = new Set<string>();
  for (const [index, group] of editor.groups.entries()) {
    const repeated = group.deploymentIds.find((id) => seen.has(id));
    group.deploymentIds.forEach((id) => seen.add(id));
    entries.push([
      `group-${index}-deployment`,
      group.deploymentIds.length < 1 || group.deploymentIds.length > MAX_GROUP_MEMBERS
        ? `Choose one to ${MAX_GROUP_MEMBERS} deployments.`
        : repeated
          ? `${deploymentLabel(repeated)} is already in another group.`
          : undefined,
    ]);
  }

  return toFieldErrors(entries);
}

/** A new, empty group with a fresh draft identity. */
export function newEditorGroup(deploymentIds: string[] = []): RouteEditorGroup {
  nextGroupKey += 1;
  return { key: `group-${nextGroupKey}`, deploymentIds };
}

function nameError(name: string, takenNames: ReadonlySet<string>): string | undefined {
  if (!name) return "Route name is required.";
  if (!ROUTE_NAME_PATTERN.test(name)) {
    return "Use lowercase letters, digits, dashes, and periods, starting with a letter, up to 63 characters.";
  }
  if (takenNames.has(name)) return `A route named ${name} already exists.`;
  return undefined;
}

let nextGroupKey = 0;
