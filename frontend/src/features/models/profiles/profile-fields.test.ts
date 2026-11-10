import { describe, expect, it } from "vitest";

import type { ProfileRow } from "./model-row";
import { compareProfiles } from "./profile-fields";

function profile(profileId: string, displayName: string, bytes?: number): ProfileRow {
  return {
    acceleratorRequirements: [],
    configurationId: "config",
    displayName,
    engineKinds: [],
    hardwareClassEstimates: [],
    scenarioUnsupportedClasses: [],
    imageInput: false,
    memory: bytes === undefined ? undefined : { kind: "estimate", max: bytes, min: bytes },
    modelId: "model",
    platforms: [],
    profileId,
    speculativeOptions: [],
  };
}

describe("compareProfiles", () => {
  it("orders by memory, largest first, then without memory, then name and id", () => {
    const profiles = [
      profile("none-b", "B"),
      profile("small", "Small", 4),
      profile("none-a", "A"),
      profile("large", "Large", 9),
      profile("same-z", "Same", 6),
      profile("same-a", "Same", 6),
    ];

    expect([...profiles].sort(compareProfiles).map(({ profileId }) => profileId)).toEqual([
      "large",
      "same-a",
      "same-z",
      "small",
      "none-a",
      "none-b",
    ]);
  });
});
