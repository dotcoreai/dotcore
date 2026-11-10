import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";

// Parameter id the catalog contract fixes for a speculative option's draft token count.
const TOKEN_COUNT_PARAMETER_ID = "token-count";

/**
 * Describes the speculative decoding a deployment resolved to: the active method's display name
 * and draft token count, or "Off" when every speculative option resolved to zero. Returns
 * undefined when the catalog model is unavailable or the deployment carries no speculative option.
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

    const value = values?.values.find((entry) => entry.parameterId === TOKEN_COUNT_PARAMETER_ID);
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
