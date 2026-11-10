import type { ReactNode } from "react";

import { FieldError } from "@/components/ui/field-error";
import { Notice } from "@/components/ui/notice";
import { DeploymentStepHeading } from "@/features/deployments/deployment-form-layout";
import { hiddenByFitNote } from "@/features/models/profiles/profile-availability";
import type { ProfileMetricColumnId } from "@/features/models/profiles/profile-fields";
import { ProfileTable, type ProfileTableProps } from "@/features/models/profiles/profile-table";

/**
 * The deploy dialog's Profile step: the model's profiles as a selectable table, with notices
 * saying when the site's compatibility or the chosen context couldn't be checked.
 */
export function ModelDeployProfileStep({
  compatibilityCheck,
  error,
  notices,
  resetsRuntime,
  sizingFailed,
  table,
}: ModelDeployProfileStepProps) {
  return (
    <div className="grid gap-4">
      <DeploymentStepHeading>Profile</DeploymentStepHeading>
      {notices}
      {compatibilityCheck === "failed" ? (
        <Notice tone="warning">Couldn&apos;t check which profiles can run on this site.</Notice>
      ) : compatibilityCheck === "checking" ? (
        <p className="text-xs text-muted-foreground" role="status">
          Checking which profiles can run on this site…
        </p>
      ) : null}
      {sizingFailed ? (
        <Notice tone="warning">
          Couldn&apos;t size this context, so other settings start from the profile defaults.
        </Notice>
      ) : null}
      <ProfileTable {...table} defaultColumns={profileStepColumns} footnote={hiddenByFitNote} />
      {error ? <FieldError>{error}</FieldError> : null}
      {resetsRuntime ? (
        <p className="text-xs text-muted-foreground" role="status">
          Changing the profile resets runtime settings to its defaults.
        </p>
      ) : null}
    </div>
  );
}

export interface ModelDeployProfileStepProps {
  /** Where the read of which profiles run on the site stands, while it can't judge them yet. */
  compatibilityCheck?: "checking" | "failed";
  error?: string;
  /** Notices about the site and the targets, shown above the table. */
  notices?: ReactNode;
  /** Whether the operator has runtime settings that a profile change resets. */
  resetsRuntime: boolean;
  /** Whether the chosen context couldn't be sized, so the launch starts from profile defaults. */
  sizingFailed: boolean;
  table: Omit<ProfileTableProps, "defaultColumns" | "footnote">;
}

// Intelligence belongs to the model, so the dialog leaves it to its View menu.
const profileStepColumns: readonly ProfileMetricColumnId[] = [
  "memory",
  "maximumContext",
  "totalContext",
];
