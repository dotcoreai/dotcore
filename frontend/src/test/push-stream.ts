/**
 * A push-driven async iterable for faking a Connect server stream in a test: a test pushes the
 * messages the server would send, `end` closes it, and an abort signal rejects a wait that the
 * stream would otherwise hold open.
 */
export class PushStream<T> {
  private readonly queue: T[] = [];
  private waiting?: {
    reject: (reason: unknown) => void;
    resolve: (result: IteratorResult<T>) => void;
  };
  private ended = false;

  constructor(private readonly signal?: AbortSignal) {
    signal?.addEventListener("abort", () => this.waiting?.reject(new Error("aborted")));
  }

  push(message: T) {
    const waiting = this.waiting;
    if (waiting) {
      this.waiting = undefined;
      waiting.resolve({ done: false, value: message });
    } else {
      this.queue.push(message);
    }
  }

  end() {
    this.ended = true;
    this.waiting?.resolve({ done: true, value: undefined });
    this.waiting = undefined;
  }

  [Symbol.asyncIterator](): AsyncIterator<T> {
    return {
      next: () => {
        if (this.signal?.aborted) return Promise.reject(new Error("aborted"));
        const queued = this.queue.shift();
        if (queued) return Promise.resolve({ done: false, value: queued });
        if (this.ended) return Promise.resolve({ done: true, value: undefined });
        return new Promise((resolve, reject) => {
          this.waiting = { reject, resolve };
        });
      },
    };
  }
}
