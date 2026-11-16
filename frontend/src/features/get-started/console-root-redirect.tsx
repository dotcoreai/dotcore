import { Navigate } from "react-router-dom";

import { useGetStartedHidden } from "./use-get-started-hidden";

/**
 * The console's landing page: Get started while the site shows it, the overview once it is
 * hidden. Renders nothing until the site's setting loads, so the console never lands on one page
 * and jumps to the other.
 */
export function ConsoleRootRedirect() {
  const { hidden } = useGetStartedHidden();

  if (hidden === undefined) return null;

  return <Navigate replace to={hidden ? "/overview" : "/get-started"} />;
}
