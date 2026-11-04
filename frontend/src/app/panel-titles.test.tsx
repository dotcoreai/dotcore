import { act, render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it, vi } from "vitest";

import type { PanelEntry } from "@/lib/panel-stack";

import { PanelTitlesProvider } from "./panel-titles";
import {
  PanelParentContext,
  type PanelTitleLookup,
  usePanelParent,
  usePublishPanelTitles,
} from "./panel-titles-context";

const route: PanelEntry = { page: "routes", screen: "route", resourceId: "r1" };

function Parent() {
  const parent = usePanelParent();
  return <p data-testid="parent">{parent ? parent.label : "none"}</p>;
}

function Publisher({ name }: { name: string }) {
  usePublishPanelTitles([{ entry: route, name }]);
  return null;
}

function Harness({
  children,
  entry = route,
  lookup,
}: {
  children?: ReactNode;
  entry?: PanelEntry;
  lookup?: PanelTitleLookup;
}) {
  return (
    <PanelTitlesProvider lookup={lookup}>
      <PanelParentContext.Provider value={entry}>
        {children}
        <Parent />
      </PanelParentContext.Provider>
    </PanelTitlesProvider>
  );
}

describe("usePanelParent", () => {
  it("falls back to the kind of resource", () => {
    render(<Harness />);

    expect(screen.getByTestId("parent")).toHaveTextContent("route");
  });

  it("is undefined without a parent entry", () => {
    render(
      <PanelTitlesProvider>
        <Parent />
      </PanelTitlesProvider>,
    );

    expect(screen.getByTestId("parent")).toHaveTextContent("none");
  });

  it("shows a published name and follows a rename", () => {
    const { rerender } = render(
      <Harness>
        <Publisher name="chat" />
      </Harness>,
    );
    expect(screen.getByTestId("parent")).toHaveTextContent("chat");

    rerender(
      <Harness>
        <Publisher name="chat-2" />
      </Harness>,
    );

    expect(screen.getByTestId("parent")).toHaveTextContent("chat-2");
  });

  it("reads a missing name once and caches it", async () => {
    const lookup = vi.fn<PanelTitleLookup>(() => Promise.resolve("looked-up"));
    const { rerender } = render(<Harness lookup={lookup} />);

    await waitFor(() => expect(screen.getByTestId("parent")).toHaveTextContent("looked-up"));
    rerender(<Harness lookup={lookup} />);

    expect(lookup).toHaveBeenCalledTimes(1);
    expect(lookup.mock.calls[0][0]).toEqual(route);
  });

  it("skips the lookup when a page has published the name", () => {
    const lookup = vi.fn<PanelTitleLookup>(() => Promise.resolve("looked-up"));

    render(
      <Harness lookup={lookup}>
        <Publisher name="chat" />
      </Harness>,
    );

    expect(screen.getByTestId("parent")).toHaveTextContent("chat");
    expect(lookup).not.toHaveBeenCalled();
  });

  it("keeps the kind label when the lookup fails", async () => {
    const lookup = vi.fn<PanelTitleLookup>(() => Promise.reject(new Error("unavailable")));

    render(<Harness lookup={lookup} />);
    await act(() => Promise.resolve());

    expect(lookup).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("parent")).toHaveTextContent("route");
  });

  it("aborts the lookup when the parent entry changes", () => {
    let signal: AbortSignal | undefined;
    const lookup = vi.fn<PanelTitleLookup>((_entry, abortSignal) => {
      signal = abortSignal;
      return new Promise(() => undefined);
    });
    const { rerender } = render(<Harness lookup={lookup} />);

    rerender(
      <Harness entry={{ page: "routes", screen: "route", resourceId: "r2" }} lookup={lookup} />,
    );

    expect(lookup).toHaveBeenCalledTimes(2);
    expect(signal?.aborted).toBe(false);
    expect(lookup.mock.calls[0][1].aborted).toBe(true);
  });
});
