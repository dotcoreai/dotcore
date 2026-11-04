import { useEffect, useMemo, useRef, useState } from "react";

import type { ListModelProfileCompatibilityResponse } from "@/gen/dotcore/model/v1/service_pb";
import { createPollTimer } from "@/lib/poll-timer";

import type { ProfileEstimates } from "./catalog-model-row";
import { indexProfileCompatibility, type ProfileCompatibility } from "./profile-compatibility";
import { type SizingScenario, sizingScenarioKey } from "./sizing-scenario";

/** How often an open consumer re-reads compatibility; target observations expire after 60 s. */
export const PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS = 5_000;

/**
 * Reads the compatibility of one catalog model's profiles, or of every model's for `allModels`, on
 * every configured model target, sizing each profile for the scenario or, without one, its defaults.
 */
export type ProfileCompatibilityLoader = (
  modelId: string,
  scenario?: SizingScenario,
) => Promise<ListModelProfileCompatibilityResponse>;

/** The `modelId` that reads every catalog model's profiles at once. */
export const allModels = "";

export interface ProfileCompatibilityState {
  /**
   * The latest successful read for the model, kept while a later read fails or reads another
   * scenario. Its verdicts don't depend on the scenario.
   */
  compatibility?: ProfileCompatibility;
  /** Why the latest read failed, while no successful read is shown. */
  error?: string;
  /** The shown read's per-class memory estimates of each profile. */
  estimates: ProfileEstimateState;
  /** Whether the first read for the model, or the catalog, is still pending. */
  loading: boolean;
  /** The shown read when it sized the requested scenario, for its estimates and memory fit. */
  sizing?: ProfileCompatibility;
  /**
   * Whether the latest read of the requested scenario failed while an earlier read is shown, so its
   * estimates and memory fit are unavailable until a later read succeeds.
   */
  sizingFailed: boolean;
}

/**
 * The per-class memory estimates of each profile, with whether the requested scenario's are still
 * loading; until they land, the previous scenario's stay. A profile without estimates, including
 * after a failed read, falls back to its declared minimum.
 */
export interface ProfileEstimateState {
  estimates: ProfileEstimates;
  pending: boolean;
}

/**
 * Reads one model's profile compatibility, or the whole catalog's for `allModels`, sized for the
 * scenario, while `active`, then re-reads it on a fixed interval, pausing while the document is
 * hidden. Changing the model drops the previous result; changing the scenario reads again at once
 * and keeps the previous result until the new one lands.
 */
export function useProfileCompatibility(
  loader: ProfileCompatibilityLoader | undefined,
  modelId: string,
  active: boolean,
  scenario?: SizingScenario,
): ProfileCompatibilityState {
  const [result, setResult] = useState<{
    compatibility?: ProfileCompatibility;
    error?: string;
    modelId: string;
    /** The scenario the result answers for. */
    scenarioKey: string;
    /** Whether the compatibility was sized for that scenario, false after a failed read of it. */
    sized: boolean;
  }>();
  const enabled = Boolean(loader && active);
  const scenarioKey = sizingScenarioKey(scenario);
  // Reads follow the scenario's key, so a new object with the same values doesn't read again.
  const scenarioRef = useRef(scenario);

  useEffect(() => {
    scenarioRef.current = scenario;
  });

  useEffect(() => {
    if (!loader || !active) return;

    let current = true;
    // Becoming visible starts a read even while one is in flight, so only the latest one counts.
    let generation = 0;
    const pollTimer = createPollTimer(() => void read(), { onVisible: () => void read() });
    const read = async () => {
      const own = ++generation;
      const latest = () => current && own === generation;
      try {
        const sized = scenarioRef.current;
        const compatibility = indexProfileCompatibility(
          await (sized ? loader(modelId, sized) : loader(modelId)),
        );
        if (latest()) setResult({ compatibility, modelId, scenarioKey, sized: true });
      } catch (error) {
        if (latest()) {
          setResult((previous) =>
            previous?.modelId === modelId && previous.compatibility
              ? previous.scenarioKey === scenarioKey
                ? previous
                : { ...previous, scenarioKey, sized: false }
              : { error: compatibilityLoadError(error), modelId, scenarioKey, sized: false },
          );
        }
      }
      if (latest()) pollTimer.arm(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    };

    if (document.visibilityState === "visible") void read();

    return () => {
      current = false;
      pollTimer.dispose();
    };
  }, [active, loader, modelId, scenarioKey]);

  const shown = result?.modelId === modelId ? result : undefined;
  const loading = enabled && !shown;
  const current = shown?.scenarioKey === scenarioKey;
  const sizing = current && shown.sized ? shown.compatibility : undefined;
  // A failed read of the current scenario leaves no estimates rather than another scenario's.
  const shownEstimates = current && !shown.sized ? undefined : shown?.compatibility?.estimates;
  const estimates = useMemo(
    () => ({ estimates: shownEstimates ?? emptyEstimates, pending: enabled && !current }),
    [current, enabled, shownEstimates],
  );
  return {
    compatibility: shown?.compatibility,
    error: shown?.error,
    estimates,
    loading,
    sizing,
    sizingFailed: Boolean(current && !shown.sized && shown.compatibility),
  };
}

const emptyEstimates: ProfileEstimates = new Map();

function compatibilityLoadError(error: unknown): string {
  return error instanceof Error && error.message ? error.message : "The read failed.";
}
