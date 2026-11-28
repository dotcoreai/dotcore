import type { ReactNode } from "react";

import { FieldError } from "@/components/ui/field-error";
import { Notice } from "@/components/ui/notice";
import { SegmentedControl } from "@/components/ui/segmented-control";
import {
  DeploymentFormSection,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import { hiddenByFitNote } from "@/features/models/profiles/profile-availability";
import type { MemoryBasis } from "@/features/models/profiles/profile-compatibility";
import { ProfileTable, type ProfileTableProps } from "@/features/models/profiles/profile-table";
import type { ProfileValueColumnId } from "@/features/models/profiles/profile-table-model";

import { ModelDeployTargetPicker } from "./model-deploy-target-picker";

/**
 * The deploy dialog's Model & machine step: the machine to run on when there is a choice, then the
 * model's profiles as a selectable table showing each one's weights and how much context per
 * request it fits, with the memory basis they are judged on in its toolbar. Notices say when the site's compatibility or the opening
 * context couldn't be checked.
 */
export function ModelDeployMachineStep({
  basis,
  compatibilityCheck,
  error,
  notices,
  onBasisChange,
  resetsRuntime,
  sizingFailed,
  table,
  target,
}: ModelDeployMachineStepProps) {
  return (
    <div className="grid gap-5">
      <DeploymentStepHeading>Model &amp; machine</DeploymentStepHeading>
      {notices}
      <ModelDeployTargetPicker {...target} />
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
      <DeploymentFormSection title="Profile">
        <ProfileTable
          {...table}
          columns={machineStepColumns}
          controls={
            onBasisChange ? (
              <SegmentedControl
                aria-label="Memory"
                onValueChange={onBasisChange}
                options={basisOptions}
                value={basis}
              />
            ) : undefined
          }
          defaultColumns={machineStepDefaultColumns}
          footnote={hiddenByFitNote}
        />
      </DeploymentFormSection>
      {error ? <FieldError>{error}</FieldError> : null}
      {resetsRuntime ? (
        <p className="text-xs text-muted-foreground" role="status">
          Changing the profile resets runtime settings to its defaults.
        </p>
      ) : null}
    </div>
  );
}

export interface ModelDeployMachineStepProps {
  basis: MemoryBasis;
  /** Where the read of which profiles run on the site stands, while it can't judge them yet. */
  compatibilityCheck?: "checking" | "failed";
  error?: string;
  /** Notices about the site and the targets, shown above the picker. */
  notices?: ReactNode;
  /** Changes the memory basis; without it, such as before compatibility is known, none shows. */
  onBasisChange?: (basis: MemoryBasis) => void;
  /** Whether the operator has runtime settings that a profile change resets. */
  resetsRuntime: boolean;
  /** Whether the opening context couldn't be sized, so the launch starts from profile defaults. */
  sizingFailed: boolean;
  table: Omit<ProfileTableProps, "columns" | "defaultColumns" | "footnote">;
  target: Parameters<typeof ModelDeployTargetPicker>[0];
}

const basisOptions = [
  { label: "Free now", value: "free" },
  { label: "Total", value: "total" },
] as const satisfies readonly { label: string; value: MemoryBasis }[];

// The table offers these columns; intelligence belongs to the model, and the maximum context to
// the View menu.
const machineStepColumns: readonly ProfileValueColumnId[] = [
  "weights",
  "memory",
  "fitsPerRequest",
  "maximumContext",
  "intelligence",
];

const machineStepDefaultColumns: readonly ProfileValueColumnId[] = [
  "weights",
  "memory",
  "fitsPerRequest",
];
