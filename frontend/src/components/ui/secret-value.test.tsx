import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { SecretValue } from "@/components/ui/secret-value";
import * as clipboard from "@/lib/clipboard";
import { setupUser } from "@/test/user";

const SECRET = "hunter2-sentinel";

function attributeText(root: HTMLElement): string {
  return [...root.querySelectorAll("*")]
    .flatMap((element) => [...element.attributes].map((attribute) => attribute.value))
    .join("\n");
}

describe("SecretValue", () => {
  const writeClipboard = vi.fn<typeof clipboard.writeClipboard>();

  beforeEach(() => {
    writeClipboard.mockReset().mockResolvedValue(undefined);
    vi.spyOn(clipboard, "writeClipboard").mockImplementation(writeClipboard);
  });

  it("keeps the value out of the DOM text and accessible names until revealed", async () => {
    const user = setupUser();
    const { container } = render(<SecretValue label="Password" value={SECRET} />);

    expect(container.textContent).not.toContain(SECRET);
    expect(attributeText(container)).not.toContain(SECRET);
    expect(screen.getByRole("button", { name: "Reveal Password" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Copy Password" })).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Reveal Password" }));
    expect(screen.getByText(SECRET)).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Hide Password" }));
    expect(container.textContent).not.toContain(SECRET);
  });

  it("copies the value without revealing it", async () => {
    const user = setupUser();
    const { container } = render(<SecretValue label="Password" value={SECRET} />);

    await user.click(screen.getByRole("button", { name: "Copy Password" }));

    expect(writeClipboard).toHaveBeenCalledExactlyOnceWith(SECRET);
    expect(container.textContent).not.toContain(SECRET);
    expect(screen.getByRole("button", { name: "Reveal Password" })).toHaveAttribute(
      "aria-pressed",
      "false",
    );
  });
});
