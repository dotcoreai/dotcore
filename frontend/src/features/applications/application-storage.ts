import { create } from "@bufbuild/protobuf";

import type { ApplicationStorageAttachment } from "@/gen/dotcore/application/v1/deployment_pb";
import {
  type ApplicationStorageChoice,
  ApplicationStorageChoiceSchema,
} from "@/gen/dotcore/application/v1/service_pb";
import type { Application } from "@/gen/dotcore/catalog/v1/application_pb";
import { StoragePlacement } from "@/gen/dotcore/catalog/v1/storage_pb";

/** One storage binding that allows several placements, so the operator picks where it lives. */
export interface StorageChoiceField {
  bindingName: string;
  /** Allowed placements in catalog order; the first is the default. */
  placements: readonly StoragePlacement[];
  serviceName: string;
}

/** The bindings of an application that allow more than one placement, in declaration order. */
export function storageChoiceFields(application: Application): StorageChoiceField[] {
  return (application.graph?.services ?? []).flatMap((service) =>
    service.storageBindings
      .filter((binding) => binding.placements.length > 1)
      .map((binding) => ({
        bindingName: binding.bindingName,
        placements: binding.placements,
        serviceName: service.serviceName,
      })),
  );
}

/** Each choice's catalog default, keyed by `storageChoiceKey`. */
export function defaultStorageInputs(
  fields: readonly StorageChoiceField[],
): Map<string, StoragePlacement> {
  return new Map(fields.map((field) => [storageChoiceKey(field), field.placements[0]]));
}

/** Identifies a binding within an application. */
export function storageChoiceKey(field: Pick<StorageChoiceField, "bindingName" | "serviceName">) {
  return `${field.serviceName}/${field.bindingName}`;
}

/** The fields whose picked placement differs from the catalog default, with that placement. */
export function changedStorageChoices(
  fields: readonly StorageChoiceField[],
  inputs: ReadonlyMap<string, StoragePlacement>,
): { field: StorageChoiceField; placement: StoragePlacement }[] {
  return fields.flatMap((field) => {
    const placement = inputs.get(storageChoiceKey(field));
    return placement === undefined || placement === field.placements[0]
      ? []
      : [{ field, placement }];
  });
}

/** The creation request's storage choices: only bindings the operator moved off their default. */
export function storageChoiceOverrides(
  fields: readonly StorageChoiceField[],
  inputs: ReadonlyMap<string, StoragePlacement>,
): ApplicationStorageChoice[] {
  return changedStorageChoices(fields, inputs).map(({ field, placement }) =>
    create(ApplicationStorageChoiceSchema, {
      serviceName: field.serviceName,
      bindingName: field.bindingName,
      placement,
    }),
  );
}

/** The operator-facing name of a binding's storage, such as "Models storage". */
export function storageBindingLabel(bindingName: string): string {
  return `${bindingName.charAt(0).toLocaleUpperCase()}${bindingName.slice(1)} storage`;
}

/** The operator-facing name of a placement. */
export function storagePlacementLabel(placement: StoragePlacement): string {
  switch (placement) {
    case StoragePlacement.APPLICATION:
      return "Shared by the app";
    case StoragePlacement.DEPLOYMENT:
      return "This deployment";
    default:
      return "Unknown";
  }
}

/** What a placement means for the data's lifetime, shown in the help popover. */
export function storagePlacementLifetime(placement: StoragePlacement): string {
  switch (placement) {
    case StoragePlacement.APPLICATION:
      return "Kept after teardown and reused by the app's next deployment.";
    case StoragePlacement.DEPLOYMENT:
      return "Deleted when the deployment is torn down.";
    default:
      return "";
  }
}

/**
 * What tearing a deployment down does to its data, in the words `dotc application teardown` uses:
 * storage placed with the deployment is deleted, storage shared by the app is kept.
 */
export function teardownConsequence(attachments: readonly ApplicationStorageAttachment[]): string {
  const bindings = (placement: StoragePlacement) =>
    attachments
      .filter((attachment) => attachment.placement === placement)
      .map((attachment) => attachment.bindingName)
      .join(", ");
  const deleted = bindings(StoragePlacement.DEPLOYMENT);
  const kept = bindings(StoragePlacement.APPLICATION);

  let text = "This stops and removes its workloads";
  if (deleted) text += ` and deletes this deployment's data (${deleted})`;
  text += kept
    ? `. It keeps the data shared by the app (${kept}) and the catalog application.`
    : ". The catalog application is kept.";
  return text;
}
