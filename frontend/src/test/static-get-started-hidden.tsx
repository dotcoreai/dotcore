import type { ReactNode } from "react";

import { GetStartedHiddenContext } from "@/features/get-started/use-get-started-hidden";

/** Provides a fixed Get started setting to a component rendered without the console layout. */
export function StaticGetStartedHidden({
  children,
  hidden = false,
}: {
  children: ReactNode;
  hidden?: boolean;
}) {
  return (
    <GetStartedHiddenContext
      value={{ hidden, hideOnce: () => {}, setHidden: () => Promise.resolve(true) }}
    >
      {children}
    </GetStartedHiddenContext>
  );
}
