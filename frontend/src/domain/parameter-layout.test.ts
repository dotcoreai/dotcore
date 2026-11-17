import { describe, expect, it } from "vitest";

import { enabledParameters, parameterLayout } from "./parameter-layout";
import type { ParameterGroupView, ParameterView } from "./parameter-view";

describe("parameterLayout", () => {
  it("keeps ungrouped fields at the top level in parameter order", () => {
    const parameters = [text("name"), text("theme")];

    expect(parameterLayout(parameters, [], new Map())).toEqual([
      { dependents: [], kind: "field", parameter: parameters[0] },
      { dependents: [], kind: "field", parameter: parameters[1] },
    ]);
  });

  it("boxes a plain group's members between the ungrouped fields around it", () => {
    const parameters = [
      text("parallel"),
      text("gpu", { groupId: "expert" }),
      text("host", { groupId: "expert" }),
      text("cache"),
    ];

    expect(parameterLayout(parameters, [expertMemory], new Map())).toEqual([
      { dependents: [], kind: "field", parameter: parameters[0] },
      {
        group: expertMemory,
        kind: "group",
        members: [
          { dependents: [], kind: "field", parameter: parameters[1] },
          { dependents: [], kind: "field", parameter: parameters[2] },
        ],
      },
      { dependents: [], kind: "field", parameter: parameters[3] },
    ]);
  });

  it("shows a mixed-visibility group in each step with that step's members only", () => {
    const basic = text("gpu", { groupId: "expert" });
    const advanced = text("host", { groupId: "expert" });

    expect(parameterLayout([basic], [expertMemory], new Map())).toEqual([
      {
        group: expertMemory,
        kind: "group",
        members: [{ dependents: [], kind: "field", parameter: basic }],
      },
    ]);
    expect(parameterLayout([advanced], [expertMemory], new Map())).toEqual([
      {
        group: expertMemory,
        kind: "group",
        members: [{ dependents: [], kind: "field", parameter: advanced }],
      },
    ]);
  });

  it("shows a switched group's other members only while its switch is on", () => {
    const parameters = [
      boolean("require-login", { groupId: "login" }),
      text("email", { groupId: "login" }),
      text("password", { groupId: "login" }),
    ];

    expect(parameterLayout(parameters, [login], new Map([["require-login", false]]))).toEqual([
      { group: login, kind: "group", members: [], switchParameter: parameters[0] },
    ]);
    expect(parameterLayout(parameters, [login], new Map([["require-login", true]]))).toEqual([
      {
        group: login,
        kind: "group",
        members: [
          { dependents: [], kind: "field", parameter: parameters[1] },
          { dependents: [], kind: "field", parameter: parameters[2] },
        ],
        switchParameter: parameters[0],
      },
    ]);
  });

  it("puts enabled dependents under their switch and leaves them out while it is off", () => {
    const parameters = [
      boolean("reports"),
      text("report-url", { enabledById: "reports" }),
      text("report-hour", { enabledById: "reports" }),
      text("theme"),
    ];

    expect(parameterLayout(parameters, [], new Map([["reports", true]]))).toEqual([
      { dependents: [parameters[1], parameters[2]], kind: "field", parameter: parameters[0] },
      { dependents: [], kind: "field", parameter: parameters[3] },
    ]);
    expect(parameterLayout(parameters, [], new Map([["reports", false]]))).toEqual([
      { dependents: [], kind: "field", parameter: parameters[0] },
      { dependents: [], kind: "field", parameter: parameters[3] },
    ]);
  });

  it("gives no box to a group whose members are all left out", () => {
    const parameters = [text("cache"), text("email", { groupId: "login" })];

    expect(parameterLayout(parameters, [login], new Map())).toEqual([
      { dependents: [], kind: "field", parameter: parameters[0] },
    ]);
  });
});

describe("enabledParameters", () => {
  it("disables a dependent whose enabler view is missing", () => {
    const parameters = [text("report-url", { enabledById: "reports" }), text("theme")];

    expect(enabledParameters(parameters, [], new Map([["reports", true]]))).toEqual([
      parameters[1],
    ]);
  });

  it("enables a switched group's members only while the switch's value is true", () => {
    const parameters = [
      boolean("require-login", { groupId: "login" }),
      text("email", { groupId: "login" }),
    ];

    expect(enabledParameters(parameters, [login], new Map())).toEqual([parameters[0]]);
    expect(enabledParameters(parameters, [login], new Map([["require-login", true]]))).toEqual(
      parameters,
    );
  });
});

const expertMemory: ParameterGroupView = {
  description: "Where expert weights live.",
  id: "expert",
  label: "Expert memory",
};

const login: ParameterGroupView = { id: "login", label: "", switchId: "require-login" };

function text(id: string, fields: Partial<ParameterView> = {}): ParameterView {
  return { id, label: id, type: { kind: "text", minimumLength: 0 }, ...fields };
}

function boolean(id: string, fields: Partial<ParameterView> = {}): ParameterView {
  return { id, label: id, type: { kind: "boolean" }, ...fields };
}
