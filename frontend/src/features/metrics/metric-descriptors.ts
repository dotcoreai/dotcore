import type { MetricsClient } from "@/api/metrics-client";
import { DEFAULT_POLL_RETRY_DELAYS_MS } from "@/lib/poll-timer";
import type { ReadState } from "@/lib/read-state";

import { type MetricRange, metricRangesFromResolutions } from "./metric-ranges";

/**
 * The metric store's descriptors for one metrics client, read once and shared by every panel.
 * Descriptors only change with the store, so the read repeats only when a metric watch reports a
 * new store incarnation or the resource watch a new manager session; a failed read retries with
 * backoff while a panel is mounted. The last ranges stay in view while a new read is in flight.
 */
export class MetricDescriptorsStore {
  private readonly client: MetricsClient;
  private readonly listeners = new Set<() => void>();
  private current: ReadState<MetricRange[]> | undefined;
  private managerGeneration = 0;
  private incarnationId: string | undefined;
  private stale = true;
  private failureCount = 0;
  private controller: AbortController | undefined;
  private retryTimer: ReturnType<typeof setTimeout> | undefined;

  constructor(client: MetricsClient) {
    this.client = client;
  }

  readonly subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    this.ensureRead();

    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size === 0) this.suspend();
    };
  };

  readonly snapshot = (): ReadState<MetricRange[]> | undefined => this.current;

  /** A different manager session than seen so far replaces the descriptors with a new read. */
  observeManagerGeneration(generation: number): void {
    if (generation === this.managerGeneration) return;

    this.managerGeneration = generation;
    // The restarted store reports a new incarnation, which this read already accounts for.
    this.incarnationId = undefined;
    this.invalidate();
  }

  /** A store incarnation other than the one seen before replaces the descriptors. */
  observeIncarnation(id: string): void {
    const previous = this.incarnationId;
    this.incarnationId = id;
    if (previous !== undefined && previous !== id) this.invalidate();
  }

  private invalidate() {
    this.stale = true;
    this.failureCount = 0;
    this.suspend();
    this.ensureRead();
  }

  /** Cancels the read in flight and any retry; the next demand reads again. */
  private suspend() {
    this.controller?.abort();
    this.controller = undefined;
    clearTimeout(this.retryTimer);
    this.retryTimer = undefined;
  }

  private ensureRead() {
    if (!this.stale || this.listeners.size === 0 || this.controller || this.retryTimer) return;

    void this.read();
  }

  private async read() {
    const controller = new AbortController();
    this.controller = controller;
    try {
      const response = await this.client.metrics.listMetricDescriptors(
        {},
        { signal: controller.signal },
      );
      if (controller.signal.aborted) return;

      this.stale = false;
      this.failureCount = 0;
      this.current = { failed: false, value: metricRangesFromResolutions(response.resolutions) };
    } catch {
      if (controller.signal.aborted) return;

      this.current = { failed: true, value: this.current?.value };
      const delays = DEFAULT_POLL_RETRY_DELAYS_MS;
      const delay = delays[Math.min(this.failureCount, delays.length - 1)];
      this.failureCount += 1;
      this.retryTimer = setTimeout(() => {
        this.retryTimer = undefined;
        this.ensureRead();
      }, delay);
    }
    this.controller = undefined;
    this.listeners.forEach((listener) => listener());
  }
}

const stores = new WeakMap<MetricsClient, MetricDescriptorsStore>();

/** The descriptors store shared by every panel reading through `client`. */
export function metricDescriptorsFor(client: MetricsClient): MetricDescriptorsStore {
  let store = stores.get(client);
  if (!store) {
    store = new MetricDescriptorsStore(client);
    stores.set(client, store);
  }

  return store;
}
