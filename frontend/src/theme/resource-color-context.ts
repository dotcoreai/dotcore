import { createContext, useContext } from "react";

import { assignResourceColors, getResourceColor } from "@/theme/resource-color";

/** Resolves resource identity marker colors from the site's color assignment. */
export interface ResourceColors {
  /** The marker color of a resource: its assigned color, or its hash color outside the pool. */
  colorOf: (key: string) => string;
  /** The color a resource about to join the site would most likely get. */
  nextColor: string;
}

export const ResourceColorContext = createContext<ResourceColors | null>(null);

/**
 * The site's marker colors. Outside a color provider, such as a component rendered in isolation,
 * every resource gets its hash color.
 */
export function useResourceColors(): ResourceColors {
  return useContext(ResourceColorContext) ?? hashColors;
}

const hashColors: ResourceColors = {
  colorOf: getResourceColor,
  nextColor: assignResourceColors([]).nextColor,
};
