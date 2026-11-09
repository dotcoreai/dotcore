import type { DataTableField } from "@/components/data-table/data-table-model";
import {
  executionTargetKey,
  executionTargetPresentation,
  placedExecutionTargetName,
} from "@/domain/execution-target";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";
import { formatCount } from "@/lib/format";
import type { StatusProjection } from "@/lib/status-projection";

import { profileEstimateKey, type ProfileRow, type ProfileSizingView } from "./catalog-model-row";
import { type ProfileStatus, type RunsOnField, runsOnField } from "./catalog-profile-fields";
import {
  type CompatibilityVerdict,
  defaultMemoryBasis,
  inScope,
  type MemoryBasis,
  type MemoryFit,
  type ProfileCompatibility,
  profileScopeClasses,
  profileScopeFit,
  profileScopeStartingTotalContext,
  profileScopeVerdict,
  profileTargetVerdict,
  profileTargetVerdicts,
  type RunsOnScope,
  runsOnScopeKey,
  verdictFit,
} from "./profile-compatibility";
import { machineName, profileCompatibilityStatus } from "./profile-compatibility-copy";
import type { ProfileNoneRun } from "./profile-table";
import type { ProfileAvailability } from "./profile-table-model";
import type { SizingScenario } from "./sizing-scenario";
import type { ProfileCompatibilityState } from "./use-profile-compatibility";

/**
 * The catalog model panel's compatibility read for its model, shared by its profile list and its
 * profile screens, with the site's configured execution targets that name machines, the panel's
 * "Sized for" the read is sized from, and the memory basis its fits are judged on. Estimates reach
 * the panel through its profile rows instead.
 */
export interface CatalogProfileCompatibility extends Omit<
  ProfileCompatibilityState,
  "estimates" | "sizing"
> {
  basis?: MemoryBasis;
  /** Changes the panel's memory basis; the read already holds both fits. */
  onBasisChange?: (basis: MemoryBasis) => void;
  /** Changes the panel's "Sized for", which reads the model again; without it, the chip has none. */
  onScenarioChange?: (scenario: SizingScenario | undefined) => void;
  scenario?: SizingScenario;
  /** The memory basis the panel opened with, which its Reset restores. */
  startBasis?: MemoryBasis;
  /** The "Sized for" the panel opened with, which its Reset restores. */
  startScenario?: SizingScenario;
  targets: readonly ConfiguredExecutionTarget[];
}

/** One evaluated target on a profile's page: where it is, and whether the profile runs there. */
export interface ProfileMachine {
  /** The target's platform and GPU count, as far as configured. */
  hardware: string;
  key: string;
  name: string;
  /** Why the profile can't run or is unknown there, or its advisories when it runs. */
  note?: ProfileStatus;
  status: StatusProjection;
  verdict: CompatibilityVerdict;
}

/** The Runs on filter of the catalog table and of the profile tables of a model. */
export interface CatalogRunsOn {
  /**
   * The Runs on field, whose values per profile are the scopes where it runs and its estimate
   * doesn't exceed memory.
   */
  field: DataTableField<ProfileRow, RunsOnField>;
  /** Why the memory fit hides a profile that runs in the chip values' scopes. */
  hiddenReason: (profile: ProfileRow, values: readonly string[]) => CatalogHiddenReason | undefined;
  /** The names of the chip values' scopes within a sentence, such as "this site". */
  scopeName: (values: readonly string[]) => string;
  scopes: (values: readonly string[]) => RunsOnScope[];
  /** The Runs on field without the memory fit, whose values are the scopes where a profile runs. */
  supportField: DataTableField<ProfileRow, RunsOnField>;
}

/**
 * Why Runs on hides a profile that runs in its scopes: its estimate exceeds memory on every machine
 * that runs it, or it can't serve the sizing scenario on any of them.
 */
export type CatalogHiddenReason = "exceeds" | "unsupported";

/**
 * The one Runs on filter of the catalog table, the catalog model panel and the launch dialog: this
 * site, then each machine with an evaluated target, over profiles that carry their model id. Each
 * host passes the read it shows. A profile runs in a scope when a target there runs it and, with
 * `fit`, its memory fit there on the memory basis, under the read's sizing scenario, fits or is
 * unknown. Undefined when the read evaluated no target, so a site without model execution targets
 * lists every profile.
 */
export function catalogRunsOn(
  compatibility: ProfileCompatibility,
  targets: readonly ConfiguredExecutionTarget[],
  { basis = defaultMemoryBasis, fit = true }: { basis?: MemoryBasis; fit?: boolean } = {},
): CatalogRunsOn | undefined {
  if (!compatibility.targets.length) return undefined;

  const options = scopeOptions(compatibility, targets);
  const { scopeName, selected } = scopeNames(options);
  const runs = (profile: ProfileRow, scope: RunsOnScope) =>
    profileScopeVerdict(compatibility, profile.modelId, profile.profileId, scope)?.status ===
    "runs";
  const scopeFit = (profile: ProfileRow, scope: RunsOnScope) =>
    profileScopeFit(compatibility, profile.modelId, profile.profileId, scope, basis)?.fit;

  return {
    field: runsOnField(
      options,
      (profile, scope) => runs(profile, scope) && (!fit || keepsFit(scopeFit(profile, scope))),
    ),
    hiddenReason: (profile, values) => {
      if (!fit) return undefined;

      const fits = selected(values)
        .filter(({ scope }) => runs(profile, scope))
        .map(({ scope }) => scopeFit(profile, scope));
      if (!fits.length || fits.some(keepsFit)) return undefined;
      return fits.every((candidate) => candidate === "unsupported") ? "unsupported" : "exceeds";
    },
    scopeName,
    scopes: (values) => selected(values).map(({ scope }) => scope),
    supportField: runsOnField(options, runs),
  };
}

/**
 * What a profile table of one model says when none of its profiles runs in the Runs on chip's
 * scopes: why the profiles that can't run there can't. Profiles that run but that the memory fit
 * hides are counted in the table's footnote instead.
 */
export function profileNoneRun(
  compatibility: ProfileCompatibility,
  runsOn: CatalogRunsOn,
  model: { modelId: string },
  profiles: readonly ProfileRow[],
  targets: readonly ConfiguredExecutionTarget[],
): (values: readonly string[]) => ProfileNoneRun {
  return (values) => {
    const reasons = profiles.flatMap((profile) =>
      runsOn.scopes(values).flatMap((scope) => {
        const verdict = profileScopeVerdict(compatibility, model.modelId, profile.profileId, scope);
        // A profile that runs there is hidden by its fit, which the footnote counts.
        if (verdict?.status === "runs") return [];
        return profileCompatibilityStatus(verdict, { profile, targets })?.text ?? [];
      }),
    );
    return { reasons: [...new Set(reasons)], title: "No profile runs here" };
  };
}

/** How many profiles Runs on hides for their memory fit, such as "2 hidden"; undefined for none. */
export function hiddenByFitNote(hidden: Record<CatalogHiddenReason, number>): string | undefined {
  const count = hidden.exceeds + hidden.unsupported;
  return count ? `${count} hidden` : undefined;
}

/** Where the catalog sizes and fits profiles, with its name within a sentence. */
export interface CatalogLocation {
  name: string;
  /** The site alone, or the machines the Runs on chip selects. */
  scopes: RunsOnScope[];
}

/**
 * The catalog's location: the machines the Runs on chip selects, otherwise the site, including
 * without the chip or when the chip selects the site.
 */
export function catalogLocation(
  runsOn: CatalogRunsOn | undefined,
  chipValues: readonly string[],
): CatalogLocation {
  const scopes = runsOn?.scopes(chipValues) ?? [];
  return scopes.length && scopes.every(({ kind }) => kind === "node")
    ? { name: runsOn!.scopeName(chipValues), scopes }
    : { name: "this site", scopes: [{ kind: "site" }] };
}

/**
 * What the compatibility read says about each profile at a location: the hardware classes its
 * targets there select and the total context it is shown at, and, from the read that sized the
 * current scenario, its memory fit there on the memory basis (it fits when one of them fits) and
 * why it can't serve the scenario at all.
 */
export function catalogSizingView(
  compatibility: ProfileCompatibility,
  sizing: ProfileCompatibility | undefined,
  location: CatalogLocation,
  scenario: SizingScenario | undefined,
  basis: MemoryBasis,
): ProfileSizingView {
  return {
    basis,
    classIds: (modelId, profileId) => [
      ...new Set(
        location.scopes.flatMap((scope) =>
          profileScopeClasses(compatibility, modelId, profileId, scope),
        ),
      ),
    ],
    fit: (modelId, profileId) => {
      const scopeFit =
        sizing && profileScopeFit(sizing, modelId, profileId, location.scopes, basis);
      return (
        scopeFit && {
          fit: scopeFit.fit,
          unsupportedReason: scopeFit.verdict.unsupportedReason,
        }
      );
    },
    location: location.name,
    totalContext: (modelId, profileId) =>
      profileTotalContext(compatibility, modelId, profileId, location.scopes, scenario),
    unsupportedClassIds: (modelId, profileId) =>
      sizing?.unsupportedClasses.get(profileEstimateKey(modelId, profileId)) ?? [],
    unsupportedReason: (modelId, profileId) =>
      sizing?.unsupported.get(profileEstimateKey(modelId, profileId)),
  };
}

/**
 * The total context a profile is shown at in the scopes: the scenario's total context or, without
 * a scenario, the one its launch starts from on the machines there that run it, as the launch
 * shows it. The value is shown only; filters judge the read's memory fit.
 */
export function profileTotalContext(
  compatibility: ProfileCompatibility | undefined,
  modelId: string,
  profileId: string,
  scopes: readonly RunsOnScope[],
  scenario: SizingScenario | undefined,
): number | undefined {
  if (scenario) return scenario.totalContextTokens;
  return (
    compatibility && profileScopeStartingTotalContext(compatibility, modelId, profileId, scopes)
  );
}

/**
 * The deployable configured model execution targets within the scopes that can run a profile, in
 * the configured order.
 */
export function profileLaunchTargets(
  compatibility: ProfileCompatibility,
  modelId: string,
  profileId: string,
  targets: readonly ConfiguredExecutionTarget[],
  scopes: readonly RunsOnScope[],
): ConfiguredExecutionTarget[] {
  return targets.filter(
    (target) =>
      scopes.some((scope) => inScope(target, scope)) &&
      executionTargetPresentation(target).deployable &&
      profileTargetVerdict(compatibility, modelId, profileId, target)?.status === "runs",
  );
}

/**
 * Whether each profile can be launched within the scopes. One that can't run there is
 * unselectable with why, and so is one that runs only on machines that can't deploy right now; one
 * that runs shows its advisories, else an advisory when its estimate exceeds memory, on the memory
 * basis, on every machine there that runs it. The fit never makes a profile unselectable, since the
 * launch can still change its sizing.
 */
export function launchProfileAvailability(
  compatibility: ProfileCompatibility,
  modelId: string,
  targets: readonly ConfiguredExecutionTarget[],
  scopes: readonly RunsOnScope[],
  basis: MemoryBasis,
): (profile: ProfileRow) => ProfileAvailability | undefined {
  return (profile) => {
    const verdict = scopesVerdict(compatibility, modelId, profile.profileId, scopes);
    if (!verdict) return undefined;

    const status = profileCompatibilityStatus(verdict, { profile, targets });
    if (verdict.status !== "runs") return { selectable: false, status };
    if (profileLaunchTargets(compatibility, modelId, profile.profileId, targets, scopes).length)
      return {
        selectable: true,
        status:
          status ??
          exceedsStatus(compatibility, modelId, profile.profileId, targets, scopes, basis),
      };

    const machines = new Set(
      scopes.flatMap((scope) =>
        profileTargetVerdicts(compatibility, modelId, profile.profileId, scope)
          .filter((candidate) => candidate.status === "runs")
          .map((candidate) => machineName(candidate.target, targets)),
      ),
    );
    return {
      selectable: false,
      status: {
        text: `${[...machines].join(" or ")} can't deploy models right now`,
        tone: "unknown",
      },
    };
  };
}

/**
 * Whether each profile runs on the site. One that doesn't is listed last with why; one that runs
 * shows no status line, since its page lists each machine's advisories.
 */
export function siteProfileAvailability(
  compatibility: ProfileCompatibility,
  modelId: string,
  targets: readonly ConfiguredExecutionTarget[],
): (profile: ProfileRow) => ProfileAvailability | undefined {
  return (profile) => {
    const verdict = profileScopeVerdict(compatibility, modelId, profile.profileId, {
      kind: "site",
    });
    if (!verdict) return undefined;

    const runs = verdict.status === "runs";
    return {
      selectable: runs,
      status: runs ? undefined : profileCompatibilityStatus(verdict, { profile, targets }),
    };
  };
}

/**
 * Every evaluated target with the profile's status there: those that run it first, then those
 * with advisories, unknown, and can't run, each group in the read's order.
 */
export function profileMachines(
  compatibility: ProfileCompatibility,
  modelId: string,
  profile: ProfileRow,
  targets: readonly ConfiguredExecutionTarget[],
): ProfileMachine[] {
  const machines = profileTargetVerdicts(compatibility, modelId, profile.profileId).map(
    (verdict) => {
      const key = executionTargetKey(verdict.target);
      const configured = targets.find((target) => executionTargetKey(target) === key);
      const note = profileCompatibilityStatus(verdict, { profile, targets });
      return {
        hardware: configured ? targetHardware(configured) : "",
        key,
        name: placedExecutionTargetName(verdict.target, targets),
        note,
        status: machineStatus(verdict, note),
        verdict,
      };
    },
  );

  // A stable sort keeps the read's order within each status.
  return machines.sort((left, right) => machineRank(left) - machineRank(right));
}

/**
 * Why a profile can't be deployed from its page: no evaluated target runs it. Undefined while
 * nothing is known, when the site has no model execution targets, or when a target runs it, so a
 * failed read never blocks deploying.
 */
export function profileDeployBlockedReason(
  compatibility: ProfileCompatibility | undefined,
  modelId: string,
  profileId: string,
): string | undefined {
  if (!compatibility) return undefined;

  const verdicts = profileTargetVerdicts(compatibility, modelId, profileId);
  if (!verdicts.length || verdicts.some(({ status }) => status === "runs")) return undefined;
  return verdicts.some(({ status }) => status === "unknown")
    ? "No machine on this site can run this profile right now"
    : "No machine on this site can run this profile";
}

/** A Runs on scope a site offers, with its chip label and its name within a sentence. */
interface ScopeOption {
  label: string;
  name: string;
  scope: RunsOnScope;
}

// this site, then each machine with an evaluated target, in the read's order.
function scopeOptions(
  compatibility: ProfileCompatibility,
  targets: readonly ConfiguredExecutionTarget[],
): ScopeOption[] {
  const machines = [
    ...new Map(compatibility.targets.map((target) => [target.siteNodeId, target])).values(),
  ];
  return [
    { label: "this site", name: "this site", scope: { kind: "site" } },
    ...machines.map((target) => {
      const name = machineName(target, targets);
      return { label: name, name, scope: { kind: "node", siteNodeId: target.siteNodeId } as const };
    }),
  ];
}

function scopeNames(options: readonly ScopeOption[]) {
  const selected = (values: readonly string[]) =>
    options.filter(({ scope }) => values.includes(runsOnScopeKey(scope)));
  const scopeName = (values: readonly string[]) =>
    selected(values)
      .map(({ name }) => name)
      .join(" or ");
  return { scopeName, selected };
}

// "Exceeds free memory on gpu1" when the profile exceeds memory on the basis on the targets it can
// launch on in the scopes and fits none of them, naming the machines it exceeds.
function exceedsStatus(
  compatibility: ProfileCompatibility,
  modelId: string,
  profileId: string,
  targets: readonly ConfiguredExecutionTarget[],
  scopes: readonly RunsOnScope[],
  basis: MemoryBasis,
): ProfileStatus | undefined {
  const fits = profileLaunchTargets(compatibility, modelId, profileId, targets, scopes).map(
    (target) => ({
      fit: verdictFit(profileTargetVerdict(compatibility, modelId, profileId, target), basis),
      target,
    }),
  );
  if (fits.some(({ fit }) => keepsFit(fit))) return undefined;

  const machines = new Set(
    fits.filter(({ fit }) => fit === "exceeds").map(({ target }) => machineName(target, targets)),
  );
  return machines.size
    ? {
        text: `Exceeds ${basis === "free" ? "free memory" : "memory"} on ${[...machines].join(" or ")}`,
        tone: "advisory",
      }
    : undefined;
}

// A profile without a fit, or whose fit is unknown, stays: missing data never hides a profile.
function keepsFit(fit: MemoryFit | undefined): boolean {
  return fit === undefined || fit === "fits" || fit === "unknown";
}

// The best verdict over several scopes, ranked as one scope ranks its targets.
function scopesVerdict(
  compatibility: ProfileCompatibility,
  modelId: string,
  profileId: string,
  scopes: readonly RunsOnScope[],
): CompatibilityVerdict | undefined {
  const verdicts = scopes.flatMap(
    (scope) => profileScopeVerdict(compatibility, modelId, profileId, scope) ?? [],
  );
  return verdicts.sort((left, right) => verdictRank(left) - verdictRank(right))[0];
}

function verdictRank({ advisories, status }: CompatibilityVerdict): number {
  switch (status) {
    case "runs":
      return advisories.length ? 1 : 0;
    case "unknown":
      return 2;
    case "cannot-run":
      return 3;
  }
}

function targetHardware(target: ConfiguredExecutionTarget): string {
  const devices = target.acceleratorDeviceIds.length;
  return [target.platformLabel, devices ? formatCount(devices, "GPU") : undefined]
    .filter(Boolean)
    .join(" · ");
}

function machineStatus(
  verdict: CompatibilityVerdict,
  note: ProfileStatus | undefined,
): StatusProjection {
  switch (verdict.status) {
    case "runs":
      return { label: "Runs", tone: note ? "warning" : "success" };
    case "unknown":
      return { label: "Unknown", tone: "neutral" };
    case "cannot-run":
      return { label: "Can't run", tone: "neutral" };
  }
}

function machineRank({ status, verdict }: ProfileMachine): number {
  switch (verdict.status) {
    case "runs":
      return status.tone === "success" ? 0 : 1;
    case "unknown":
      return 2;
    case "cannot-run":
      return 3;
  }
}
