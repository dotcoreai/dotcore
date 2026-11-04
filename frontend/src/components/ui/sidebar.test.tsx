import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import {
  Sidebar,
  SidebarContent,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarRail,
} from "@/components/ui/sidebar";

// window.matchMedia is stubbed globally in src/test/setup.ts, since the console shell now
// renders the sidebar on every route.
describe("Sidebar primitive", () => {
  it("renders the sidebar shell expanded by default with a reachable menu button", () => {
    render(
      <SidebarProvider>
        <Sidebar>
          <SidebarContent>
            <SidebarMenu>
              <SidebarMenuItem>
                <SidebarMenuButton>Machines</SidebarMenuButton>
              </SidebarMenuItem>
            </SidebarMenu>
          </SidebarContent>
        </Sidebar>
      </SidebarProvider>,
    );

    const sidebar = document.querySelector('[data-slot="sidebar"]');
    expect(sidebar).not.toBeNull();
    expect(sidebar).toHaveAttribute("data-state", "expanded");
    expect(sidebar).toHaveAttribute("data-side", "left");

    const menuButton = screen.getByRole("button", { name: "Machines" });
    expect(menuButton).toHaveAttribute("data-slot", "sidebar-menu-button");
  });

  // Guards the click-to-toggle behavior investigated after an annotation reported the rail as
  // dead: jsdom cannot prove the resting-visibility or cursor styling fix (no CSS layout, no
  // pseudo-elements), so this pins the one part of that fix that is behavioral rather than visual.
  it("toggles the sidebar open state when the edge rail is clicked", async () => {
    const user = userEvent.setup();
    render(
      <SidebarProvider>
        <Sidebar>
          <SidebarContent />
          <SidebarRail />
        </Sidebar>
      </SidebarProvider>,
    );

    const sidebar = document.querySelector('[data-slot="sidebar"]');
    const rail = screen.getByRole("button", { name: "Toggle Sidebar" });
    expect(sidebar).toHaveAttribute("data-state", "expanded");

    await user.click(rail);
    expect(sidebar).toHaveAttribute("data-state", "collapsed");

    await user.click(rail);
    expect(sidebar).toHaveAttribute("data-state", "expanded");
  });
});
