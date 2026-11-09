import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it } from "vitest";

import type { DiagnosticLogOperations } from "@/api/diagnostic-log-client";
import { ProfileMenu } from "@/app/profile-menu";
import { ThemeProvider } from "@/theme/theme-provider";

class FakeStorage {
  getItem() {
    return null;
  }

  removeItem() {}

  setItem() {}
}

function renderProfileMenu(diagnostics?: DiagnosticLogOperations) {
  return render(
    <MemoryRouter>
      <ThemeProvider
        document={document}
        eventTarget={new EventTarget()}
        storage={new FakeStorage()}
      >
        <ProfileMenu diagnostics={diagnostics} />
      </ThemeProvider>
    </MemoryRouter>,
  );
}

function emptyDiagnostics(): DiagnosticLogOperations {
  return {
    follow: async function* () {
      await new Promise(() => undefined);
      yield { event: { kind: "entry" as const, entry: undefined as never } };
    },
    read: () =>
      Promise.resolve({
        entries: [],
        historyState: "no-older-output-observed",
        responseTruncated: false,
        sources: [{ sourceLabel: "Manager", sourceName: "node-manager", state: "available" }],
      }),
  };
}

describe("ProfileMenu", () => {
  it("identifies the static operator through an accessible trigger and menu", async () => {
    const user = userEvent.setup();
    renderProfileMenu();

    const trigger = screen.getByRole("button", { name: "Open profile menu" });
    expect(trigger).toHaveTextContent("LU");
    expect(trigger).toHaveTextContent("Local user");
    expect(trigger).toHaveTextContent("Administrator");
    expect(trigger.querySelector(".console-profile-copy")).toBeInTheDocument();
    expect(trigger.querySelector(".console-profile-avatar")).toHaveTextContent("LU");

    await user.click(trigger);

    const menu = screen.getByRole("menu");
    expect(menu).toBeVisible();
    expect(within(menu).getByRole("menuitemcheckbox", { name: "Dark mode" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    expect(within(menu).getByRole("menuitem", { name: "Site logs" })).toBeVisible();
    expect(within(menu).getByRole("menuitem", { name: "Preferences" })).toBeVisible();
    expect(within(menu).getAllByRole("menuitemcheckbox")).toHaveLength(1);
    // Every item leads with a decorative icon.
    const items = [
      ...within(menu).getAllByRole("menuitem"),
      ...within(menu).getAllByRole("menuitemcheckbox"),
    ];
    expect(items.map((item) => item.textContent)).toEqual([
      "Preferences",
      "Site logs",
      "Dark mode",
    ]);
    for (const item of items) {
      expect(item.querySelector('svg[aria-hidden="true"]')).toBeInTheDocument();
    }
    expect(within(menu).queryByText("Local user")).not.toBeInTheDocument();
    expect(within(menu).queryByText("Administrator")).not.toBeInTheDocument();
    expect(within(menu).queryByRole("separator")).not.toBeInTheDocument();
    expect(screen.queryByText(/sign out|profile/i)).not.toBeInTheDocument();
  });

  it.each([
    [
      "pointer",
      async (user: ReturnType<typeof userEvent.setup>, trigger: HTMLElement) => {
        await user.click(trigger);
        await user.click(screen.getByRole("menuitem", { name: "Site logs" }));
      },
    ],
    [
      "keyboard",
      async (user: ReturnType<typeof userEvent.setup>, trigger: HTMLElement) => {
        trigger.focus();
        await user.keyboard("{Enter}");
        await waitFor(() =>
          expect(screen.getByRole("menuitem", { name: "Preferences" })).toHaveFocus(),
        );
        await user.keyboard("{ArrowDown}");
        await waitFor(() =>
          expect(screen.getByRole("menuitem", { name: "Site logs" })).toHaveFocus(),
        );
        await user.keyboard("{Enter}");
      },
    ],
  ])(
    "opens Site logs locally with the %s and restores trigger focus after Escape",
    async (_method, openLogs) => {
      const user = userEvent.setup();
      const initialUrl = window.location.href;
      renderProfileMenu(emptyDiagnostics());

      const trigger = screen.getByRole("button", { name: "Open profile menu" });
      await openLogs(user, trigger);

      const dialog = await screen.findByRole("dialog", { name: "Site logs" });
      expect(document.querySelector('[role="menu"]')).not.toBeInTheDocument();
      expect(dialog).not.toHaveTextContent("Retained backend diagnostics are bounded");
      const output = screen.getByRole("log", { name: "Site log output" });
      await waitFor(() =>
        expect(output).toHaveTextContent("No retained backend logs are available."),
      );
      await waitFor(() => expect(output).toHaveFocus());
      expect(window.location.href).toBe(initialUrl);

      await user.keyboard("{Escape}");
      expect(dialog).not.toBeInTheDocument();
      await waitFor(() => expect(trigger).toHaveFocus());
      expect(window.location.href).toBe(initialUrl);
    },
  );

  it("toggles Dark mode with pointer, Enter, and Space", async () => {
    const user = userEvent.setup();
    renderProfileMenu();

    const trigger = screen.getByRole("button", { name: "Open profile menu" });
    await user.click(trigger);
    const darkMode = screen.getByRole("menuitemcheckbox", { name: "Dark mode" });

    await user.click(darkMode);
    expect(darkMode).toHaveAttribute("aria-checked", "false");

    darkMode.focus();
    await user.keyboard("{Enter}");
    expect(darkMode).toHaveAttribute("aria-checked", "true");

    await user.keyboard(" ");
    expect(darkMode).toHaveAttribute("aria-checked", "false");
  });

  it("returns focus to the trigger when Escape closes the menu", async () => {
    const user = userEvent.setup();
    renderProfileMenu();

    const trigger = screen.getByRole("button", { name: "Open profile menu" });
    await user.click(trigger);
    expect(screen.getByRole("menu")).toBeVisible();

    await user.keyboard("{Escape}");

    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });
});
