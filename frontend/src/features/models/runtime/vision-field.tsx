import { Field } from "@/components/ui/field";
import { Switch } from "@/components/ui/switch";

import type { VisionOption } from "./vision-option";

interface VisionFieldProps {
  disabled: boolean;
  enabled: boolean;
  onChange: (enabled: boolean) => void;
  /** The vision option the selected configuration declares. */
  option: VisionOption;
}

/** Turns a deployment's image input on or off, with the catalog's description as help. */
export function VisionField({ disabled, enabled, onChange, option }: VisionFieldProps) {
  return (
    <Field
      controlId="vision-option"
      help={option.description || undefined}
      label={option.displayName}
      layout="inline"
    >
      {(control) => (
        <Switch {...control} checked={enabled} disabled={disabled} onCheckedChange={onChange} />
      )}
    </Field>
  );
}
