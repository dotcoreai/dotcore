import {
  speculativeEnabledId,
  speculativeTokenCountId,
} from "@/features/models/profiles/model-row";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";

/**
 * Describes the speculative decoding a deployment resolved to: the active method's display name
 * and draft token count, or its name alone for a switch, or "Off" when every speculative option
 * resolved off. Returns undefined when the catalog model is unavailable or the deployment carries
 * no speculative option.
 */
export function deploymentSpeculativeLabel(
  snapshot: ResolvedLocalModelDeploymentSnapshot,
  catalogModel: Model | undefined,
): string | undefined {
  if (!catalogModel) return undefined;

  const active: string[] = [];
  let resolved = false;

  for (const option of snapshot.resolvedOptions) {
    const values = option.values;
    const declared = catalogModel.options.find(
      (candidate) =>
        candidate.optionId === values?.optionId && candidate.kind.case === "speculativeDecoding",
    );
    if (!declared) continue;

    const switched = values?.values.find((entry) => entry.parameterId === speculativeEnabledId);
    if (switched?.value.case === "boolean") {
      resolved = true;
      if (switched.value.value) active.push(declared.displayName);
      continue;
    }

    const value = values?.values.find((entry) => entry.parameterId === speculativeTokenCountId);
    if (value?.value.case !== "unsignedInteger") continue;

    resolved = true;
    if (value.value.value > 0n) {
      const count = value.value.value;
      active.push(`${declared.displayName} · ${count} ${count === 1n ? "token" : "tokens"}`);
    }
  }

  if (!resolved) return undefined;
  return active.length ? active.join(", ") : "Off";
}
