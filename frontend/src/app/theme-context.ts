import { createContext, useContext } from "react";

import { ColorScheme } from "@/gen/dotcore/console/v1/preferences_pb";

export interface ThemeContextValue {
  colorScheme: ColorScheme;
  setColorScheme: (colorScheme: ColorScheme) => void;
}

export const ThemeContext = createContext<ThemeContextValue | null>(null);

export function useTheme(): ThemeContextValue {
  const context = useContext(ThemeContext);

  if (!context) {
    throw new Error("useTheme must be used within a ThemeProvider");
  }

  return context;
}
