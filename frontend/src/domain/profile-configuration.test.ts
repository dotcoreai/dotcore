import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import {
  ModelEngineConfigurationSchema,
  ModelProfileSchema,
  ModelSchema,
} from "@/gen/dotcore/catalog/v1/model_pb";

import { profileConfiguration, profileConfigurations } from "./profile-configuration";

const model = create(ModelSchema, {
  configurations: ["other", "gb10", "rtx5090"].map((configurationId) =>
    create(ModelEngineConfigurationSchema, { configurationId }),
  ),
});
const profile = create(ModelProfileSchema, {
  configurationIds: ["rtx5090", "gb10"],
  profileId: "iq3-s",
});

describe("profileConfiguration", () => {
  it("returns the picked variant", () => {
    expect(profileConfiguration(model, profile, "gb10")?.configurationId).toBe("gb10");
  });

  it("falls back to the first variant without a pick", () => {
    expect(profileConfiguration(model, profile)?.configurationId).toBe("rtx5090");
    expect(profileConfiguration(model, profile, "")?.configurationId).toBe("rtx5090");
  });

  it("ignores a pick that is not a variant of the profile", () => {
    expect(profileConfiguration(model, profile, "other")?.configurationId).toBe("rtx5090");
  });

  it("is undefined without a profile", () => {
    expect(profileConfiguration(model, undefined, "gb10")).toBeUndefined();
  });
});

describe("profileConfigurations", () => {
  it("lists the variants in the profile's order", () => {
    expect(profileConfigurations(model, profile).map((c) => c.configurationId)).toEqual([
      "rtx5090",
      "gb10",
    ]);
  });

  it("is empty without a profile", () => {
    expect(profileConfigurations(model, undefined)).toEqual([]);
  });
});
