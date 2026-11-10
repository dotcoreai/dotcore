import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { expect } from "vitest";

/**
 * Opens an inspector's "More actions" menu and picks the named item. `scope` narrows the trigger
 * lookup when several inspectors or footers are on screen.
 */
export async function chooseMoreAction(
  user: UserEvent,
  name: string | RegExp,
  scope: HTMLElement = document.body,
) {
  await user.click(await within(scope).findByRole("button", { name: "More actions" }));
  await user.click(await screen.findByRole("menuitem", { name }));
}

/** Opens an inspector's "More actions" menu and returns the names of its items, in order. */
export async function moreActionNames(user: UserEvent, scope: HTMLElement = document.body) {
  await user.click(await within(scope).findByRole("button", { name: "More actions" }));
  const menu = await screen.findByRole("menu");
  return within(menu)
    .getAllByRole("menuitem")
    .map((item) => item.getAttribute("aria-label"));
}

/**
 * Lists an inspector footer's actions: its visible buttons and links, then the items of its "More
 * actions" menu, closing the menu again. Disabled actions end with " (disabled)".
 */
export async function inspectorActionList(
  user: UserEvent,
  footer: HTMLElement = screen.getByTestId("inspector-footer"),
) {
  const buttons = [...footer.querySelectorAll<HTMLElement>("button, a")].map(actionEntry);
  const trigger = within(footer).queryByRole("button", { name: "More actions" });
  if (!trigger || trigger.hasAttribute("disabled")) return { buttons, menu: [] };

  await user.click(trigger);
  const menu = await screen.findByRole("menu");
  const items = within(menu).getAllByRole("menuitem").map(actionEntry);
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByRole("menu")).not.toBeInTheDocument());
  return { buttons, menu: items };
}

function actionEntry(element: HTMLElement): string {
  const name = element.getAttribute("aria-label") ?? element.textContent ?? "";
  const disabled =
    element.hasAttribute("disabled") || element.getAttribute("aria-disabled") === "true";
  return disabled ? `${name} (disabled)` : name;
}

/** Synchronous `chooseMoreAction` for tests driven by `fireEvent` under fake timers. */
export function pickMoreAction(name: string | RegExp, scope: HTMLElement = document.body) {
  fireEvent.click(within(scope).getByRole("button", { name: "More actions" }));
  fireEvent.click(screen.getByRole("menuitem", { name }));
}

/**
 * Waits until an open inspector has moved focus to its heading. The inspector takes that focus a
 * frame after it opens, so typing into it any earlier can lose keys to the move. `name` picks the
 * heading when the inspector has several level 2 headings.
 */
export async function waitForInspectorFocus(inspector: HTMLElement, name?: string | RegExp) {
  await waitFor(() =>
    expect(within(inspector).getByRole("heading", { level: 2, name })).toHaveFocus(),
  );
}
