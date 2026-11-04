import { expect } from "vitest";

import { getResourceColor } from "@/lib/resource-color";

/**
 * Asserts that `element` directly contains exactly one decorative identity marker filled with the
 * color of `colorKey`.
 */
export function expectResourceMarker(element: Element | null | undefined, colorKey: string) {
  const markers = element?.querySelectorAll(':scope > [data-slot="resource-marker"]') ?? [];
  expect(markers).toHaveLength(1);
  expect(markers[0]).toHaveAttribute("aria-hidden", "true");
  expect(markers[0]).toHaveStyle({ backgroundColor: getResourceColor(colorKey) });
}

/** Returns the header title element that names an inspector panel. */
export function inspectorTitle(panel: HTMLElement): HTMLElement | null {
  const titleId = panel.getAttribute("aria-labelledby");
  return titleId ? document.getElementById(titleId) : null;
}
