import { createContext, useCallback, useContext, useEffect, useLayoutEffect, useRef } from "react";

interface InspectorFloatingLayerRegistry {
  register: (token: symbol, open: boolean) => void;
}

const InspectorFloatingLayerContext = createContext<InspectorFloatingLayerRegistry | null>(null);

function useInspectorFloatingLayerRegistration(
  actualOpen?: boolean,
  initiallyOpen = false,
): (open: boolean) => void {
  const registry = useContext(InspectorFloatingLayerContext);
  const token = useRef(Symbol("inspector-floating-layer"));
  const initialRegistrationComplete = useRef(false);

  useLayoutEffect(() => {
    if (actualOpen !== undefined) {
      registry?.register(token.current, actualOpen);
      return;
    }
    if (!initialRegistrationComplete.current) {
      initialRegistrationComplete.current = true;
      if (initiallyOpen) registry?.register(token.current, true);
    }
  }, [actualOpen, initiallyOpen, registry]);

  useEffect(
    () => () => {
      registry?.register(token.current, false);
    },
    [registry],
  );

  return useCallback(
    (open: boolean) => {
      registry?.register(token.current, open);
    },
    [registry],
  );
}

/* eslint-disable react-refresh/only-export-components */
export {
  InspectorFloatingLayerContext,
  type InspectorFloatingLayerRegistry,
  useInspectorFloatingLayerRegistration,
};
/* eslint-enable react-refresh/only-export-components */
