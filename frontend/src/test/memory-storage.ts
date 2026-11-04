import type { StorageLike } from "@/lib/browser-storage";

/** Browser storage held in a map, so each test starts from its own empty storage. */
export class MemoryStorage implements StorageLike {
  readonly values = new Map<string, string>();

  getItem(key: string) {
    return this.values.get(key) ?? null;
  }

  removeItem(key: string) {
    this.values.delete(key);
  }

  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

/** Browser storage that fails every call, as a browser denying storage does. */
export class FailingStorage implements StorageLike {
  getItem(): string | null {
    throw new Error("storage is unavailable");
  }

  removeItem() {
    throw new Error("storage is unavailable");
  }

  setItem() {
    throw new Error("storage is unavailable");
  }
}
