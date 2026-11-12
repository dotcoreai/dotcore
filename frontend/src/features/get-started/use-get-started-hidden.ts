import { createContext, useContext } from "react";

/**
 * Whether Get started is hidden for the whole site, and how the console changes it. Every user
 * under the provider sees the same value.
 */
export interface GetStartedHidden {
  /** Why the last change failed, until the next change starts; the setting is then unchanged. */
  error?: string;
  /** Hides Get started once; does nothing while it is hidden, unloaded or a change is pending. */
  hideOnce: () => void;
  /** Whether the site hides Get started; unknown until the site's settings load. */
  hidden: boolean | undefined;
  /** Saves the setting and answers whether the site accepted it. */
  setHidden: (hidden: boolean) => Promise<boolean>;
}

export const GetStartedHiddenContext = createContext<GetStartedHidden | null>(null);

/** The site's Get started setting; reads under `GetStartedHiddenProvider`. */
export function useGetStartedHidden(): GetStartedHidden {
  const value = useContext(GetStartedHiddenContext);

  if (!value) {
    throw new Error("useGetStartedHidden must be used within a GetStartedHiddenProvider");
  }

  return value;
}
