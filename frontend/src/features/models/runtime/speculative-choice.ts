import { create } from "@bufbuild/protobuf";

import {
  type SpeculativeOption,
  speculativeTokenCountId,
} from "@/features/models/profiles/model-row";
import { type ModelOptionValues, ModelOptionValuesSchema } from "@/gen/dotcore/catalog/v1/model_pb";

/**
 * The operator's speculative decoding choice in the deploy dialog: the option that drafts tokens,
 * or an empty option id for off, and the token count as typed. The count is kept while off so
 * switching a method back on restores it.
 */
export interface SpeculativeChoice {
  count: string;
  optionId: string;
}

/**
 * The choice a profile resolves to: its first option defaulting on with that count, or off. Off
 * leaves the count empty, because the catalog recommends no count for an option it defaults off.
 */
export function defaultSpeculativeChoice(options: readonly SpeculativeOption[]): SpeculativeChoice {
  const active = options.find((option) => option.defaultCount > 0);
  return active
    ? { count: String(active.defaultCount), optionId: active.optionId }
    : { count: "", optionId: "" };
}

/**
 * The choice to edit: the stored one while its option is still declared, otherwise the profile's
 * default, as when a catalog refresh drops the chosen option.
 */
export function currentSpeculativeChoice(
  options: readonly SpeculativeOption[],
  choice: SpeculativeChoice,
): SpeculativeChoice {
  return !choice.optionId || options.some((option) => option.optionId === choice.optionId)
    ? choice
    : defaultSpeculativeChoice(options);
}

/**
 * The choice after the operator picks another method. An empty count takes the profile's count
 * for the option when it is on, otherwise the configuration default when it is on, otherwise the
 * lowest enabled count, so a method picked on a profile defaulting off starts valid.
 */
export function speculativeMethodChoice(
  options: readonly SpeculativeOption[],
  configurationOptions: readonly SpeculativeOption[],
  choice: SpeculativeChoice,
  optionId: string,
): SpeculativeChoice {
  const option = options.find((candidate) => candidate.optionId === optionId);
  if (!option || choice.count) return { ...choice, optionId };

  const configurationCount =
    configurationOptions.find((candidate) => candidate.optionId === optionId)?.defaultCount ?? 0;
  const count =
    [option.defaultCount, configurationCount].find((candidate) => candidate > 0) ??
    speculativeEnabledMinimum(option);
  return { count: String(count), optionId };
}

/** The error of a choice's count, which must be within the chosen option's range above 0. */
export function speculativeChoiceError(
  options: readonly SpeculativeOption[],
  choice: SpeculativeChoice,
): string | undefined {
  const option = options.find((candidate) => candidate.optionId === choice.optionId);
  if (!option || choiceCount(option, choice) !== undefined) return undefined;

  return `Speculative token count must be an integer from ${speculativeEnabledMinimum(option)} to ${option.maximum} tokens.`;
}

/**
 * The option overrides a choice sends: none while it matches the profile's default, otherwise
 * every speculative option, the chosen one at its count and the others at 0. Undefined while the
 * count is invalid.
 */
export function speculativeOptionOverrides(
  options: readonly SpeculativeOption[],
  choice: SpeculativeChoice,
): ModelOptionValues[] | undefined {
  if (speculativeChoiceError(options, choice)) return undefined;
  if (sameChoice(options, choice, defaultSpeculativeChoice(options))) return [];

  return options.map((option) =>
    create(ModelOptionValuesSchema, {
      optionId: option.optionId,
      values: [
        {
          parameterId: speculativeTokenCountId,
          value: {
            case: "unsignedInteger",
            value: BigInt(option.optionId === choice.optionId ? choice.count : 0),
          },
        },
      ],
    }),
  );
}

/**
 * Describes a choice that differs from the profile's default for the deploy summary, such as
 * "Speculative decoding MTP, 5 tokens" or "Speculative decoding off"; empty when unchanged or
 * while the count is invalid.
 */
export function speculativeCustomizations(
  options: readonly SpeculativeOption[],
  choice: SpeculativeChoice,
): string[] {
  if (sameChoice(options, choice, defaultSpeculativeChoice(options))) return [];

  const option = options.find((candidate) => candidate.optionId === choice.optionId);
  if (!option) return ["Speculative decoding off"];

  const count = choiceCount(option, choice);
  if (count === undefined) return [];
  return [
    `Speculative decoding ${option.displayName}, ${count} ${count === 1 ? "token" : "tokens"}`,
  ];
}

/** The lowest count a chosen option accepts: at least one token, because 0 is the off choice. */
export function speculativeEnabledMinimum(option: SpeculativeOption): number {
  return Math.max(option.minimum, 1);
}

// Off compares by option alone, because the kept count does not reach the deployment.
function sameChoice(
  options: readonly SpeculativeOption[],
  left: SpeculativeChoice,
  right: SpeculativeChoice,
): boolean {
  if (left.optionId !== right.optionId) return false;

  const option = options.find((candidate) => candidate.optionId === left.optionId);
  return !option || choiceCount(option, left) === choiceCount(option, right);
}

function choiceCount(option: SpeculativeOption, choice: SpeculativeChoice): number | undefined {
  if (!/^\d+$/.test(choice.count)) return undefined;

  const count = Number(choice.count);
  return count >= speculativeEnabledMinimum(option) && count <= option.maximum ? count : undefined;
}
