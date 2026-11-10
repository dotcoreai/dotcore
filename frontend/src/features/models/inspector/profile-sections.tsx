import { useMemo, useState } from "react";

import type { DataTablePredicate } from "@/components/data-table/data-table-view";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { Notice } from "@/components/ui/notice";
import { StatusBadge } from "@/components/ui/status-badge";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  engineKindLabel,
  formatSpeculativeDefault,
  formatSpeculativeRange,
  type ModelRow,
  type ProfileRow,
} from "@/features/models/profiles/model-row";
import {
  formatContext,
  formatMemory,
  formatMemoryFit,
} from "@/features/models/profiles/model-table-model";
import {
  type CatalogProfileCompatibility,
  catalogRunsOn,
  hiddenByFitNote,
  type ProfileMachine,
  profileMachines,
  profileNoneRun,
  siteProfileAvailability,
} from "@/features/models/profiles/profile-availability";
import { MemoryPlaceholder, ProfilePills } from "@/features/models/profiles/profile-cells";
import { defaultMemoryBasis } from "@/features/models/profiles/profile-compatibility";
import type { ProfileMetricColumnId } from "@/features/models/profiles/profile-fields";
import { ProfileTable } from "@/features/models/profiles/profile-table";
import type { ProfileTableField } from "@/features/models/profiles/profile-table-model";
import {
  type CatalogChips,
  defaultCatalogChips,
  profileTableStart,
} from "@/features/models/profiles/profile-table-seed";
import { useSizedForChipSettings } from "@/features/models/profiles/use-sized-for-chip-settings";
import type { Model, ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { formatBytes } from "@/lib/format";
import { cn } from "@/lib/utils";

/**
 * The Profiles section of the catalog model panel: the model's profiles in the catalog page's
 * table with Profile and Memory columns, each row opening its profile. With a compatibility read,
 * the Runs on chip applies the catalog's Runs on rule and the footer counts what the memory fit
 * hides; without one, every profile is listed. The table starts with the catalog's chips.
 */
export function CatalogProfiles({
  catalogChips = defaultCatalogChips,
  compatibility,
  memoryPending,
  onSelectProfile,
  row,
}: {
  catalogChips?: CatalogChips;
  compatibility?: CatalogProfileCompatibility;
  memoryPending: boolean;
  onSelectProfile?: (profileId: string) => void;
  row: ModelRow;
}) {
  const read = compatibility?.compatibility;
  const targets = compatibility?.targets ?? noTargets;
  const sizingFailed = compatibility?.sizingFailed ?? false;
  const basis = compatibility?.basis ?? defaultMemoryBasis;
  const loadingVisible = useDelayedLoading(compatibility?.loading ?? false);
  // Runs on judges memory fit from the shown read, as the catalog does; after the scenario's read
  // failed, it judges support alone.
  const filter = useMemo(
    () => (read ? catalogRunsOn(read, targets, { basis, fit: !sizingFailed }) : undefined),
    [basis, read, sizingFailed, targets],
  );
  const runsOn = useMemo(
    () =>
      read &&
      filter && {
        ...filter,
        noneRun: profileNoneRun(read, filter, row, row.profiles, targets),
      },
    [filter, read, row, targets],
  );
  // The catalog's chips, with Runs on once the read first offers it; they stay fixed from then on
  // for the model, so later reads never move the chip under the operator.
  const [landedStart, setLandedStart] = useState<{
    modelId: string;
    start: DataTablePredicate<ProfileTableField>[];
  }>();
  if (filter && landedStart?.modelId !== row.modelId)
    setLandedStart({ modelId: row.modelId, start: profileTableStart(catalogChips, filter.field) });
  const start = useMemo(
    () =>
      landedStart?.modelId === row.modelId
        ? landedStart.start
        : profileTableStart(catalogChips, undefined),
    [catalogChips, landedStart, row.modelId],
  );
  const onScenarioChange = filter ? compatibility?.onScenarioChange : undefined;
  const onBasisChange = filter ? compatibility?.onBasisChange : undefined;
  const sizedForSettings = useSizedForChipSettings(
    onScenarioChange && {
      onChange: onScenarioChange,
      start: compatibility?.startScenario,
      value: compatibility?.scenario,
    },
    onBasisChange && {
      onChange: onBasisChange,
      start: compatibility?.startBasis ?? defaultMemoryBasis,
      value: basis,
    },
  );
  const availability = useMemo(
    () => (read ? siteProfileAvailability(read, row.modelId, targets) : undefined),
    [read, row.modelId, targets],
  );

  return (
    <section aria-labelledby="profiles-heading">
      <InspectorSectionHeader headingId="profiles-heading" title="Profiles" />
      <div className="mt-3 grid gap-2">
        {compatibility?.error ? (
          <Notice tone="warning">{"Couldn't check which profiles can run on this site."}</Notice>
        ) : loadingVisible && !read ? (
          <p className="text-xs text-muted-foreground" role="status">
            Checking which profiles can run on this site…
          </p>
        ) : null}
        <ProfileTable
          availability={availability}
          chipSettings={sizedForSettings}
          columns={panelColumns}
          key={row.modelId}
          estimatesPending={memoryPending}
          footnote={hiddenByFitNote}
          onOpen={(profileId) => onSelectProfile?.(profileId)}
          profiles={row.profiles}
          runsOn={runsOn}
          start={start}
        />
      </div>
    </section>
  );
}

/**
 * The profile sub-screen of the catalog model panel: a Profile section with the profile's
 * description, engine, memory, platforms, and catalog identifiers, then, with a compatibility
 * read, a Runs on section listing each configured model execution target and whether the profile
 * runs there.
 */
export function CatalogProfileSections({
  compatibility,
  memoryPending,
  model,
  profile,
  row,
}: {
  compatibility?: CatalogProfileCompatibility;
  memoryPending: boolean;
  model: Model;
  profile: ModelProfile;
  row: ProfileRow;
}) {
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile.configurationId,
  );

  return (
    <>
      <section aria-labelledby="profile-summary-heading">
        <InspectorSectionHeader
          headingId="profile-summary-heading"
          meta={
            <span className="inline-flex flex-wrap items-center justify-end gap-1.5">
              <ProfilePills profile={row} />
            </span>
          }
          title="Profile"
        />
        {row.description ? (
          <p className="mt-2 text-[12.5px] text-muted-foreground">{row.description}</p>
        ) : null}
        <DetailList>
          <Detail
            label="Engine"
            value={row.engineKinds.map(engineKindLabel).join(", ") || "Unavailable"}
          />
          <Detail label="Quantization" value={row.quantization ?? "Not declared"} />
          <Detail label="Memory" value={<ProfileMemoryText pending={memoryPending} row={row} />} />
          {!memoryPending && (row.fit || row.scenarioUnsupported) ? (
            <Detail label="Memory fit" value={fitText(row)} />
          ) : null}
          <Detail label="Maximum context" value={formatContext(row.maximumContext)} />
          <Detail label="Total context" value={formatContext(row.totalContext)} />
          {row.speculativeOptions.length ? (
            <>
              <Detail
                label="Speculative decoding"
                value={row.speculativeOptions
                  .map((option) => `${option.displayName} · ${formatSpeculativeRange(option)}`)
                  .join(", ")}
              />
              <Detail
                label="Speculative default"
                value={formatSpeculativeDefault(row.speculativeOptions)}
              />
            </>
          ) : null}
          <Detail
            label="Platforms"
            value={
              row.platforms
                .map((platform) => `${platform.operatingSystem}/${platform.architecture}`)
                .join(", ") || "Unavailable"
            }
          />
          <Detail label="Accelerator" value={acceleratorLabel(row)} />
          <Detail
            label="Engine release"
            value={
              [
                ...new Set(
                  configuration?.engineArtifacts.map(({ engineReleaseId }) => engineReleaseId),
                ),
              ].join(", ") || "Unavailable"
            }
          />
          <Detail label="Profile ID" value={profile.profileId} />
        </DetailList>
      </section>
      {compatibility ? (
        <ProfileRunsOn compatibility={compatibility} modelId={model.modelId} row={row} />
      ) : null}
    </>
  );
}

const panelColumns: readonly ProfileMetricColumnId[] = ["memory"];

const noTargets: readonly ConfiguredExecutionTarget[] = [];

/**
 * The Runs on section of a profile's page: each evaluated target with the profile's status pill
 * and why it can't run or what it advises, those that run it first.
 */
function ProfileRunsOn({
  compatibility,
  modelId,
  row,
}: {
  compatibility: CatalogProfileCompatibility;
  modelId: string;
  row: ProfileRow;
}) {
  const read = compatibility.compatibility;
  const loadingVisible = useDelayedLoading(compatibility.loading);
  const machines = read ? profileMachines(read, modelId, row, compatibility.targets) : [];
  const runCount = machines.filter(({ verdict }) => verdict.status === "runs").length;

  return (
    <section aria-labelledby="profile-runs-on-heading">
      <InspectorSectionHeader
        headingId="profile-runs-on-heading"
        meta={machines.length ? `${runCount} of ${machines.length} can run` : undefined}
        title="Runs on"
      />
      {compatibility.error ? (
        <Notice className="mt-3" tone="warning">
          {"Couldn't check which machines can run this profile."}
        </Notice>
      ) : !read ? (
        loadingVisible ? (
          <p className="mt-3 text-xs text-muted-foreground" role="status">
            Checking which machines can run this profile…
          </p>
        ) : null
      ) : read.targets.length ? (
        <MachineTable machines={machines} />
      ) : (
        <p className="mt-3 text-[12.5px] text-muted-foreground">
          No model execution targets are configured. Add one to the site configuration to deploy.
        </p>
      )}
    </section>
  );
}

/** A compact table of machines: name, hardware, and note, with the status pill on the right. */
function MachineTable({ machines }: { machines: readonly ProfileMachine[] }) {
  return (
    <div className="mt-3 overflow-hidden rounded-xl border border-border bg-card">
      <Table>
        <TableHeader>
          <TableRow className="hover:bg-transparent">
            <TableHead className="h-auto w-full px-3 py-2 text-xs text-muted-foreground">
              Machine
            </TableHead>
            <TableHead className="h-auto px-3 py-2 text-xs text-muted-foreground">Status</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {machines.map(({ hardware, key, name, note, status }) => (
            <TableRow className="hover:bg-transparent" key={key}>
              <TableCell className="px-3 py-2.5 align-top text-[13px] whitespace-normal">
                <div className="font-semibold">{name}</div>
                {hardware ? <div className="text-xs text-muted-foreground">{hardware}</div> : null}
                {note ? (
                  <div
                    className={cn(
                      "mt-0.5 text-xs",
                      note.tone === "advisory"
                        ? "text-warning-foreground"
                        : "text-muted-foreground",
                    )}
                  >
                    {note.text}
                  </div>
                ) : null}
              </TableCell>
              <TableCell className="px-3 py-2.5 align-top">
                <StatusBadge status={status} />
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  );
}

/**
 * A profile's memory: one line per supported hardware class when it has several, each naming the
 * class and its accelerator memory with any pinned host memory, or that the scenario isn't
 * supported there; otherwise one value.
 */
function ProfileMemoryText({ pending, row }: { pending: boolean; row: ProfileRow }) {
  if (!pending && row.scenarioUnsupported) return "Unsupported";
  if (pending || row.hardwareClassEstimates.length + row.scenarioUnsupportedClasses.length < 2) {
    return <MemoryText memory={row.memory} pending={pending} />;
  }

  return (
    <>
      {row.hardwareClassEstimates.map(
        ({ acceleratorMemoryBytes, displayName, hardwareClassId, hostMemoryBytes }) => (
          <span className="block" key={hardwareClassId}>
            <span className="font-normal text-muted-foreground">{displayName}</span>{" "}
            {formatBytes(acceleratorMemoryBytes)}
            {hostMemoryBytes ? ` + ${formatBytes(hostMemoryBytes)} host RAM` : null}
          </span>
        ),
      )}
      {row.scenarioUnsupportedClasses.map(({ displayName, hardwareClassId }) => (
        <span className="block" key={hardwareClassId}>
          <span className="font-normal text-muted-foreground">{displayName}</span> unsupported for
          this scenario
        </span>
      ))}
    </>
  );
}

/** A memory value followed by its muted sub-label, or a placeholder while estimates load. */
function MemoryText({ memory, pending }: { memory: ProfileRow["memory"]; pending: boolean }) {
  if (pending) return <MemoryPlaceholder />;

  const { label, value } = formatMemory(memory);
  return (
    <>
      {value}
      {label ? <span className="font-normal text-muted-foreground"> {label}</span> : null}
    </>
  );
}

// The profile's advisory fit at the catalog's location, or why it can't serve the scenario.
function fitText({ fit, scenarioUnsupported }: ProfileRow): string {
  if (scenarioUnsupported) return scenarioUnsupported;
  if (!fit) return "";
  if (fit.fit === "unsupported") return fit.reason ?? `Unsupported on ${fit.location}`;

  const label = formatMemoryFit(fit.fit, fit.basis);
  return `${label.charAt(0).toUpperCase()}${label.slice(1)} on ${fit.location}`;
}

// A CPU-only profile runs without accelerators; a profile without requirements declares none.
function acceleratorLabel(profile: ProfileRow): string {
  if (!profile.acceleratorRequirements.length) {
    return profile.memory?.kind === "system-ram" ? "None" : "Not declared";
  }

  return profile.acceleratorRequirements
    .map(({ displayName, minimumMemoryBytesPerDevice }) =>
      minimumMemoryBytesPerDevice > 0
        ? `${displayName}, ${formatMemory({ kind: "minimum", max: minimumMemoryBytesPerDevice, min: minimumMemoryBytesPerDevice }).value} per device`
        : displayName,
    )
    .join("; ");
}
