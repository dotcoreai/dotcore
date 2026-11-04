import { create } from "@bufbuild/protobuf";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { type ReactNode, useRef } from "react";
import { createMemoryRouter, RouterProvider, useLocation } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ChatClient, ChatRequest } from "@/api/chat-client";
import type { RouteClient } from "@/api/route-client";
import { ConsoleLayout, usePanelNavigation } from "@/app/console-layout";
import { ThemeProvider } from "@/app/theme-provider";
import { InspectorActions } from "@/components/inspector-actions/inspector-actions";
import { InspectorShell } from "@/components/ui/inspector-shell";
import type { ChatTarget } from "@/features/chat/chattable-route";
import { useChatAction } from "@/features/chat/use-chat-action";
import { InferenceRequestKind } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import {
  InferenceRouteAvailabilitySchema,
  InferenceRouteSchema,
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import { ChatRoute } from "@/routes/chat";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

function routeView(
  name: string,
  options: { enabled?: boolean; available?: boolean | null; kinds?: InferenceRequestKind[] } = {},
) {
  const { enabled = true, available = true, kinds = [] } = options;
  return create(InferenceRouteViewSchema, {
    requestKinds: kinds,
    route: create(InferenceRouteSchema, { inferenceRouteId: `id-${name}`, name, enabled }),
    availability:
      available === null ? undefined : create(InferenceRouteAvailabilitySchema, { available }),
  });
}

function fakeRouteClient(views = [routeView("alpha")]) {
  const listRoutes = vi.fn(() =>
    Promise.resolve(create(ListRoutesResponseSchema, { routes: views })),
  );
  return { client: { route: { listRoutes } } as unknown as RouteClient, listRoutes };
}

function fakeChatClient(reply = ["Hel", "lo"]) {
  const requests: ChatRequest[] = [];
  const client: ChatClient = {
    // eslint-disable-next-line @typescript-eslint/require-await
    async *streamChat(request) {
      requests.push(request);
      for (const text of reply) yield { type: "content", text };
      yield { type: "done" };
    },
  };
  return { client, requests };
}

function renderConsole(
  routeClient?: RouteClient,
  chatClient?: ChatClient,
  options: { initialEntries?: string[]; initialEntry?: string; routesPage?: ReactNode } = {},
) {
  const watch = new FakeResourceWatch({
    sources: routeClient ? { routes: () => routeClient.route.listRoutes({}) } : {},
  });
  const router = createMemoryRouter(
    [
      {
        element: (
          <ConsoleLayout
            chatClient={chatClient}
            eventTarget={new EventTarget()}
            resourceWatchClient={watch.client()}
            storage={window.localStorage}
          />
        ),
        children: [
          { path: "/models", element: <p>Models page</p> },
          { path: "/routes", element: options.routesPage ?? <p>Routes page</p> },
          { path: "/chat", element: <ChatRoute /> },
        ],
      },
    ],
    {
      initialEntries: options.initialEntries ?? [options.initialEntry ?? "/models"],
      initialIndex: options.initialEntries ? options.initialEntries.length - 1 : undefined,
    },
  );

  render(
    <ThemeProvider
      document={document}
      eventTarget={new EventTarget()}
      storage={window.localStorage}
    >
      <RouterProvider router={router} />
    </ThemeProvider>,
  );

  return router;
}

function setViewportWidth(width: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
  window.dispatchEvent(new Event("resize"));
}

/** A route inspector whose only action is Chat, open while the URL names a panel. */
function ChatInspectorPage({ target }: { target: ChatTarget }) {
  const location = useLocation();
  const navigation = usePanelNavigation();
  const openerRef = useRef<HTMLButtonElement>(null);
  const chat = useChatAction(target);

  return (
    <>
      <button ref={openerRef} type="button">
        Route opener
      </button>
      <InspectorShell
        body={<p>Route body</p>}
        footer={<InspectorActions primary={chat} />}
        id="chat-inspector"
        onOpenChange={(open) => {
          if (!open) navigation.close();
        }}
        open={location.search.includes("panel=")}
        openerRef={openerRef}
        title="Route beta"
      />
    </>
  );
}

function renderRouteInspector(
  target: ChatTarget,
  views = [routeView("alpha"), routeView("beta")],
  chatClient?: ChatClient,
) {
  return renderConsole(fakeRouteClient(views).client, chatClient, {
    initialEntry: "/routes?panel=routes%2Froute%3Aroute-beta",
    routesPage: <ChatInspectorPage target={target} />,
  });
}

const betaTarget: ChatTarget = { label: "Chat", mode: "chat", route: "beta" };

const inspectorChat = () =>
  within(screen.getByTestId("inspector-footer")).getByRole("button", { name: "Chat" });

afterEach(() => {
  vi.restoreAllMocks();
  setViewportWidth(1024);
});

/** The Chat page's conversation card. */
const chatPage = () => screen.getByRole("region", { name: "Chat" });

const location = (router: ReturnType<typeof renderConsole>) =>
  `${router.state.location.pathname}${router.state.location.search}`;

const panel = () => {
  const element = document.querySelector<HTMLElement>('section[aria-label="Chat"]');
  if (!element) throw new Error("chat panel is not mounted");
  return element;
};

describe("Chat", () => {
  it("toggles from the menu item, reflecting the open state", async () => {
    const user = userEvent.setup();
    renderConsole(fakeRouteClient().client);

    const item = screen.getByRole("button", { name: "Chat" });
    expect(item).toHaveAttribute("aria-pressed", "false");
    expect(panel()).not.toBeVisible();

    await user.click(item);
    expect(item).toHaveAttribute("aria-pressed", "true");
    expect(panel()).toBeVisible();

    await user.click(within(panel()).getByRole("button", { name: "Close" }));
    expect(panel()).not.toBeVisible();
    expect(item).toHaveAttribute("aria-pressed", "false");
  });

  it("lists only enabled, available routes", async () => {
    const user = userEvent.setup();
    const { client, listRoutes } = fakeRouteClient([
      routeView("alpha"),
      routeView("disabled", { enabled: false }),
      routeView("down", { available: false }),
      routeView("unknown", { available: null }),
    ]);
    renderConsole(client);

    await user.click(screen.getByRole("button", { name: "Chat" }));
    await waitFor(() => expect(listRoutes).toHaveBeenCalled());
    const picker = await within(panel()).findByRole("combobox", { name: "Route" });
    await waitFor(() => expect(picker).toHaveValue("alpha"));

    await user.click(within(panel()).getByRole("button", { name: "Show Route options" }));
    const options = await screen.findAllByRole("option");
    expect(options).toHaveLength(1);
    expect(options[0]).toHaveTextContent("alpha");
  });

  it("shows no route available with a link to Routes when none qualifies", async () => {
    const user = userEvent.setup();
    renderConsole(fakeRouteClient([routeView("down", { available: false })]).client);

    await user.click(screen.getByRole("button", { name: "Chat" }));
    expect(await within(panel()).findByText("No route available")).toBeVisible();
    expect(within(panel()).getByRole("button", { name: "Send" })).toBeDisabled();

    await user.click(within(panel()).getByRole("link", { name: "Go to Routes" }));
    expect(await screen.findByText("Routes page")).toBeInTheDocument();
  });

  it("streams a reply to the selected route and keeps it across close and navigation", async () => {
    const user = userEvent.setup();
    const chat = fakeChatClient();
    const router = renderConsole(fakeRouteClient().client, chat.client);

    await user.click(screen.getByRole("button", { name: "Chat" }));
    const box = await within(panel()).findByRole("textbox", { name: "Message" });
    await waitFor(() => expect(box).toHaveFocus());
    await waitFor(() =>
      expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("alpha"),
    );

    await user.type(box, "hi{Shift>}{Enter}{/Shift}there{Enter}");

    expect(await within(panel()).findByText("Hello")).toBeVisible();
    expect(chat.requests).toHaveLength(1);
    expect(chat.requests[0].model).toBe("alpha");
    expect(chat.requests[0].messages).toEqual([{ role: "user", content: "hi\nthere" }]);
    expect(box).toHaveValue("");
    expect(box).toHaveFocus();

    await user.click(within(panel()).getByRole("button", { name: "Close" }));
    await act(() => router.navigate("/routes"));
    await user.click(screen.getByRole("button", { name: "Chat" }));
    expect(within(panel()).getByText("Hello")).toBeVisible();

    await user.click(within(panel()).getByRole("button", { name: "New chat" }));
    expect(within(panel()).queryByText("Hello")).toBeNull();
  });

  it("drops focus from the message box after sending on a touch device", async () => {
    const user = userEvent.setup();
    const baseMatchMedia = window.matchMedia.bind(window);
    window.matchMedia = vi.fn((query: string) => ({
      ...baseMatchMedia(query),
      matches: query === "(pointer: coarse)",
    }));
    renderConsole(fakeRouteClient().client, fakeChatClient().client);

    await user.click(screen.getByRole("button", { name: "Chat" }));
    const box = await within(panel()).findByRole("textbox", { name: "Message" });
    await waitFor(() =>
      expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("alpha"),
    );
    await user.type(box, "hi");
    await user.click(within(panel()).getByRole("button", { name: "Send" }));

    expect(await within(panel()).findByText("Hello")).toBeVisible();
    expect(box).toHaveValue("");
    expect(box).not.toHaveFocus();
  });

  it("shows the error reason with Retry", async () => {
    const user = userEvent.setup();
    const client: ChatClient = {
      // eslint-disable-next-line @typescript-eslint/require-await
      async *streamChat() {
        yield { type: "error", message: "model not ready" };
      },
    };
    renderConsole(fakeRouteClient().client, client);

    await user.click(screen.getByRole("button", { name: "Chat" }));
    await waitFor(() =>
      expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("alpha"),
    );
    await user.type(within(panel()).getByRole("textbox", { name: "Message" }), "q{Enter}");

    expect(await within(panel()).findByRole("alert")).toHaveTextContent("model not ready");
    expect(within(panel()).getByRole("button", { name: "Retry" })).toBeVisible();
    expect(within(panel()).getByText("q")).toBeVisible();
  });

  it("closes on Escape from inside the panel, not from elsewhere on the page", async () => {
    const user = userEvent.setup();
    renderConsole(fakeRouteClient().client);

    await user.click(screen.getByRole("button", { name: "Chat" }));
    await user.keyboard("{Escape}");
    expect(panel()).not.toBeVisible();

    await user.click(screen.getByRole("button", { name: "Chat" }));
    (document.activeElement as HTMLElement).blur();
    await user.keyboard("{Escape}");
    expect(panel()).toBeVisible();
  });

  it("leaves Escape to an open route list", async () => {
    const user = userEvent.setup();
    renderConsole(fakeRouteClient().client);

    await user.click(screen.getByRole("button", { name: "Chat" }));
    await user.click(await within(panel()).findByRole("button", { name: "Show Route options" }));
    await screen.findAllByRole("option");
    await user.keyboard("{Escape}");

    await waitFor(() => expect(screen.queryAllByRole("option")).toHaveLength(0));
    expect(panel()).toBeVisible();
  });

  it("shows no floating chat button on mobile while closed", async () => {
    setViewportWidth(390);
    renderConsole(fakeRouteClient().client);
    await new Promise((resolve) => setTimeout(resolve, 20));

    expect(screen.queryByRole("button", { name: "Chat" })).toBeNull();
    setViewportWidth(1024);
  });

  it("opens the chat on the inspector's route beside a wide inspector", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    renderRouteInspector(betaTarget);

    await user.click(inspectorChat());

    expect(panel()).toBeVisible();
    await waitFor(() =>
      expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("beta"),
    );
    await user.click(within(panel()).getByRole("textbox", { name: "Message" }));
    expect(screen.getByRole("complementary", { name: "Route beta" })).toBeVisible();

    await user.keyboard("{Escape}");

    expect(panel()).not.toBeVisible();
    expect(screen.getByRole("complementary", { name: "Route beta" })).toBeVisible();
  });

  it("closes a wide inspector the opened chat would overlap", async () => {
    const user = userEvent.setup();
    setViewportWidth(1024);
    // jsdom has no layout: place the chat at 268-668px and the inspector from 564px.
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (
      this: Element,
    ) {
      if (this.hasAttribute("data-inspector-companion")) return new DOMRect(268, 0, 400, 560);
      if (this.hasAttribute("data-inspector-panel")) return new DOMRect(564, 0, 460, 800);
      return new DOMRect();
    });
    renderRouteInspector(betaTarget);

    await user.click(inspectorChat());

    await waitFor(() =>
      expect(screen.queryByRole("complementary", { name: "Route beta" })).not.toBeInTheDocument(),
    );
    expect(panel()).toBeVisible();
    expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("beta");
  });

  it("closes a narrow inspector before opening the full-screen chat", async () => {
    const user = userEvent.setup();
    setViewportWidth(500);
    renderRouteInspector(betaTarget);

    await user.click(
      within(screen.getByRole("dialog", { name: "Route beta" })).getByRole("button", {
        name: "Chat",
      }),
    );

    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: "Route beta" })).not.toBeInTheDocument(),
    );
    expect(panel()).toBeVisible();
    expect(panel()).toHaveClass("max-[721px]:inset-0");
    await waitFor(() =>
      expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("beta"),
    );
  });

  it("keeps the conversation and selects the requested route again after a picker change", async () => {
    const user = userEvent.setup();
    const chat = fakeChatClient();
    renderRouteInspector(betaTarget, undefined, chat.client);

    await user.click(inspectorChat());
    const picker = within(panel()).getByRole("combobox", { name: "Route" });
    await waitFor(() => expect(picker).toHaveValue("beta"));
    await user.click(within(panel()).getByRole("button", { name: "Show Route options" }));
    await user.click(await screen.findByRole("option", { name: /^alpha/ }));
    expect(picker).toHaveValue("alpha");
    await user.type(within(panel()).getByRole("textbox", { name: "Message" }), "hi{Enter}");
    expect(await within(panel()).findByText("Hello")).toBeVisible();

    await user.click(inspectorChat());

    expect(picker).toHaveValue("beta");
    expect(within(panel()).getByText("Hello")).toBeVisible();
    await user.type(within(panel()).getByRole("textbox", { name: "Message" }), "again{Enter}");
    await waitFor(() => expect(chat.requests).toHaveLength(2));
    expect(chat.requests.map((request) => request.model)).toEqual(["alpha", "beta"]);
    expect(chat.requests[1].messages.map((message) => message.content)).toEqual([
      "hi",
      "Hello",
      "again",
    ]);
  });

  it("shows a requested route that is no longer available instead of another route", async () => {
    const user = userEvent.setup();
    renderRouteInspector(betaTarget, [routeView("alpha"), routeView("beta", { available: false })]);

    await user.click(inspectorChat());

    const picker = within(panel()).getByRole("combobox", { name: "Route" });
    expect(
      await within(panel()).findByText("beta is not available. Pick another route to continue."),
    ).toBeVisible();
    expect(picker).toHaveValue("beta");
    expect(within(panel()).getByRole("button", { name: "Send" })).toBeDisabled();
  });

  it("disables Chat with the reason when the inspector has no chattable route", () => {
    renderRouteInspector({ label: "Chat", unavailableReason: "The route is unavailable." });

    expect(inspectorChat()).toHaveAttribute("aria-disabled", "true");
    expect(inspectorChat()).toHaveAccessibleDescription("The route is unavailable.");
  });
});

describe("Chat page", () => {
  const twoRoutes = () => fakeRouteClient([routeView("alpha"), routeView("beta")]).client;

  it("keeps the conversation across expanding the panel and showing the page as a panel", async () => {
    const user = userEvent.setup();
    const chat = fakeChatClient();
    const router = renderConsole(twoRoutes(), chat.client);

    await user.click(screen.getByRole("button", { name: "Chat" }));
    await waitFor(() =>
      expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("alpha"),
    );
    await user.type(within(panel()).getByRole("textbox", { name: "Message" }), "hi{Enter}");
    expect(await within(panel()).findByText("Hello")).toBeVisible();

    await user.click(within(panel()).getByRole("button", { name: "Open the Chat page" }));

    expect(location(router)).toBe("/chat?route=alpha");
    expect(panel()).not.toBeVisible();
    expect(within(chatPage()).getByText("Hello")).toBeVisible();
    await user.type(within(chatPage()).getByRole("textbox", { name: "Message" }), "more{Enter}");
    await waitFor(() => expect(chat.requests).toHaveLength(2));
    expect(chat.requests[1].model).toBe("alpha");
    expect(chat.requests[1].messages.map((message) => message.content)).toEqual([
      "hi",
      "Hello",
      "more",
    ]);

    await user.click(within(chatPage()).getByRole("button", { name: "Show as panel" }));

    await waitFor(() => expect(location(router)).toBe("/models"));
    await waitFor(() => expect(panel()).toBeVisible());
    expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("alpha");
    expect(within(panel()).getByText("more")).toBeVisible();
    expect(within(panel()).getAllByText("Hello")).toHaveLength(2);
  });

  it("selects the route named in the URL", async () => {
    const user = userEvent.setup();
    const chat = fakeChatClient();
    renderConsole(twoRoutes(), chat.client, { initialEntry: "/chat?route=beta" });

    const picker = within(chatPage()).getByRole("combobox", { name: "Route" });
    await waitFor(() => expect(picker).toHaveValue("beta"));
    await user.type(within(chatPage()).getByRole("textbox", { name: "Message" }), "hi{Enter}");

    await waitFor(() => expect(chat.requests).toHaveLength(1));
    expect(chat.requests[0].model).toBe("beta");
  });

  it("replaces the URL's route when the page's picker changes, so Back leaves the page", async () => {
    const user = userEvent.setup();
    const router = renderConsole(twoRoutes(), undefined, {
      initialEntries: ["/models", "/chat?route=alpha"],
    });

    const picker = within(chatPage()).getByRole("combobox", { name: "Route" });
    await waitFor(() => expect(picker).toHaveValue("alpha"));
    await user.click(within(chatPage()).getByRole("button", { name: "Show Route options" }));
    await user.click(await screen.findByRole("option", { name: /^beta/ }));

    expect(location(router)).toBe("/chat?route=beta");
    expect(picker).toHaveValue("beta");
    await act(() => router.navigate(-1));
    expect(location(router)).toBe("/models");
  });

  it("shows a route in the URL that isn't available as unavailable", async () => {
    renderConsole(
      fakeRouteClient([routeView("alpha"), routeView("beta", { available: false })]).client,
      undefined,
      {
        initialEntry: "/chat?route=beta",
      },
    );

    expect(
      await within(chatPage()).findByText("beta is not available. Pick another route to continue."),
    ).toBeVisible();
    expect(within(chatPage()).getByRole("combobox", { name: "Route" })).toHaveValue("beta");
    expect(within(chatPage()).getByRole("button", { name: "Send" })).toBeDisabled();
  });

  it("hides the panel on the page, marks Chat current there, and leaves the panel closed after Back", async () => {
    const user = userEvent.setup();
    const router = renderConsole(twoRoutes());

    await user.click(screen.getByRole("button", { name: "Chat" }));
    expect(panel()).toBeVisible();
    await act(() => router.navigate("/chat"));

    expect(panel()).not.toBeVisible();
    const item = screen.getByRole("button", { name: "Chat" });
    expect(item).toHaveAttribute("aria-current", "page");
    expect(item).toHaveAttribute("aria-pressed", "false");

    await act(() => router.navigate(-1));
    expect(location(router)).toBe("/models");
    expect(panel()).not.toBeVisible();
    expect(screen.getByRole("button", { name: "Chat" })).not.toHaveAttribute("aria-current");
  });

  it("shows as a panel on the page's route over Routes when the page has no previous page", async () => {
    const user = userEvent.setup();
    const router = renderConsole(twoRoutes(), undefined, { initialEntry: "/chat?route=beta" });
    await waitFor(() =>
      expect(within(chatPage()).getByRole("combobox", { name: "Route" })).toHaveValue("beta"),
    );

    await user.click(within(chatPage()).getByRole("button", { name: "Show as panel" }));

    await waitFor(() => expect(location(router)).toBe("/routes"));
    await waitFor(() => expect(panel()).toBeVisible());
    expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("beta");
  });

  it("shows as a panel from the sidebar's Chat item on the page", async () => {
    const user = userEvent.setup();
    const router = renderConsole(twoRoutes(), undefined, {
      initialEntries: ["/models", "/chat?route=beta"],
    });
    await waitFor(() =>
      expect(within(chatPage()).getByRole("combobox", { name: "Route" })).toHaveValue("beta"),
    );

    await user.click(screen.getByRole("button", { name: "Chat" }));

    await waitFor(() => expect(location(router)).toBe("/models"));
    await waitFor(() => expect(panel()).toBeVisible());
    expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("beta");
  });
});

describe("Chat request kinds", () => {
  const { CHAT, DECISION } = InferenceRequestKind;
  const kindRoutes = () =>
    fakeRouteClient([
      routeView("gemma", { kinds: [CHAT] }),
      routeView("clef", { kinds: [DECISION] }),
      routeView("mixed", { kinds: [CHAT, DECISION] }),
      routeView("down", { available: false, kinds: [DECISION] }),
    ]).client;
  const modeSwitch = () => within(panel()).queryByRole("radiogroup", { name: "Request kind" });

  async function pickRoute(user: ReturnType<typeof userEvent.setup>, name: RegExp) {
    await user.click(within(panel()).getByRole("button", { name: "Show Route options" }));
    await user.click(await screen.findByRole("option", { name }));
  }

  it("lists every available route with the kinds it serves", async () => {
    const user = userEvent.setup();
    renderConsole(kindRoutes());

    await user.click(screen.getByRole("button", { name: "Chat" }));
    await waitFor(() =>
      expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("gemma"),
    );
    await user.click(within(panel()).getByRole("button", { name: "Show Route options" }));

    const options = await screen.findAllByRole("option");
    expect(options.map((option) => option.getAttribute("aria-label"))).toEqual([
      "gemma Chat",
      "clef Decisions",
      "mixed Chat, Decisions",
    ]);
  });

  it("shows the conversation on a chat-only route and decisions on a decision-only one", async () => {
    const user = userEvent.setup();
    renderConsole(kindRoutes());

    await user.click(screen.getByRole("button", { name: "Chat" }));
    await waitFor(() => expect(panel()).toHaveAttribute("data-chat-mode", "chat"));
    expect(within(panel()).getByRole("textbox", { name: "Message" })).toBeVisible();
    expect(modeSwitch()).toBeNull();

    await pickRoute(user, /^clef/);

    expect(panel()).toHaveAttribute("data-chat-mode", "decision");
    expect(within(panel()).queryByRole("textbox", { name: "Message" })).toBeNull();
    expect(within(panel()).queryByRole("button", { name: "New chat" })).toBeNull();
    expect(modeSwitch()).toBeNull();
  });

  it("starts a mixed route on Chat and remembers its switch for the session", async () => {
    const user = userEvent.setup();
    renderConsole(kindRoutes());

    await user.click(screen.getByRole("button", { name: "Chat" }));
    await pickRoute(user, /^mixed/);

    expect(panel()).toHaveAttribute("data-chat-mode", "chat");
    expect(within(modeSwitch()!).getByRole("radio", { name: "Chat" })).toBeChecked();
    await user.click(within(modeSwitch()!).getByRole("radio", { name: "Decisions" }));
    expect(panel()).toHaveAttribute("data-chat-mode", "decision");
    expect(within(panel()).queryByRole("textbox", { name: "Message" })).toBeNull();

    await pickRoute(user, /^gemma/);
    expect(panel()).toHaveAttribute("data-chat-mode", "chat");
    await pickRoute(user, /^mixed/);

    expect(panel()).toHaveAttribute("data-chat-mode", "decision");
    expect(within(modeSwitch()!).getByRole("radio", { name: "Decisions" })).toBeChecked();
  });

  it("opens the chat in decision mode from an Ask action, on a mixed route too", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    renderRouteInspector({ label: "Ask", mode: "decision", route: "mixed" }, [
      routeView("gemma", { kinds: [CHAT] }),
      routeView("mixed", { kinds: [CHAT, DECISION] }),
    ]);

    await user.click(
      within(screen.getByTestId("inspector-footer")).getByRole("button", { name: "Ask" }),
    );

    expect(panel()).toBeVisible();
    await waitFor(() =>
      expect(within(panel()).getByRole("combobox", { name: "Route" })).toHaveValue("mixed"),
    );
    expect(panel()).toHaveAttribute("data-chat-mode", "decision");
    expect(within(modeSwitch()!).getByRole("radio", { name: "Decisions" })).toBeChecked();
  });

  it("keeps the mode across expanding to the Chat page", async () => {
    const user = userEvent.setup();
    renderConsole(kindRoutes());

    await user.click(screen.getByRole("button", { name: "Chat" }));
    await pickRoute(user, /^mixed/);
    await user.click(within(modeSwitch()!).getByRole("radio", { name: "Decisions" }));
    await user.click(within(panel()).getByRole("button", { name: "Open the Chat page" }));

    expect(chatPage()).toHaveAttribute("data-chat-mode", "decision");
    expect(
      within(within(chatPage()).getByRole("radiogroup", { name: "Request kind" })).getByRole(
        "radio",
        { name: "Decisions" },
      ),
    ).toBeChecked();
  });
});
