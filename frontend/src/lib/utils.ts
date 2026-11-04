import { type ClassValue, clsx } from "clsx";
import { extendTailwindMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/**
 * Size names of the density scale in `index.css`, the `--spacing-<name>` entries behind utilities
 * such as `h-control-md` and `min-h-menu-row`. A name missing here stops `cn()` from letting a
 * caller's `h-auto` or `size-11` replace the primitive's density class.
 */
export const DENSITY_SIZE_NAMES = [
  "control-xs",
  "control-sm",
  "control-md",
  "control-action",
  "control-lg",
  "menu-row",
  "menu-row-sm",
];

const twMerge = extendTailwindMerge({
  extend: { theme: { spacing: DENSITY_SIZE_NAMES } },
});
