import { useRef, useState } from "react";

/**
 * Tracks which async actions are running for which resource. `start` refuses an action that is
 * already running for the same resource, and `isRunning` answers synchronously for callbacks.
 * Render code reads `isPending` and `pendingAction`, which follow state.
 */
export function usePendingActions<Action extends string>() {
  const [running, setRunning] = useState<PendingMap<Action>>(() => new Map());
  const runningRef = useRef<PendingMap<Action>>(new Map());

  const start = (resourceId: string, action: Action): boolean => {
    if (runningRef.current.get(resourceId)?.has(action)) {
      return false;
    }

    runningRef.current = withAction(runningRef.current, resourceId, action);
    setRunning(runningRef.current);
    return true;
  };

  const finish = (resourceId: string, action: Action) => {
    runningRef.current = withoutAction(runningRef.current, resourceId, action);
    setRunning(runningRef.current);
  };

  const isRunning = (resourceId: string, action: Action): boolean =>
    runningRef.current.get(resourceId)?.has(action) === true;

  /** Whether the resource has a running action, or the given one when `action` is set. */
  const isPending = (resourceId: string, action?: Action): boolean =>
    action === undefined ? running.has(resourceId) : running.get(resourceId)?.has(action) === true;

  /** The first of `order` that is running for the resource, so callers pick the precedence. */
  const pendingAction = <Picked extends Action>(
    resourceId: string,
    order: readonly Picked[],
  ): Picked | undefined => {
    const actions = running.get(resourceId);
    return order.find((action) => actions?.has(action) === true);
  };

  return { finish, isPending, isRunning, pendingAction, start };
}

type PendingMap<Action extends string> = ReadonlyMap<string, ReadonlySet<Action>>;

// Returns a new map, so the state and ref never share a mutable object.
function withAction<Action extends string>(
  running: PendingMap<Action>,
  resourceId: string,
  action: Action,
): PendingMap<Action> {
  const next = new Map(running);
  next.set(resourceId, new Set([...(running.get(resourceId) ?? []), action]));
  return next;
}

// Drops the resource entry once its last action finishes, so `isPending` without an action stays accurate.
function withoutAction<Action extends string>(
  running: PendingMap<Action>,
  resourceId: string,
  action: Action,
): PendingMap<Action> {
  const next = new Map(running);
  const remaining = new Set(running.get(resourceId));
  remaining.delete(action);
  if (remaining.size === 0) {
    next.delete(resourceId);
  } else {
    next.set(resourceId, remaining);
  }
  return next;
}
