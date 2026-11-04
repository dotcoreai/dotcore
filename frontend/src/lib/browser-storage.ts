/** The part of the Web Storage API the console uses, so tests can pass a fake. */
export interface StorageLike {
  getItem(key: string): string | null;
  removeItem(key: string): void;
  setItem(key: string, value: string): void;
}

/** The browser's local storage, or nothing outside a browser or when the browser denies it. */
export function getBrowserStorage(): StorageLike | undefined {
  if (typeof window === "undefined") {
    return undefined;
  }

  try {
    return window.localStorage;
  } catch {
    return undefined;
  }
}
