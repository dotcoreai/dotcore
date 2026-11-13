import { create } from "@bufbuild/protobuf";
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { ParameterVisibility } from "@/gen/dotcore/catalog/v1/parameter_pb";
import { ResolvedLocalModelDeploymentSnapshotSchema } from "@/gen/dotcore/model/v1/deployment_pb";

import { DeploymentParametersSection } from "./engine-parameters";

const uint = (parameterId: string, displayName: string, visibility: ParameterVisibility) => ({
  parameterId,
  displayName,
  visibility,
  parameterType: {
    case: "unsignedInteger" as const,
    value: { minimum: 1n, maximum: 1000000n, unit: "" },
  },
});

const snapshot = create(ResolvedLocalModelDeploymentSnapshotSchema, {
  engineParameterSchema: {
    schemaId: "schema",
    parameters: [
      uint("slots", "Slots", ParameterVisibility.BASIC),
      uint("cap", "Expert cap", ParameterVisibility.ADVANCED),
      uint("engine-hidden", "Engine hidden", ParameterVisibility.HIDDEN),
      uint("profile-hidden", "Profile hidden", ParameterVisibility.ADVANCED),
      uint("profile-shown", "Profile shown", ParameterVisibility.HIDDEN),
      {
        parameterId: "key",
        displayName: "API key",
        visibility: ParameterVisibility.ADVANCED,
        parameterType: { case: "secret", value: { minimumLength: 12 } },
      },
      uint("unset", "Unset", ParameterVisibility.BASIC),
    ],
  },
  engineParameterValues: {
    values: [
      { parameterId: "slots", value: { case: "unsignedInteger", value: 4n } },
      { parameterId: "cap", value: { case: "unsignedInteger", value: 900n } },
      { parameterId: "engine-hidden", value: { case: "unsignedInteger", value: 1n } },
      { parameterId: "profile-hidden", value: { case: "unsignedInteger", value: 2n } },
      { parameterId: "profile-shown", value: { case: "unsignedInteger", value: 5n } },
      { parameterId: "key", value: { case: "string", value: "s3cretvalue-abc" } },
    ],
  },
  profileEngineParameterDefaults: {
    values: [
      { parameterId: "slots", value: { case: "unsignedInteger", value: 4n } },
      { parameterId: "cap", value: { case: "unsignedInteger", value: 800n } },
    ],
  },
  profileEngineParameterVisibilityOverrides: [
    { parameterId: "profile-hidden", visibility: ParameterVisibility.HIDDEN },
    { parameterId: "profile-shown", visibility: ParameterVisibility.BASIC },
  ],
  generatedEngineParameterIds: ["key"],
  sizing: { derivedEngineParameterIds: ["cap"] },
});

describe("DeploymentParametersSection", () => {
  it("lists visible parameters in schema order with their tags and a masked secret", () => {
    render(<DeploymentParametersSection snapshot={snapshot} />);

    const rows = screen
      .getAllByRole("term")
      .map((term) => [term.textContent, term.nextElementSibling?.textContent]);
    expect(rows).toEqual([
      ["Slots", "default4"],
      ["Expert cap", "sized900"],
      ["Profile shown", "5"],
      ["API key", "generated••••••••"],
    ]);
    expect(screen.queryByText("s3cretvalue-abc")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Reveal API key" })).toBeVisible();
  });
});
