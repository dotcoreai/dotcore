import { Field } from "@/components/ui/field";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";

import type { KvCachePrecisionOption } from "./kv-cache-precision";

interface KvCachePrecisionFieldProps {
  disabled: boolean;
  onChange: (precision: string) => void;
  /** The KV cache precision option the selected configuration declares. */
  option: KvCachePrecisionOption;
  precision: string;
}

/**
 * Chooses the precision of a deployment's KV cache among the choices its configuration offers, with
 * the catalog's description as help.
 */
export function KvCachePrecisionField({
  disabled,
  onChange,
  option,
  precision,
}: KvCachePrecisionFieldProps) {
  return (
    <Field
      className="min-w-0 gap-2 rounded-lg border border-border bg-card p-4 text-sm font-semibold"
      controlId="kv-cache-precision"
      help={option.description || undefined}
      label={option.displayName}
    >
      {(control) => (
        <NativeSelect
          {...control}
          disabled={disabled}
          onChange={(event) => onChange(event.target.value)}
          value={precision}
        >
          {option.choices.map((choice) => (
            <NativeSelectOption key={choice} value={choice}>
              {choice}
            </NativeSelectOption>
          ))}
        </NativeSelect>
      )}
    </Field>
  );
}
