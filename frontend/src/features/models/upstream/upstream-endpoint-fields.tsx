import type { RefObject } from "react";

import { Detail, DetailList } from "@/components/ui/detail-list";
import { Field } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Notice } from "@/components/ui/notice";
import { SecretValue } from "@/components/ui/secret-value";
import type { FieldErrors } from "@/lib/form-validation";

import type { UpstreamFormValues } from "./upstream-form";

/** The endpoint fields of an upstream model deployment: base URL, upstream model ID and API key. */
export function UpstreamEndpointFields({
  values,
  onChange,
  error,
  editing = false,
  storedApiKey,
  fieldErrors = new Map(),
  onFieldBlur,
  baseUrlRef,
  upstreamModelIdRef,
}: {
  values: UpstreamFormValues;
  onChange: (values: UpstreamFormValues) => void;
  error?: string;
  editing?: boolean;
  /** The stored API key shown masked above the replace field; blank when none is configured. */
  storedApiKey?: string;
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
      <Field
        controlId="upstream-base-url"
        error={fieldErrors.get("baseUrl")}
        help="Address of the OpenAI-compatible server that runs the model."
        label="Base URL"
      >
        {(control) => (
          <Input
            {...control}
            ref={baseUrlRef}
            required
            type="url"
            value={values.baseUrl}
            onBlur={() => onFieldBlur?.("baseUrl")}
            onChange={(e) => set("baseUrl", e.target.value)}
          />
        )}
      </Field>
      <Field
        controlId="upstream-model-id"
        error={fieldErrors.get("upstreamModelId")}
        help="The model name that server expects in requests."
        label="Upstream model ID"
      >
        {(control) => (
          <Input
            {...control}
            ref={upstreamModelIdRef}
            required
            value={values.upstreamModelId}
            onBlur={() => onFieldBlur?.("upstreamModelId")}
            onChange={(e) => set("upstreamModelId", e.target.value)}
          />
        )}
      </Field>
      {editing && storedApiKey !== undefined ? (
        <DetailList>
          <Detail
            label="Current API key"
            value={
              storedApiKey ? (
                <SecretValue label="API key" value={storedApiKey} />
              ) : (
                <span className="text-muted-foreground">Not configured</span>
              )
            }
          />
        </DetailList>
      ) : null}
      <Field controlId="upstream-api-key" label="API key">
        {(control) => (
          <Input
            {...control}
            type="password"
            autoComplete="new-password"
            value={values.apiKey}
            onChange={(e) => set("apiKey", e.target.value)}
            placeholder={editing ? "Optional; blank retains stored key" : "Optional"}
          />
        )}
      </Field>
    </div>
  );
}
