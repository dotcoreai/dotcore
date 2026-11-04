import { useState } from "react";

import { Card } from "@/components/ui/card";
import { FieldError } from "@/components/ui/field-error";
import { Input } from "@/components/ui/input";
import { PageSectionHeading } from "@/components/ui/page-header";
import { Switch } from "@/components/ui/switch";
import { PreferenceRow } from "@/features/preferences/preferences-page";
import { useDeveloperSettings } from "@/hooks/use-developer-settings";
import { getBrowserStorage } from "@/lib/browser-storage";
import { MAX_LATENCY_MS, writeDeveloperSettings } from "@/lib/console-preferences";

/** Latency range applied when simulation is switched on with no range configured yet. */
const SUGGESTED_RANGE = { latencyMinMs: 500, latencyMaxMs: 2_000 };

/** Developer tools for this browser: synthetic API latency for reproducing slow backends. */
export function DeveloperSection() {
  const settings = useDeveloperSettings();
  // A field shows its typed draft only while that draft is not a stored value, so edits made in
  // another tab show through without an effect.
  const [draft, setDraft] = useState<{ min?: string; max?: string }>({});
  const minText = draft.min ?? String(settings.latencyMinMs);
  const maxText = draft.max ?? String(settings.latencyMaxMs);
  const error = rangeError(minText, maxText);

  // Edits are validated as they are typed but stored on blur or Enter, so the setting never
  // passes through half-typed values.
  function commit() {
    if (error || (draft.min === undefined && draft.max === undefined)) {
      return;
    }

    setDraft({});
    writeDeveloperSettings(getBrowserStorage(), {
      latencyEnabled: settings.latencyEnabled,
      latencyMinMs: parseMs(minText)!,
      latencyMaxMs: parseMs(maxText)!,
    });
  }

  function setEnabled(latencyEnabled: boolean) {
    const unconfigured = settings.latencyMaxMs === 0;

    writeDeveloperSettings(getBrowserStorage(), {
      ...settings,
      ...(latencyEnabled && unconfigured ? SUGGESTED_RANGE : {}),
      latencyEnabled,
    });
  }

  return (
    <section aria-labelledby="preferences-developer-heading" className="flex flex-col gap-3">
      <PageSectionHeading id="preferences-developer-heading">Developer</PageSectionHeading>
      <Card>
        <PreferenceRow
          description="Delay every API call by a random time within the range below. Delays count toward request timeouts."
          label="Simulate API latency"
        >
          {(labelledBy) => (
            <Switch
              aria-labelledby={labelledBy}
              checked={settings.latencyEnabled}
              onCheckedChange={setEnabled}
            />
          )}
        </PreferenceRow>
        <div className="flex flex-wrap gap-4">
          <LatencyField
            describedBy={error ? "preferences-latency-error" : undefined}
            invalid={Boolean(error)}
            label="Minimum delay (ms)"
            onChange={(text) => setDraft((current) => ({ ...current, min: text }))}
            onCommit={commit}
            value={minText}
          />
          <LatencyField
            describedBy={error ? "preferences-latency-error" : undefined}
            invalid={Boolean(error)}
            label="Maximum delay (ms)"
            onChange={(text) => setDraft((current) => ({ ...current, max: text }))}
            onCommit={commit}
            value={maxText}
          />
        </div>
        {error ? <FieldError id="preferences-latency-error">{error}</FieldError> : null}
      </Card>
    </section>
  );
}

function LatencyField({
  describedBy,
  invalid,
  label,
  onChange,
  onCommit,
  value,
}: {
  describedBy?: string;
  invalid: boolean;
  label: string;
  onChange: (text: string) => void;
  onCommit: () => void;
  value: string;
}) {
  return (
    <label className="flex w-44 flex-col gap-1.5 text-sm font-medium">
      {label}
      <Input
        aria-describedby={describedBy}
        aria-invalid={invalid || undefined}
        inputMode="numeric"
        min={0}
        onBlur={onCommit}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter") onCommit();
        }}
        step={1}
        type="number"
        value={value}
      />
    </label>
  );
}

function rangeError(minText: string, maxText: string): string | undefined {
  const min = parseMs(minText);
  const max = parseMs(maxText);

  if (min === undefined || max === undefined) {
    return "Enter whole milliseconds, 0 or more.";
  }

  if (max > MAX_LATENCY_MS) {
    return `Maximum delay must be at most ${MAX_LATENCY_MS.toLocaleString("en-US")} ms.`;
  }

  if (min > max) {
    return "Minimum delay must not exceed the maximum.";
  }

  return undefined;
}

function parseMs(text: string): number | undefined {
  if (!/^\d+$/.test(text.trim())) {
    return undefined;
  }

  return Number(text.trim());
}
