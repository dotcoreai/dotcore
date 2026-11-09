import { type ReactNode, useCallback, useEffect, useMemo, useState } from "react";

import { ColorScheme } from "@/gen/dotcore/console/v1/preferences_pb";
import { getBrowserStorage, type StorageLike } from "@/lib/browser-storage";
import {
  applyColorScheme,
  readColorScheme,
  subscribeToColorScheme,
  writeColorScheme,
} from "@/lib/console-preferences";
import { ThemeContext } from "@/theme/theme-context";

export interface ThemeProviderProps {
  children: ReactNode;
  document?: Document;
  eventTarget?: EventTarget;
  storage?: StorageLike;
}

export function ThemeProvider({
  children,
  document = globalThis.document,
  eventTarget = globalThis.window,
  storage = getBrowserStorage(),
}: ThemeProviderProps) {
  const [colorScheme, setCurrentColorScheme] = useState(() => readColorScheme(storage));

  const setColorScheme = useCallback(
    (nextColorScheme: ColorScheme) => {
      const colorScheme =
        nextColorScheme === ColorScheme.DARK ? ColorScheme.DARK : ColorScheme.LIGHT;

      setCurrentColorScheme(colorScheme);
      applyColorScheme(document, colorScheme);
      writeColorScheme(storage, colorScheme);
    },
    [document, storage],
  );

  useEffect(() => {
    applyColorScheme(document, colorScheme);
  }, [colorScheme, document]);

  useEffect(
    () =>
      subscribeToColorScheme(eventTarget, storage, (nextColorScheme) => {
        setCurrentColorScheme(nextColorScheme);
        applyColorScheme(document, nextColorScheme);
      }),
    [document, eventTarget, storage],
  );

  const value = useMemo(
    () => ({
      colorScheme,
      setColorScheme,
    }),
    [colorScheme, setColorScheme],
  );

  return <ThemeContext value={value}>{children}</ThemeContext>;
}
