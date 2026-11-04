/** The latest value of a read, and whether the most recent attempt failed. */
export interface ReadState<T> {
  failed: boolean;
  value?: T;
}

/** The read with its value mapped, keeping whether it failed. */
export function mapRead<T, U>(
  read: ReadState<T> | undefined,
  map: (value: T) => U,
): ReadState<U> | undefined {
  return (
    read && { failed: read.failed, value: read.value === undefined ? undefined : map(read.value) }
  );
}
