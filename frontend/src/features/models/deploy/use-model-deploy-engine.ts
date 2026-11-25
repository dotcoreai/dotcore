import { ConnectError } from "@connectrpc/connect";
import { useEffect, useMemo, useRef, useState } from "react";

import { formatArgumentLines, parseArgumentLines } from "@/features/models/runtime/argument-lines";
import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { timing } from "@/lib/timing";

import type { LocalDeploymentValues } from "./model-deploy-launch";

/**
 * The custom engine arguments the Engine step's text box holds, and the tokens they split into.
 * Restored tokens start as one line each, so they read back unchanged.
 */
export function useCustomArguments(startTokens: readonly string[]) {
  const [text, setText] = useState(() => formatArgumentLines(startTokens));
  const tokens = useMemo(() => parseArgumentLines(text), [text]);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  return {
    inputRef,
    /** Clears the arguments, as when Advanced turns off. */
    reset: () => setText(""),
    setText,
    text,
    tokens,
  };
}

/**
 * The Engine step's launch command: the backend's preview of the launch, read when the step opens
 * and again once typing pauses, never while an earlier step has errors. Only the latest request's
 * answer counts; until it lands, the previous one stays shown as updating. A rejection blocks the
 * deploy only while it answers the current values.
 */
export function useLiveCommandPreview({
  active,
  blocked,
  onPreview,
  request,
}: {
  /** Whether the Engine step shows, which reads the command. */
  active: boolean;
  /** Whether an earlier step has errors, which skips the read. */
  blocked: boolean;
  onPreview: (values: LocalDeploymentValues) => Promise<ResolvedLocalModelDeploymentSnapshot>;
  /** The launch to preview; undefined while its values are incomplete. */
  request?: LocalDeploymentValues;
}) {
  const [result, setResult] = useState<PreviewResult>();
  const [pendingKey, setPendingKey] = useState<string>();
  const requestKey = request && previewKey(request);
  // The latest request and loader, so a new loader from the opener doesn't read again.
  const requestRef = useRef(request);
  const onPreviewRef = useRef(onPreview);
  const generationRef = useRef(0);
  // Whether the step has read once since it opened, so later reads wait for typing to pause.
  const enteredRef = useRef(false);

  useEffect(() => {
    requestRef.current = request;
    onPreviewRef.current = onPreview;
  });

  useEffect(() => {
    if (!active) {
      enteredRef.current = false;
      return;
    }
    if (blocked || !requestKey) return;

    const read = async () => {
      const values = requestRef.current;
      if (!values) return;
      const generation = ++generationRef.current;
      setPendingKey(requestKey);
      try {
        const snapshot = await onPreviewRef.current(values);
        if (generation === generationRef.current) setResult({ key: requestKey, snapshot });
      } catch (failure) {
        if (generation === generationRef.current)
          setResult({
            // The service's own words, without the transport's status prefix.
            error: ConnectError.from(failure).rawMessage || "The service returned no details.",
            key: requestKey,
          });
      } finally {
        if (generation === generationRef.current) setPendingKey(undefined);
      }
    };

    if (!enteredRef.current) {
      enteredRef.current = true;
      void read();
      return;
    }
    const timer = setTimeout(() => void read(), timing().commandPreviewInputMs);
    return () => clearTimeout(timer);
  }, [active, blocked, requestKey]);

  const current = result !== undefined && result.key === requestKey;
  const updating = useDelayedLoading(
    active && !blocked && Boolean(requestKey) && (!current || pendingKey !== undefined),
  );

  return {
    /** The latest answer, current or not. */
    result,
    /** The backend's rejection of the current values, which blocks the deploy. */
    rejection: current ? result.error : undefined,
    /** Whether the shown command is older than the values, once that lasts long enough to show. */
    updating,
  };
}

/** One preview answer: the resolved launch, or why the backend rejected it. */
export interface PreviewResult {
  error?: string;
  key: string;
  snapshot?: ResolvedLocalModelDeploymentSnapshot;
}

// What decides the command: the profile, the configuration and the target. The name and route
// don't change it, so editing them leaves the preview current.
function previewKey({ configuration, placement, profileId }: LocalDeploymentValues): string {
  return JSON.stringify([profileId, configuration, placement], (_key, value: unknown) =>
    typeof value === "bigint" ? value.toString() : value,
  );
}
