import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { beforeEach, describe, expect, it } from "vitest";

import { PreferencesPage } from "@/features/preferences/preferences-page";
import { readDeveloperSettings, writeDeveloperSettings } from "@/lib/console-preferences";
import { ThemeProvider } from "@/theme/theme-provider";

function renderPage() {
  return render(
    <MemoryRouter>
      <ThemeProvider>
        <PreferencesPage />
      </ThemeProvider>
    </MemoryRouter>,
  );
}

describe("Preferences developer section", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("starts disabled with a zero range", () => {
    renderPage();

    expect(screen.getByRole("switch", { name: "Simulate API latency" })).toHaveAttribute(
      "aria-checked",
      "false",
    );
    expect(screen.getByRole("spinbutton", { name: "Minimum delay (ms)" })).toHaveValue(0);
    expect(screen.getByRole("spinbutton", { name: "Maximum delay (ms)" })).toHaveValue(0);
  });

  it("enables with a suggested range and persists it", async () => {
    const user = userEvent.setup();
    renderPage();

    await user.click(screen.getByRole("switch", { name: "Simulate API latency" }));

    expect(readDeveloperSettings()).toEqual({
      latencyEnabled: true,
      latencyMinMs: 500,
      latencyMaxMs: 2_000,
    });
    expect(screen.getByRole("spinbutton", { name: "Minimum delay (ms)" })).toHaveValue(500);
    expect(screen.getByRole("spinbutton", { name: "Maximum delay (ms)" })).toHaveValue(2_000);

    await user.click(screen.getByRole("switch", { name: "Simulate API latency" }));

    expect(readDeveloperSettings()).toEqual({
      latencyEnabled: false,
      latencyMinMs: 500,
      latencyMaxMs: 2_000,
    });
  });

  it("saves a valid edit and restores it after a fresh mount", async () => {
    const user = userEvent.setup();
    writeDeveloperSettings(window.localStorage, {
      latencyEnabled: true,
      latencyMinMs: 100,
      latencyMaxMs: 200,
    });
    const { unmount } = renderPage();

    const maximum = screen.getByRole("spinbutton", { name: "Maximum delay (ms)" });
    await user.clear(maximum);
    await user.type(maximum, "3000");
    expect(readDeveloperSettings().latencyMaxMs).toBe(200);
    await user.tab();

    expect(readDeveloperSettings()).toEqual({
      latencyEnabled: true,
      latencyMinMs: 100,
      latencyMaxMs: 3_000,
    });

    unmount();
    renderPage();

    expect(screen.getByRole("spinbutton", { name: "Maximum delay (ms)" })).toHaveValue(3_000);
  });

  it("shows an error and keeps the stored range while the minimum exceeds the maximum", () => {
    writeDeveloperSettings(window.localStorage, {
      latencyEnabled: true,
      latencyMinMs: 100,
      latencyMaxMs: 200,
    });
    renderPage();

    const minimum = screen.getByRole("spinbutton", { name: "Minimum delay (ms)" });
    fireEvent.change(minimum, { target: { value: "900" } });
    fireEvent.blur(minimum);

    expect(screen.getByRole("alert")).toHaveTextContent(
      "Minimum delay must not exceed the maximum.",
    );
    expect(minimum).toHaveAttribute("aria-invalid", "true");
    expect(readDeveloperSettings()).toEqual({
      latencyEnabled: true,
      latencyMinMs: 100,
      latencyMaxMs: 200,
    });

    const maximum = screen.getByRole("spinbutton", { name: "Maximum delay (ms)" });
    fireEvent.change(maximum, { target: { value: "1000" } });
    fireEvent.blur(maximum);

    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(readDeveloperSettings()).toEqual({
      latencyEnabled: true,
      latencyMinMs: 900,
      latencyMaxMs: 1_000,
    });
  });

  it("rejects a maximum above the cap and an empty field without saving", () => {
    writeDeveloperSettings(window.localStorage, {
      latencyEnabled: true,
      latencyMinMs: 0,
      latencyMaxMs: 500,
    });
    renderPage();

    const maximum = screen.getByRole("spinbutton", { name: "Maximum delay (ms)" });
    fireEvent.change(maximum, { target: { value: "" } });
    expect(screen.getByRole("alert")).toHaveTextContent("Enter whole milliseconds, 0 or more.");

    fireEvent.change(maximum, { target: { value: "30001" } });
    fireEvent.blur(maximum);
    expect(screen.getByRole("alert")).toHaveTextContent("Maximum delay must be at most 30,000 ms.");
    expect(readDeveloperSettings().latencyMaxMs).toBe(500);
  });
});
