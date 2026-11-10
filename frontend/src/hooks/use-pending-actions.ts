import { useRef, useState } from "react";

/**
 * Tracks which async actions are running for which resource. `start` refuses an action that is
 * already running for the same resource, and `isRunning` answers synchronously for callbacks.
 * Render code reads `isPending` and `pendingAction`, which follow state.
 */
export function usePendingActions<Action extends string>() {
  const [running, setRunning] = useState<ReadonlySet<string>>(() => new Set());
  const runningRef = useRef<Set<string>>(new Set());

  const start = (resourceId: string, action: Action): boolean => {
    const key = pendingKey(resourceId, action);
    if (runningRef.current.has(key)) {
      return false;
    }

    runningRef.current.add(key);
    setRunning(new Set(runningRef.current));
    return true;
  };

  const finish = (resourceId: string, action: Action) => {
    runningRef.current.delete(pendingKey(resourceId, action));
    setRunning(new Set(runningRef.current));
  };

  const isRunning = (resourceId: string, action: Action): boolean =>
    runningRef.current.has(pendingKey(resourceId, action));

  /** Whether the resource has a running action, or the given one when `action` is set. */
  const isPending = (resourceId: string, action?: Action): boolean =>
    action === undefined
      ? [...running].some((key) => key.startsWith(pendingKeyPrefix(resourceId)))
      : running.has(pendingKey(resourceId, action));

  /** The first of `order` that is running for the resource, so callers pick the precedence. */
  const pendingAction = <Picked extends Action>(
    resourceId: string,
    order: readonly Picked[],
  ): Picked | undefined => order.find((action) => running.has(pendingKey(resourceId, action)));

  return { finish, isPending, isRunning, pendingAction, start };
}

function pendingKey(resourceId: string, action: string): string {
  return `${pendingKeyPrefix(resourceId)}${action}`;
}

// The length prefix keeps one resource ID from matching another that it starts.
function pendingKeyPrefix(resourceId: string): string {
  return `${resourceId.length}:${resourceId}:`;
}
