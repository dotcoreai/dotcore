import { Field } from "@/components/ui/field";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import {
  DeploymentForm,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import type { StoragePlacement } from "@/gen/dotcore/catalog/v1/storage_pb";

import type { ApplicationParameterField } from "../application-parameters";
import {
  type StorageChoiceField,
  storageChoiceKey,
  storagePlacementLabel,
  storagePlacementLifetime,
} from "../application-storage";
import {
  ApplicationParametersForm,
  type ApplicationParametersFormProps,
} from "./application-deploy-settings-step";

interface ApplicationDeployAdvancedStepProps {
  /** The application's advanced parameters. */
  fields: readonly ApplicationParameterField[];
  form: ApplicationParametersFormProps;
  onStorageChange: (field: StorageChoiceField, placement: StoragePlacement) => void;
  /** The storage bindings with a placement choice. */
  storageFields: readonly StorageChoiceField[];
  /** The chosen placement of each storage binding, by its key. */
  storageInputs: ReadonlyMap<string, StoragePlacement>;
}

/**
 * The Advanced step of the application deploy dialog: the application's advanced parameters, then
 * where each storage binding with a choice lives.
 */
export function ApplicationDeployAdvancedStep({
  fields,
  form,
  onStorageChange,
  storageFields,
  storageInputs,
}: ApplicationDeployAdvancedStepProps) {
  return (
    <DeploymentForm>
      <DeploymentStepHeading>Advanced</DeploymentStepHeading>
      <ApplicationParametersForm fields={fields} form={form} />
      {storageFields.length ? (
        <div className="grid gap-3">
          {storageFields.map((field) => (
            <StorageChoiceSelect
              field={field}
              key={storageChoiceKey(field)}
              onChange={(placement) => onStorageChange(field, placement)}
              value={storageInputs.get(storageChoiceKey(field)) ?? field.placements[0]}
            />
          ))}
        </div>
      ) : null}
    </DeploymentForm>
  );
}

/** Picks where one storage binding lives; the help popover says how long each choice keeps data. */
function StorageChoiceSelect({
  field,
  onChange,
  value,
}: {
  field: StorageChoiceField;
  onChange: (placement: StoragePlacement) => void;
  value: StoragePlacement;
}) {
  return (
    <Field
      controlId={`application-storage-${storageChoiceKey(field).replace("/", "-")}`}
      help={
        <div className="grid gap-1.5">
          <p>{field.description}</p>
          <ul className="grid gap-1">
            {field.placements.map((placement) => (
              <li key={placement}>
                <strong>{`${storagePlacementLabel(placement)}.`}</strong>{" "}
                {storagePlacementLifetime(placement)}
              </li>
            ))}
          </ul>
        </div>
      }
      label={field.displayName}
    >
      {(control) => (
        <NativeSelect
          {...control}
          className="w-full"
          onChange={(event) => onChange(Number(event.target.value))}
          value={value}
        >
          {field.placements.map((placement) => (
            <NativeSelectOption key={placement} value={placement}>
              {storagePlacementLabel(placement)}
            </NativeSelectOption>
          ))}
        </NativeSelect>
      )}
    </Field>
  );
}
