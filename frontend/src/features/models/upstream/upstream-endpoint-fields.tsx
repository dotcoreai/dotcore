/* eslint-disable jsx-a11y/label-has-associated-control */
import type { RefObject } from "react";

import { FieldError } from "@/components/ui/field-error";
import { Input } from "@/components/ui/input";
import { Notice } from "@/components/ui/notice";
import type { FieldErrors } from "@/lib/form-validation";

import type { UpstreamFormValues } from "./upstream-form";

/** The endpoint fields of an upstream model deployment: base URL, upstream model ID and API key. */
export function UpstreamEndpointFields({
  values,
  onChange,
  error,
  editing = false,
  fieldErrors = new Map(),
  onFieldBlur,
  baseUrlRef,
  upstreamModelIdRef,
}: {
  values: UpstreamFormValues;
  onChange: (values: UpstreamFormValues) => void;
  error?: string;
  editing?: boolean;
  fieldErrors?: FieldErrors;
  onFieldBlur?: (field: "baseUrl" | "upstreamModelId") => void;
  baseUrlRef?: RefObject<HTMLInputElement | null>;
  upstreamModelIdRef?: RefObject<HTMLInputElement | null>;
}) {
  const set = <K extends keyof UpstreamFormValues>(key: K, value: UpstreamFormValues[K]) =>
    onChange({ ...values, [key]: value });
  return (
    <div className="grid gap-4">
      {error ? (
        <Notice role="alert" tone="destructive">
          {error}
        </Notice>
      ) : null}
      <label className="grid gap-1">
        Base URL
        <Input
          ref={baseUrlRef}
          required
          aria-describedby={fieldErrors.get("baseUrl") ? "upstream-base-url-error" : undefined}
          aria-invalid={fieldErrors.get("baseUrl") ? "true" : undefined}
          type="url"
          value={values.baseUrl}
          onBlur={() => onFieldBlur?.("baseUrl")}
          onChange={(e) => set("baseUrl", e.target.value)}
        />
        {fieldErrors.get("baseUrl") ? (
          <FieldError id="upstream-base-url-error">{fieldErrors.get("baseUrl")}</FieldError>
        ) : null}
      </label>
      <label className="grid gap-1">
        Upstream model ID
        <Input
          ref={upstreamModelIdRef}
          required
          aria-label="Upstream model ID"
          value={values.upstreamModelId}
          aria-describedby={
            fieldErrors.get("upstreamModelId") ? "upstream-model-id-error" : undefined
          }
          aria-invalid={fieldErrors.get("upstreamModelId") ? "true" : undefined}
          onBlur={() => onFieldBlur?.("upstreamModelId")}
          onChange={(e) => set("upstreamModelId", e.target.value)}
        />
        {fieldErrors.get("upstreamModelId") ? (
          <FieldError id="upstream-model-id-error">{fieldErrors.get("upstreamModelId")}</FieldError>
        ) : null}
      </label>
      <label className="grid gap-1">
        API key
        <Input
          type="password"
          autoComplete="new-password"
          value={values.apiKey}
          onChange={(e) => set("apiKey", e.target.value)}
          placeholder={editing ? "Optional; blank retains stored key" : "Optional"}
        />
      </label>
    </div>
  );
}
