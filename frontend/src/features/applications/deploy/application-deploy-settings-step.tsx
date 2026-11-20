import type { ComponentProps } from "react";

import { ParametersForm } from "@/components/parameters/parameters";
import {
  DeploymentForm,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";

import type { ApplicationParameterField } from "../application-parameters";

/** The parameter form's state and handlers, shared by the Settings and Advanced steps. */
export type ApplicationParametersFormProps = Omit<
  ComponentProps<typeof ParametersForm>,
  "idPrefix" | "parameters"
>;

interface ApplicationDeploySettingsStepProps {
  /** The application's basic parameters. */
  fields: readonly ApplicationParameterField[];
  form: ApplicationParametersFormProps;
}

/** The Settings step of the application deploy dialog: the application's basic parameters. */
export function ApplicationDeploySettingsStep({
  fields,
  form,
}: ApplicationDeploySettingsStepProps) {
  return (
    <DeploymentForm>
      <DeploymentStepHeading>Settings</DeploymentStepHeading>
      <ApplicationParametersForm fields={fields} form={form} />
    </DeploymentForm>
  );
}

/** The parameter form of one application deploy step. */
export function ApplicationParametersForm({ fields, form }: ApplicationDeploySettingsStepProps) {
  return (
    <ParametersForm
      {...form}
      idPrefix="application-parameter"
      parameters={fields.map((field) => field.view)}
    />
  );
}
