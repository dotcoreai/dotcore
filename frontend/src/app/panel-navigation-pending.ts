import { createContext, useContext, useEffect, useRef } from "react";

interface PanelNavigationPendingRegistration {
  register: (token: symbol, reason: string | undefined) => void;
}

const PanelNavigationPendingContext = createContext(false);
const PanelNavigationPendingRegistrationContext =
  createContext<PanelNavigationPendingRegistration | null>(null);

function usePanelPendingNavigation(pending: boolean, reason: string): void {
  const registration = useContext(PanelNavigationPendingRegistrationContext);
  const token = useRef(Symbol("panel-navigation-pending"));

  useEffect(() => {
    if (!registration) return;
    registration.register(token.current, pending ? reason : undefined);
    return () => registration.register(token.current, undefined);
  }, [pending, reason, registration]);
}

function usePanelNavigationPending(): boolean {
  return useContext(PanelNavigationPendingContext);
}

export {
  PanelNavigationPendingContext,
  PanelNavigationPendingRegistrationContext,
  usePanelNavigationPending,
  usePanelPendingNavigation,
};
export type { PanelNavigationPendingRegistration };
