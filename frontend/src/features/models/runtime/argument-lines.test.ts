import { describe, expect, it } from "vitest";

import { formatArgumentLines, parseArgumentLines } from "./argument-lines";

describe("parseArgumentLines", () => {
  it("gives the same tokens for one argument per line and a pasted command line", () => {
    const perLine = parseArgumentLines("--temp\n0.7\n\n--top-k\n40\n--jinja\n");
    const pasted = parseArgumentLines("--temp 0.7   --top-k 40 --jinja");

    expect(perLine).toEqual(["--temp", "0.7", "--top-k", "40", "--jinja"]);
    expect(pasted).toEqual(perLine);
  });

  it("keeps spaces inside quotes and drops the quotes", () => {
    expect(
      parseArgumentLines(`--system-prompt "be brief, please" --stop '###  end' --alias a\\ b`),
    ).toEqual(["--system-prompt", "be brief, please", "--stop", "###  end", "--alias", "a b"]);
  });

  it("keeps an empty quoted argument and escaped quotes inside double quotes", () => {
    expect(parseArgumentLines(`--prefix "" --quote "say \\"hi\\""`)).toEqual([
      "--prefix",
      "",
      "--quote",
      'say "hi"',
    ]);
  });

  it("reads an unterminated quote to the end of its line", () => {
    expect(parseArgumentLines(`--name "two words\n--next`)).toEqual([
      "--name",
      "two words",
      "--next",
    ]);
  });
});

describe("parseArgumentLines continuations", () => {
  it("reads a trailing backslash as a line continuation, like the per-line form", () => {
    expect(parseArgumentLines("--temp 0.7 \\\n--top-k 40")).toEqual([
      "--temp",
      "0.7",
      "--top-k",
      "40",
    ]);
    expect(parseArgumentLines("--temp 0.7\\\n --top-k 40")).toEqual([
      "--temp",
      "0.7",
      "--top-k",
      "40",
    ]);
  });

  it("keeps an escaped backslash at a line end", () => {
    expect(parseArgumentLines("a\\\\\nb")).toEqual(["a\\", "b"]);
  });
});

describe("formatArgumentLines", () => {
  it("writes one token per line, quoting only tokens that need it, and reads back the same tokens", () => {
    const tokens = ["--temp", "0.7", "--system-prompt", "be brief", "", "it's", 'say "hi"', "a\\b"];

    const text = formatArgumentLines(tokens);

    expect(text.split("\n")).toEqual([
      "--temp",
      "0.7",
      "--system-prompt",
      "'be brief'",
      "''",
      `'it'\\''s'`,
      `'say "hi"'`,
      "'a\\b'",
    ]);
    expect(parseArgumentLines(text)).toEqual(tokens);
  });
});
