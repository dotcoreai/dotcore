import { useEffect, useSyncExternalStore } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import type { ReadState } from "@/lib/read-state";
import { useOptionalManagerSession } from "@/resource-watch/resource-watch";

import { metricDescriptorsFor } from "./metric-descriptors";
import type { MetricRange } from "./metric-ranges";

/**
 * The ranges the store serves, from its advertised resolutions; absent until first read. Every
 * panel of a client shares one read, repeated only for a new store incarnation or manager session.
 */
export function useMetricRanges(client: MetricsClient): ReadState<MetricRange[]> | undefined {
  const store = metricDescriptorsFor(client);
  const { generation } = useOptionalManagerSession();

  useEffect(() => store.observeManagerGeneration(generation), [generation, store]);

  return useSyncExternalStore(store.subscribe, store.snapshot);
}
