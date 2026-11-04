import type { ReactNode, RefObject } from "react";

import { DetailList } from "@/components/ui/detail-list";
import { FieldError } from "@/components/ui/field-error";
import { Input } from "@/components/ui/input";

import { DeploymentFormSection, DeploymentStepHeading } from "./deployment-form-layout";

interface DeploymentSummaryStepProps {
  /** The summary's detail rows, describing what will be deployed. */
  children: ReactNode;
  disabled: boolean;
  name: string;
  nameError?: string;
  /** The name input's element ID, which also prefixes its error's ID. */
  nameInputId: string;
  nameRef: RefObject<HTMLInputElement | null>;
  /** Notices that block or qualify deployment, shown above the deployment name. */
  notices?: ReactNode;
  onNameBlur: () => void;
  onNameChange: (name: string) => void;
  /** Fields that follow the name, such as the route name. */
  nameFields?: ReactNode;
}

/**
 * The final Deploy step shared by the deployment dialogs: the notices that block or qualify
 * deployment, the deployment name, the fields that follow it, and a summary of what will be
 * deployed.
 */
export function DeploymentSummaryStep({
  children,
  disabled,
  name,
  nameError,
  nameInputId,
  nameRef,
  notices,
  onNameBlur,
  onNameChange,
  nameFields,
}: DeploymentSummaryStepProps) {
  const errorId = `${nameInputId}-error`;

  return (
    <div className="grid gap-4">
      <DeploymentStepHeading>Deploy</DeploymentStepHeading>
      {notices}
      {/* The error sits outside the label so it describes the field instead of naming it. */}
      <div className="grid gap-1 text-sm">
        <label htmlFor={nameInputId}>Deployment name</label>
        <Input
          aria-describedby={nameError ? errorId : undefined}
          aria-invalid={nameError ? "true" : undefined}
          disabled={disabled}
          id={nameInputId}
          onBlur={onNameBlur}
          onChange={(event) => onNameChange(event.target.value)}
          ref={nameRef}
          value={name}
        />
        {nameError ? <FieldError id={errorId}>{nameError}</FieldError> : null}
      </div>
      {nameFields}
      <DeploymentFormSection title="Summary">
        <div className="rounded-lg border border-border bg-muted/30 px-3 pb-3">
          <DetailList>{children}</DetailList>
        </div>
      </DeploymentFormSection>
    </div>
  );
}
