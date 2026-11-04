import { expect } from "vitest";

/**
 * Asserts that a page's headings form the console outline: one `h1` naming the page, then section
 * and card levels that descend one step at a time. A page that jumps from `h1` straight to `h3`
 * fails, which is what makes the outline navigable by heading rather than by eye.
 *
 * Levels are read from the rendered roles, never from class names, so a heading styled to look
 * smaller still has to sit at its real level.
 */
export function expectHeadingOutline(container: HTMLElement, expected: number[]) {
  const levels = [...container.querySelectorAll("h1, h2, h3, h4, h5, h6")]
    .filter((heading) => !heading.closest("[role='complementary'], [role='dialog']"))
    .map((heading) => Number(heading.tagName.slice(1)));

  expect(levels).toEqual(expected);
  expect(levels[0]).toBe(1);

  const skipped = levels.filter((level, index) => index > 0 && level - levels[index - 1] > 1);
  expect(skipped).toEqual([]);
}
