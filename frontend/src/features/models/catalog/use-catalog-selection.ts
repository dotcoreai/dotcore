import { type RefObject, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import type { ModelsPolling } from "@/features/models/polling/models-polling-context";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { usePanelNavigation } from "@/panels/panel-navigation";
import {
  activePanel,
  appendPanel,
  appendPanelLocation,
  canonicalizePanelLocation,
  deepensPanelStack,
  isSamePanelEntry,
  type PanelEntry,
  panelPagePath,
  popPanel,
  selectPanel,
} from "@/panels/panel-stack";

import type { CatalogViewState } from "./catalog-view-state";

/**
 * The Catalog page's selection: the catalog model or profile whose panel is open, kept in step
 * with the URL's panel stack. Selecting navigates the panel stack, a selection whose model or
 * profile leaves the catalog closes, and closing the last panel returns focus to the page heading.
 */
export function useCatalogSelection({
  applyView,
  panelNavigation,
  selectionRef,
  setDeployError,
  setDeployModel,
  setDeployProfileId,
  setUpstreamError,
  snapshot,
  viewState,
}: CatalogSelectionOptions) {
  const location = useLocation();
  const navigate = useNavigate();
  const [selection, setSelection] = useState<CatalogSelection>();
  const canonical = useMemo(
    () => canonicalizePanelLocation({ pathname: location.pathname, search: location.search }),
    [location.pathname, location.search],
  );
  const urlEntry = activePanel(canonical.search);
  const urlSelection = catalogSelectionFromEntry(urlEntry);
  const openerRef = useRef<HTMLElement | null>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const pendingSelectionRef = useRef<CatalogSelection | undefined>(undefined);
  const shouldRestoreHeadingFocus = useRef(false);
  const closeSelection = useCallback(() => {
    setDeployModel(undefined);
    setDeployProfileId(undefined);
    pendingSelectionRef.current = undefined;
    selectionRef.current = undefined;
    setDeployError(undefined);
    setUpstreamError(undefined);
    setSelection(undefined);
    panelNavigation.close();
  }, [
    panelNavigation,
    selectionRef,
    setDeployError,
    setDeployModel,
    setDeployProfileId,
    setUpstreamError,
  ]);

  useEffect(() => {
    if (canonical.recovered || (urlEntry && !urlSelection)) {
      // The shown view wins over the one the location was read with; another page takes none.
      void navigate(
        canonical.pathname === location.pathname
          ? { ...canonical, search: applyView(canonical.search, viewState) }
          : canonical,
        { replace: true },
      );
      return;
    }

    if (pendingSelectionRef.current) {
      if (isSameSelection(pendingSelectionRef.current, urlSelection)) {
        pendingSelectionRef.current = undefined;
      } else {
        return;
      }
    }

    if (!isSameSelection(selectionRef.current, urlSelection)) {
      selectionRef.current = urlSelection;
      setDeployError(undefined);
      setUpstreamError(undefined);
      setSelection(urlSelection);
    }
  }, [
    applyView,
    canonical,
    location.pathname,
    navigate,
    selectionRef,
    setDeployError,
    setUpstreamError,
    urlEntry,
    urlSelection,
    viewState,
  ]);

  const selectedModel =
    selection?.kind === "catalog" || selection?.kind === "catalog-profile"
      ? snapshot?.availableModels.find((model) => model.modelId === selection.modelId)
      : undefined;

  useEffect(() => {
    if (!selection || !snapshot) {
      return;
    }

    if (
      selection.kind === "catalog-profile" &&
      selectedModel &&
      !selectedModel.profiles.some((profile) => profile.profileId === selection.profileId)
    ) {
      queueMicrotask(() => {
        if (isSameSelection(selectionRef.current, selection)) {
          void navigate(
            {
              pathname: panelPagePath("model-catalog"),
              search: applyView(popPanel(location.search), viewState),
            },
            { replace: true },
          );
        }
      });
      return;
    }

    if ((selection.kind === "catalog" || selection.kind === "catalog-profile") && !selectedModel) {
      queueMicrotask(() => {
        if (isSameSelection(selectionRef.current, selection)) {
          openerRef.current = headingRef.current;
          shouldRestoreHeadingFocus.current = true;
          setDeployModel(undefined);
          closeSelection();
        }
      });
    }
  }, [
    applyView,
    closeSelection,
    selectedModel,
    selection,
    selectionRef,
    setDeployModel,
    snapshot,
    location.search,
    navigate,
    viewState,
  ]);

  useEffect(() => {
    if (!selection && shouldRestoreHeadingFocus.current) {
      headingRef.current?.focus();
      shouldRestoreHeadingFocus.current = false;
    }
  }, [selection]);

  const select = (nextSelection: CatalogSelection, opener: HTMLElement | null) => {
    openerRef.current = opener;
    pendingSelectionRef.current = nextSelection;
    selectionRef.current = nextSelection;
    setDeployError(undefined);
    setUpstreamError(undefined);
    setSelection(nextSelection);
    // A profile opens above its catalog model, so Back returns to the model.
    const search =
      nextSelection.kind === "catalog-profile"
        ? appendPanel(
            selectPanel(location.search, {
              page: "model-catalog",
              resourceId: nextSelection.modelId,
              screen: "catalog",
            }),
            catalogSelectionEntry(nextSelection),
          )
        : selectPanel(location.search, catalogSelectionEntry(nextSelection));
    panelNavigation.navigatePanel(
      { pathname: panelPagePath("model-catalog"), search },
      { replace: !deepensPanelStack(location.search, search) },
    );
  };

  // A deployment the operator came from is the panel beneath, so opening it again returns there.
  const openDeployment = (entry: PanelEntry) => {
    if (panelNavigation.parent && isSamePanelEntry(panelNavigation.parent, entry)) {
      panelNavigation.back();
    } else {
      panelNavigation.navigatePanel(appendPanelLocation(location, entry));
    }
  };

  const onInspectorOpenChange = (open: boolean) => {
    if (!open) {
      setDeployModel(undefined);
      panelNavigation.close();
    }
  };

  return {
    headingRef,
    onInspectorOpenChange,
    openDeployment,
    openerRef,
    select,
    selectedModel,
    selection,
    urlSelection,
  };
}

export interface CatalogSelectionOptions {
  applyView: (search: string, state: CatalogViewState) => string;
  panelNavigation: ReturnType<typeof usePanelNavigation>;
  /** The latest selection, shared with the deploy flows that check it after a request resolves. */
  selectionRef: RefObject<CatalogSelection | undefined>;
  setDeployError: (error: undefined) => void;
  setDeployModel: (model: Model | undefined) => void;
  setDeployProfileId: (profileId: string | undefined) => void;
  setUpstreamError: (error: undefined) => void;
  snapshot: ModelsPolling["snapshot"];
  viewState: CatalogViewState;
}

export type CatalogSelection =
  | { kind: "catalog"; modelId: string }
  | { kind: "catalog-profile"; modelId: string; profileId: string };

export function isSameSelection(
  left: CatalogSelection | undefined,
  right: CatalogSelection | undefined,
): boolean {
  if (!left || !right || left.kind !== right.kind) {
    return false;
  }

  if (left.kind === "catalog" && right.kind === "catalog") {
    return left.modelId === right.modelId;
  }

  return (
    left.kind === "catalog-profile" &&
    right.kind === "catalog-profile" &&
    left.modelId === right.modelId &&
    left.profileId === right.profileId
  );
}

function catalogSelectionFromEntry(entry: PanelEntry | undefined): CatalogSelection | undefined {
  if (!entry || entry.page !== "model-catalog" || !entry.resourceId) return undefined;
  if (entry.screen === "catalog") return { kind: "catalog", modelId: entry.resourceId };
  if (entry.screen === "catalog-profile" && entry.profileId)
    return { kind: "catalog-profile", modelId: entry.resourceId, profileId: entry.profileId };
  return undefined;
}

function catalogSelectionEntry(selection: CatalogSelection): PanelEntry {
  if (selection.kind === "catalog")
    return { page: "model-catalog", resourceId: selection.modelId, screen: "catalog" };
  return {
    page: "model-catalog",
    profileId: selection.profileId,
    resourceId: selection.modelId,
    screen: "catalog-profile",
  };
}
