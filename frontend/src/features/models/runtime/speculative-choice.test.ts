import { describe, expect, it } from "vitest";

import type { SpeculativeOption } from "@/features/models/profiles/model-row";

import {
  defaultSpeculativeChoice,
  speculativeChoiceError,
  speculativeCustomizations,
  speculativeMethodChoice,
  speculativeOptionOverrides,
} from "./speculative-choice";

describe("speculative choice", () => {
  const mtp = option("mtp", "MTP", 3, 8);
  const dflash = option("dflash", "DFlash", 0, 15);

  it("starts from the first option defaulting on, or off with no count", () => {
    expect(defaultSpeculativeChoice([dflash, mtp])).toEqual({ count: "3", optionId: "mtp" });
    expect(defaultSpeculativeChoice([dflash])).toEqual({ count: "", optionId: "" });
  });

  it("sets every speculative option, the chosen one to its count and the others to 0", () => {
    expect(overrides([mtp, dflash], { count: "12", optionId: "dflash" })).toEqual([
      ["mtp", "token-count", 0n],
      ["dflash", "token-count", 12n],
    ]);
    expect(overrides([mtp, dflash], { count: "3", optionId: "" })).toEqual([
      ["mtp", "token-count", 0n],
      ["dflash", "token-count", 0n],
    ]);
  });

  it("sends nothing for the default choice and holds back an invalid count", () => {
    // Off keeps a typed count that never reaches the deployment.
    expect(overrides([dflash], { count: "7", optionId: "" })).toEqual([]);
    expect(overrides([mtp, dflash], { count: "03", optionId: "mtp" })).toEqual([]);

    expect(speculativeOptionOverrides([mtp], { count: "0", optionId: "mtp" })).toBeUndefined();
    expect(speculativeChoiceError([mtp], { count: "9", optionId: "mtp" })).toBe(
      "Speculative token count must be an integer from 1 to 8 tokens.",
    );
    expect(speculativeChoiceError([mtp], { count: "", optionId: "" })).toBeUndefined();
  });

  it("prefills an empty count from the profile, the configuration default, or the lowest enabled count", () => {
    const off = { count: "", optionId: "" };
    const configuration = [mtp, dflash];
    expect(speculativeMethodChoice(configuration, configuration, off, "mtp")).toEqual({
      count: "3",
      optionId: "mtp",
    });
    expect(speculativeMethodChoice(configuration, configuration, off, "dflash")).toEqual({
      count: "1",
      optionId: "dflash",
    });

    // A profile count wins over the configuration default; a profile defaulting off does not.
    const profile = [option("mtp", "MTP", 5, 8), option("dflash", "DFlash", 0, 15)];
    expect(speculativeMethodChoice(profile, configuration, off, "mtp")).toEqual({
      count: "5",
      optionId: "mtp",
    });
    const profileOff = [option("mtp", "MTP", 0, 8), dflash];
    expect(speculativeMethodChoice(profileOff, configuration, off, "mtp")).toEqual({
      count: "3",
      optionId: "mtp",
    });

    // A typed count is kept, even outside the new method's range.
    expect(
      speculativeMethodChoice(
        configuration,
        configuration,
        { count: "12", optionId: "dflash" },
        "mtp",
      ),
    ).toEqual({ count: "12", optionId: "mtp" });
  });

  it("summarizes a changed choice from its parsed count, and nothing while it is invalid", () => {
    expect(speculativeCustomizations([mtp], { count: "05", optionId: "mtp" })).toEqual([
      "Speculative decoding MTP, 5 tokens",
    ]);
    expect(speculativeCustomizations([mtp], { count: "01", optionId: "mtp" })).toEqual([
      "Speculative decoding MTP, 1 token",
    ]);
    expect(speculativeCustomizations([mtp], { count: "9", optionId: "mtp" })).toEqual([]);
    expect(speculativeCustomizations([mtp], { count: "03", optionId: "mtp" })).toEqual([]);
    expect(speculativeCustomizations([mtp], { count: "3", optionId: "" })).toEqual([
      "Speculative decoding off",
    ]);
  });
});

function option(
  optionId: string,
  displayName: string,
  defaultCount: number,
  maximum: number,
): SpeculativeOption {
  return { defaultCount, displayName, maximum, minimum: 0, optionId };
}

function overrides(options: SpeculativeOption[], choice: { count: string; optionId: string }) {
  return speculativeOptionOverrides(options, choice)?.flatMap((override) =>
    override.values.map((value) => [override.optionId, value.parameterId, value.value.value]),
  );
}
