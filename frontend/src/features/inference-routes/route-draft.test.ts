import { describe, expect, it } from "vitest";

import {
  MAX_GROUP_MEMBERS,
  MAX_ROUTE_GROUPS,
  type RouteEditorState,
  validateRouteEditor,
} from "./route-draft";

function editor(name: string, groups: string[][]): RouteEditorState {
  return {
    groups: groups.map((deploymentIds, index) => ({ deploymentIds, key: `group-${index}` })),
    name,
  };
}

const label = (id: string) => `Label ${id}`;

describe("route editor validation", () => {
  it("accepts a valid draft", () => {
    expect(validateRouteEditor(editor(" chat-2 ", [["a", "b"], ["c"]]), new Set(), label)).toEqual(
      new Map(),
    );
  });

  it("accepts a name with periods", () => {
    expect(validateRouteEditor(editor("chat.2", [["a"]]), new Set(), label)).toEqual(new Map());
  });

  it.each([
    ["", "Route name is required."],
    [
      "Chat",
      "Use lowercase letters, digits, dashes, and periods, starting with a letter, up to 63 characters.",
    ],
    [
      "chat route",
      "Use lowercase letters, digits, dashes, and periods, starting with a letter, up to 63 characters.",
    ],
    [
      "-chat",
      "Use lowercase letters, digits, dashes, and periods, starting with a letter, up to 63 characters.",
    ],
    [
      `a${"b".repeat(63)}`,
      "Use lowercase letters, digits, dashes, and periods, starting with a letter, up to 63 characters.",
    ],
    [
      "chat_2",
      "Use lowercase letters, digits, dashes, and periods, starting with a letter, up to 63 characters.",
    ],
    ["taken", "A route named taken already exists."],
  ])("rejects the name %j", (name, message) => {
    expect(validateRouteEditor(editor(name, [["a"]]), new Set(["taken"]), label)).toEqual(
      new Map([["name", message]]),
    );
  });

  it("accepts the longest valid name", () => {
    expect(validateRouteEditor(editor(`a${"b".repeat(62)}`, [["a"]]), new Set(), label).size).toBe(
      0,
    );
  });

  it("rejects no groups and more than eight groups", () => {
    expect(validateRouteEditor(editor("chat", []), new Set(), label)).toEqual(
      new Map([["groups", "Routes have one to 8 groups."]]),
    );
    const nine = Array.from({ length: MAX_ROUTE_GROUPS + 1 }, (_, index) => [`d${index}`]);
    expect(validateRouteEditor(editor("chat", nine), new Set(), label).get("groups")).toBe(
      "Routes have one to 8 groups.",
    );
  });

  it("rejects an empty group and a group over 32 members, keyed by group", () => {
    const tooMany = Array.from({ length: MAX_GROUP_MEMBERS + 1 }, (_, index) => `m${index}`);
    expect(validateRouteEditor(editor("chat", [["a"], [], tooMany]), new Set(), label)).toEqual(
      new Map([
        ["group-1-deployment", "Choose one to 32 deployments."],
        ["group-2-deployment", "Choose one to 32 deployments."],
      ]),
    );
  });

  it("rejects a deployment repeated in a later group on that later group", () => {
    expect(
      validateRouteEditor(
        editor("chat", [
          ["a", "b"],
          ["c", "b"],
        ]),
        new Set(),
        label,
      ),
    ).toEqual(new Map([["group-1-deployment", "Label b is already in another group."]]));
  });

  it("orders errors as the form lays out its fields", () => {
    expect([
      ...validateRouteEditor(editor("", [[], ["a"], ["a"]]), new Set(), label).keys(),
    ]).toEqual(["name", "group-0-deployment", "group-2-deployment"]);
  });
});
