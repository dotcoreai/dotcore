import { useCallback, useState } from "react";

import { useWatchedExecutionTargets } from "@/app/resource-watch";
import type {
  ConfiguredExecutionTarget,
  ListConfiguredExecutionTargetsResponse,
} from "@/gen/dotcore/site/v1/service_pb";

export type ConfiguredExecutionTargetLoader = () => Promise<{
  executionTargets: ConfiguredExecutionTarget[];
}>;

interface ConfiguredExecutionTargetState {
  current: boolean;
  error?: string;
  loading: boolean;
  executionTargets: ConfiguredExecutionTarget[];
}

/** Optional settings of `useConfiguredExecutionTargets`. */
export interface ConfiguredExecutionTargetOptions {
  /** Whether the initial targets are already current, so no loading state shows. */
  initialCurrent?: boolean;
  /** The targets shown before the watch delivers them. */
  initialExecutionTargets?: readonly ConfiguredExecutionTarget[];
}

/**
 * Shows the site's configured execution targets from the console's resource watch while `active`
 * and a `loader` exists. A failed watch keeps the last targets and reports why. `refresh` reads the
 * targets once through the loader, for a check against fresh state right before a submit, and shows
 * what it read until the watch delivers newer targets.
 */
export function useConfiguredExecutionTargets(
  loader: ConfiguredExecutionTargetLoader | undefined,
  active: boolean,
  {
    initialCurrent = false,
    initialExecutionTargets = noTargets,
  }: ConfiguredExecutionTargetOptions = {},
): ConfiguredExecutionTargetState & {
  refresh: () => Promise<ConfiguredExecutionTarget[] | undefined>;
} {
  const watched = useWatchedExecutionTargets({ enabled: active && Boolean(loader) });
  const [fresh, setFresh] = useState<FreshRead>();

  const refresh = useCallback(async () => {
    if (!loader) return undefined;

    const after = watched?.value;
    try {
      const result = await loader();
      setFresh({ after, executionTargets: result.executionTargets });
      return result.executionTargets;
    } catch (error) {
      setFresh({ after, error: configuredExecutionTargetLoadError(error) });
      return undefined;
    }
  }, [loader, watched?.value]);

  // A fresh read is shown only until the watch delivers newer targets.
  const shownFresh = fresh?.after === watched?.value ? fresh : undefined;
  const error =
    shownFresh?.error ?? (watched?.failed ? configuredExecutionTargetLoadError() : undefined);
  const executionTargets =
    shownFresh?.executionTargets ??
    watched?.value?.executionTargets ??
    (initialExecutionTargets as ConfiguredExecutionTarget[]);
  if (!shownFresh?.executionTargets && !watched?.value) {
    return {
      current: initialCurrent,
      error,
      executionTargets,
      loading: Boolean(loader && active && !initialCurrent && !error),
      refresh,
    };
  }

  return {
    current: !watched?.failed && !shownFresh?.error,
    error,
    executionTargets,
    loading: false,
    refresh,
  };
}

/** What the last fresh read answered, shown over the watched targets it was read after. */
interface FreshRead {
  after: ListConfiguredExecutionTargetsResponse | undefined;
  error?: string;
  executionTargets?: ConfiguredExecutionTarget[];
}

const noTargets: readonly ConfiguredExecutionTarget[] = [];

export function configuredExecutionTargetLoadError(error?: unknown): string {
  const detail = error instanceof Error && error.message ? `: ${error.message}` : "";
  return `Configured execution targets could not be loaded from the site service${detail}. Try again before deploying.`;
}
