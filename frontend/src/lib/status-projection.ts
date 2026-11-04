/**
 * The visual tone a projected status carries. Every console surface picks its badge variant from
 * this set, so one state reads the same way on a card as it does in an inspector.
 */
export type LifecycleTone = "success" | "info" | "warning" | "neutral" | "destructive";

/**
 * A projected status: the single word a surface renders for a resource, the tone that word carries,
 * and an optional label qualifying it.
 *
 * The attention label explains the status beside it — load progress, a failure message, what the
 * resource is waiting for, or why it is unavailable. It never carries a fact that holds regardless
 * of status, such as a resource kind or an observation timestamp.
 */
export interface StatusProjection {
  attention?: string;
  label: string;
  tone: LifecycleTone;
}
