import { useId } from "react";

import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";

import type { KvCachePrecisionOption } from "./kv-cache-precision";

interface KvCachePrecisionFieldProps {
  disabled: boolean;
  onChange: (precision: string) => void;
  /** The KV cache precision option the selected configuration declares. */
  option: KvCachePrecisionOption;
  precision: string;
}

/** Chooses the precision of a deployment's KV cache among the choices its configuration offers. */
export function KvCachePrecisionField({
  disabled,
  onChange,
  option,
  precision,
}: KvCachePrecisionFieldProps) {
  const selectId = useId();

  return (
    <div className="grid min-w-0 gap-2 rounded-lg border border-border bg-card p-4">
      <label className="text-sm font-semibold" htmlFor={selectId}>
        {option.displayName}
      </label>
      <NativeSelect
        disabled={disabled}
        id={selectId}
        onChange={(event) => onChange(event.target.value)}
        value={precision}
      >
        {option.choices.map((choice) => (
          <NativeSelectOption key={choice} value={choice}>
            {choice}
          </NativeSelectOption>
        ))}
      </NativeSelect>
    </div>
  );
}
