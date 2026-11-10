import { Code, ConnectError } from "@connectrpc/connect";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { runStreamLoop } from "@/lib/stream-loop";
import { productionTiming, setTiming } from "@/lib/timing";

beforeEach(() => {
  setTiming(productionTiming);
  vi.useFakeTimers();
});

afterEach(() => {
  setVisibility("visible");
  vi.useRealTimers();
});

describe("runStreamLoop", () => {
  it("delivers messages and reopens with growing backoff after failures", async () => {
    const harness = new Harness();
    const stop = harness.run();
    await flush();

    harness.streams[0].push("a");
    await flush();
    expect(harness.messages).toEqual(["a"]);

    harness.streams[0].fail(new ConnectError("down", Code.Unavailable));
    await flush();
    expect(harness.failures).toEqual([{ code: Code.Unavailable, terminal: false }]);

    await advance(999);
    expect(harness.streams).toHaveLength(1);
    await advance(1);
    expect(harness.streams).toHaveLength(2);

    harness.streams[1].fail(new ConnectError("down", Code.Unavailable));
    await flush();
    await advance(1_999);
    expect(harness.streams).toHaveLength(2);
    await advance(1);
    expect(harness.streams).toHaveLength(3);
    stop();
  });

  it("resets the backoff after a live stream stays open past the longest retry delay", async () => {
    const harness = new Harness();
    const stop = harness.run();
    await flush();
    harness.streams[0].fail(new ConnectError("down", Code.Unavailable));
    await flush();
    await advance(1_000);
    harness.streams[1].fail(new ConnectError("down", Code.Unavailable));
    await flush();
    await advance(2_000);
    expect(harness.streams).toHaveLength(3);

    harness.streams[2].push("live");
    await advance(5_000);
    harness.streams[2].push("still live");
    await flush();
    harness.streams[2].fail(new ConnectError("down", Code.Unavailable));
    await flush();

    await advance(1_000);
    expect(harness.streams).toHaveLength(4);
    stop();
  });

  it("keeps the backoff when the stream drops before it is live or before it is healthy", async () => {
    const harness = new Harness();
    const stop = harness.run();
    await flush();
    harness.streams[0].fail(new ConnectError("down", Code.Unavailable));
    await flush();
    await advance(1_000);

    harness.streams[1].push("live");
    await advance(1_000);
    harness.streams[1].push("early");
    harness.streams[1].fail(new ConnectError("down", Code.Unavailable));
    await flush();

    await advance(1_999);
    expect(harness.streams).toHaveLength(2);
    await advance(1);
    expect(harness.streams).toHaveLength(3);
    stop();
  });

  it("closes the stream while hidden and reopens it when visible", async () => {
    const harness = new Harness();
    const stop = harness.run();
    await flush();
    expect(harness.streams).toHaveLength(1);

    setVisibility("hidden");
    expect(harness.streams[0].signal.aborted).toBe(true);
    await advance(10_000);
    expect(harness.streams).toHaveLength(1);
    expect(harness.failures).toEqual([]);

    setVisibility("visible");
    await flush();
    expect(harness.streams).toHaveLength(2);
    expect(harness.streams[1].signal.aborted).toBe(false);
    stop();
  });

  it("stops on a terminal code without retrying and reports it once", async () => {
    const harness = new Harness();
    const stop = harness.run();
    await flush();

    harness.streams[0].fail(new ConnectError("bad", Code.InvalidArgument));
    await flush();
    await advance(30_000);

    expect(harness.streams).toHaveLength(1);
    expect(harness.failures).toEqual([{ code: Code.InvalidArgument, terminal: true }]);
    stop();
  });

  it("honors the caller's terminal codes", async () => {
    const harness = new Harness();
    const stop = harness.run({ terminalCodes: [Code.InvalidArgument] });
    await flush();

    harness.streams[0].fail(new ConnectError("gone", Code.NotFound));
    await flush();
    await advance(1_000);

    expect(harness.streams).toHaveLength(2);
    expect(harness.failures).toEqual([{ code: Code.NotFound, terminal: false }]);
    stop();
  });

  it("retries a stream that ends without an error", async () => {
    const harness = new Harness();
    const stop = harness.run();
    await flush();

    harness.streams[0].end();
    await flush();
    expect(harness.failures).toEqual([{ code: undefined, terminal: false }]);
    await advance(1_000);

    expect(harness.streams).toHaveLength(2);
    stop();
  });

  it("aborts the stream and stays quiet once stopped", async () => {
    const harness = new Harness();
    const stop = harness.run();
    await flush();

    stop();
    harness.streams[0].fail(new ConnectError("down", Code.Unavailable));
    await flush();
    await advance(10_000);

    expect(harness.streams[0].signal.aborted).toBe(true);
    expect(harness.streams).toHaveLength(1);
    expect(harness.failures).toEqual([]);
  });
});

class FakeStream implements AsyncIterable<string> {
  private queue: Array<{ value: string } | { error: unknown } | { done: true }> = [];
  private waiting: (() => void) | undefined;

  constructor(readonly signal: AbortSignal) {}

  push(value: string) {
    this.enqueue({ value });
  }

  fail(error: unknown) {
    this.enqueue({ error });
  }

  end() {
    this.enqueue({ done: true });
  }

  async *[Symbol.asyncIterator]() {
    for (;;) {
      while (this.queue.length === 0) {
        await new Promise<void>((resolve) => {
          this.waiting = resolve;
        });
      }
      const item = this.queue.shift()!;
      if ("error" in item) throw item.error;
      if ("done" in item) return;
      yield item.value;
    }
  }

  private enqueue(item: FakeStream["queue"][number]) {
    this.queue.push(item);
    this.waiting?.();
  }
}

class Harness {
  streams: FakeStream[] = [];
  messages: string[] = [];
  failures: Array<{ code: Code | undefined; terminal: boolean }> = [];

  run(extra: { terminalCodes?: readonly Code[] } = {}) {
    return runStreamLoop<string>({
      ...extra,
      onFailure: (error, terminal) => this.failures.push({ code: error?.code, terminal }),
      onMessage: (message, markLive) => {
        this.messages.push(message);
        if (message.startsWith("live")) markLive();
      },
      open: (signal) => {
        const stream = new FakeStream(signal);
        this.streams.push(stream);
        return stream;
      },
    });
  }
}

function setVisibility(visibilityState: DocumentVisibilityState) {
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: visibilityState,
  });
  document.dispatchEvent(new Event("visibilitychange"));
}

async function flush() {
  await vi.advanceTimersByTimeAsync(0);
}

async function advance(ms: number) {
  await vi.advanceTimersByTimeAsync(ms);
}
