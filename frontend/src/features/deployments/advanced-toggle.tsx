interface AdvancedToggleProps {
  checked: boolean;
  disabled?: boolean;
  onChange: (checked: boolean) => void;
}

/** Footer checkbox that switches a deployment dialog between its basic and advanced steps. */
export function AdvancedToggle({ checked, disabled = false, onChange }: AdvancedToggleProps) {
  return (
    <label className="flex items-center gap-2 text-sm font-medium">
      <input
        checked={checked}
        className="size-4 accent-primary"
        disabled={disabled}
        onChange={(event) => onChange(event.target.checked)}
        type="checkbox"
      />
      Advanced
    </label>
  );
}
