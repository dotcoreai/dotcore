import { useRef, useState } from "react";

import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";

import type { LocalDeploymentValues } from "./model-deploy-launch";

/**
 * The deploy dialog's command preview: the launch command the backend would run for the form's
 * values, shown in its own dialog. A form change invalidates it, and only the latest request's
 * answer is kept.
 */
export function useCommandPreview(
  onPreview: (values: LocalDeploymentValues) => Promise<ResolvedLocalModelDeploymentSnapshot>,
) {
  const [preview, setPreview] = useState<ResolvedLocalModelDeploymentSnapshot>();
  const [previewError, setPreviewError] = useState<string>();
  const [previewOpen, setPreviewOpen] = useState(false);
  const [previewPending, setPreviewPending] = useState(false);
  const previewGeneration = useRef(0);
  const previewButtonRef = useRef<HTMLButtonElement>(null);
  const previewLoadingVisible = useDelayedLoading(previewPending);

  const invalidatePreview = () => {
    previewGeneration.current += 1;
    setPreviewOpen(false);
    setPreviewPending(false);
    setPreview(undefined);
    setPreviewError(undefined);
  };

  const requestPreview = async (values: LocalDeploymentValues) => {
    const generation = ++previewGeneration.current;
    previewButtonRef.current?.blur();
    setPreviewOpen(true);
    setPreviewPending(true);
    setPreviewError(undefined);
    setPreview(undefined);
    try {
      const result = await onPreview(values);
      if (generation === previewGeneration.current) setPreview(result);
    } catch (previewFailure) {
      if (generation === previewGeneration.current)
        setPreviewError(
          previewFailure instanceof Error ? previewFailure.message : "Preview was rejected.",
        );
    } finally {
      if (generation === previewGeneration.current) setPreviewPending(false);
    }
  };

  return {
    invalidatePreview,
    preview,
    previewButtonRef,
    previewError,
    previewLoadingVisible,
    previewOpen,
    previewPending,
    requestPreview,
    setPreviewOpen,
  };
}
