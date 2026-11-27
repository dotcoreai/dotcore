import type {
  Model,
  ModelEngineConfiguration,
  ModelProfile,
} from "@/gen/dotcore/catalog/v1/model_pb";

/**
 * The configuration a profile launches with. A profile lists one configuration per variant; this
 * returns the picked one (the compatibility read's pick for a chosen machine, or a deployment's
 * snapshot) and the first variant when none is given or the pick isn't one of the profile's. The
 * variants share their defaults, so the first stands in until a machine is chosen.
 */
export function profileConfiguration(
  model: Model,
  profile: ModelProfile | undefined,
  pickedConfigurationId?: string,
): ModelEngineConfiguration | undefined {
  if (!profile) return undefined;

  const configurationId =
    pickedConfigurationId && profile.configurationIds.includes(pickedConfigurationId)
      ? pickedConfigurationId
      : profile.configurationIds[0];
  return model.configurations.find((candidate) => candidate.configurationId === configurationId);
}

/** Every variant of a profile, in the profile's order; empty for a configuration the model lacks. */
export function profileConfigurations(
  model: Model,
  profile: ModelProfile | undefined,
): ModelEngineConfiguration[] {
  return (profile?.configurationIds ?? []).flatMap((configurationId) =>
    model.configurations.filter((candidate) => candidate.configurationId === configurationId),
  );
}
