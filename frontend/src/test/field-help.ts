import { fireEvent, screen, waitFor } from "@testing-library/react";
import { expect } from "vitest";

/**
 * Opens a field's help popover, returns the text of each of its facts, one per paragraph, and
 * closes it again.
 */
export async function readFieldHelpFacts(label: string): Promise<string[]> {
  fireEvent.click(screen.getByRole("button", { name: `About ${label}` }));
  const popover = await screen.findByRole("dialog", { name: `About ${label}` });
  const facts = [...popover.querySelectorAll("p")].map((fact) => fact.textContent ?? "");

  fireEvent.keyDown(popover, { key: "Escape" });
  await waitFor(() =>
    expect(screen.queryByRole("dialog", { name: `About ${label}` })).not.toBeInTheDocument(),
  );

  return facts;
}
