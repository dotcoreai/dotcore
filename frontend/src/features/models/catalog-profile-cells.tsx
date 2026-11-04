import { CpuIcon, FastForwardIcon, GpuIcon } from "lucide-react";

import { Metric, PendingValue } from "@/components/data-table/data-table-cells";
import { Badge } from "@/components/ui/badge";
import type { ValueRange } from "@/lib/data-table-model";
import { cn } from "@/lib/utils";

import {
  engineKindLabel,
  type ProfileAcceleratorRequirement,
  type ProfileFit,
  type ProfileMemory,
  type ProfileRow,
} from "./catalog-model-row";
import type { ProfileStatus } from "./catalog-profile-fields";
import {
  formatContext,
  formatMemory,
  formatMemoryFit,
  formatReferenceScore,
  type MemoryRange,
} from "./catalog-table-model";

/**
 * A profile's name cell: its name and pills (see `ProfilePills`), then its quantization and an
 * optional status line.
 */
export function ProfileNameCell({
  profile,
  status,
}: {
  profile: ProfileRow;
  status?: ProfileStatus;
}) {
  return (
    <>
      <div className="flex flex-wrap items-center gap-1.5 text-[13px] font-semibold">
        {profile.displayName}
        <ProfilePills profile={profile} />
      </div>
      {profile.quantization ? (
        <div className="mt-0.5 text-xs text-muted-foreground">{profile.quantization}</div>
      ) : null}
      {status ? (
        <div className={cn("mt-0.5 text-xs", statusTones[status.tone])}>{status.text}</div>
      ) : null}
    </>
  );
}

/**
 * A profile's engine pill, one pill per speculative decoding method its configuration offers, and
 * one pill per accelerator vendor its supported hardware classes select, which tells where it runs.
 * The profile detail lists the individual hardware classes.
 */
export function ProfilePills({ profile }: { profile: ProfileRow }) {
  return (
    <>
      {profile.engineKinds.map((kind) => (
        <Badge className={pillClassName} key={kind} title="Inference engine" variant="neutral">
          <CpuIcon aria-hidden="true" />
          {engineKindLabel(kind)}
        </Badge>
      ))}
      {profile.speculativeOptions.map(({ displayName, optionId }) => (
        <Badge
          className={pillClassName}
          key={optionId}
          title="Speculative decoding"
          variant="neutral"
        >
          <FastForwardIcon aria-hidden="true" />
          {displayName}
        </Badge>
      ))}
      {supportedVendors(profile.acceleratorRequirements).map((vendor) => (
        <Badge className={pillClassName} key={vendor} title="Supported hardware" variant="neutral">
          <GpuIcon aria-hidden="true" />
          {vendorLabel(vendor)}
        </Badge>
      ))}
    </>
  );
}

/**
 * A memory value with its label, or a placeholder while estimates load. With a fit, the label says
 * whether the estimate fits the location's memory, and a profile that can't serve the scenario
 * reads "Unsupported".
 */
export function MemoryValue({
  fit,
  memory,
  pending,
  unsupported = false,
}: {
  fit?: ProfileFit;
  memory: MemoryRange | ProfileMemory | undefined;
  pending: boolean;
  unsupported?: boolean;
}) {
  if (pending) return <MemoryPlaceholder />;
  if (unsupported || fit?.fit === "unsupported") return <Metric value="Unsupported" />;

  const formatted = formatMemory(memory);
  return (
    <Metric
      label={fit ? formatMemoryFit(fit.fit, fit.basis) : formatted.label}
      value={formatted.value}
    />
  );
}

/** The pending memory value while estimates load, so late results don't read as final. */
export function MemoryPlaceholder() {
  return <PendingValue label="Loading estimate" />;
}

/** A context size or range, such as "32k" or "32k–128k". */
export function ContextValue({ context }: { context: ValueRange | number | undefined }) {
  return <Metric value={formatContext(context)} />;
}

/** A model's intelligence score, or nothing when unscored. */
export function IntelligenceValue({ score }: { score: number | undefined }) {
  return score === undefined ? null : <Metric value={formatReferenceScore(score)} />;
}

const pillClassName = "h-4.5 rounded-md px-1.5 py-0 font-normal";

const statusTones: Record<ProfileStatus["tone"], string> = {
  advisory: "text-warning-foreground",
  blocked: "text-destructive",
  unknown: "text-muted-foreground",
};

// Vendors in first-seen order, so classes of one vendor collapse into a single pill.
function supportedVendors(requirements: readonly ProfileAcceleratorRequirement[]): string[] {
  return [...new Set(requirements.flatMap((requirement) => requirement.vendors))];
}

function vendorLabel(vendor: string): string {
  switch (vendor) {
    case "nvidia":
      return "NVIDIA";
    case "apple":
      return "Apple Silicon";
    default:
      return vendor;
  }
}
