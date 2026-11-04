import { describe, expect, it } from "vitest";

import { parseJson, writeJson } from "@/features/chat/decisions/ordered-json";

describe("ordered JSON", () => {
  it("keeps object members as written, integer-like and repeated keys included", () => {
    expect(parseJson('{"b": 1, "10": 2, "b": 3, "1": {}}')).toEqual({
      fields: [
        ["b", 1],
        ["10", 2],
        ["b", 3],
        ["1", { fields: [] }],
      ],
    });
  });

  it("reads every value kind, with escapes and whitespace", () => {
    expect(
      parseJson(' [ "a\\"b\\\\", "\\u00e9}", -1.5e2, 0, true, false, null, [], [ [ 1 ] ] ]\n'),
    ).toEqual(['a"b\\', "é}", -150, 0, true, false, null, [], [[1]]]);
  });

  it("reads nothing from text that isn't JSON", () => {
    expect(parseJson('{"a": 1,}')).toBeUndefined();
    expect(parseJson("")).toBeUndefined();
  });

  it("reads nothing from JSON nested deeper than the reader can walk", () => {
    const depth = 100_000;
    const text = `${"[".repeat(depth)}${"]".repeat(depth)}`;
    expect(() => JSON.parse(text) as unknown).not.toThrow();

    expect(parseJson(text)).toBeUndefined();
  });

  it("writes back the text JSON.stringify would indent, members in order", () => {
    const text = '{"2":[1,true,null],"1":{"x":"y"},"e":{},"l":[]}';
    expect(writeJson(parseJson(text)!)).toBe(
      [
        "{",
        '  "2": [',
        "    1,",
        "    true,",
        "    null",
        "  ],",
        '  "1": {',
        '    "x": "y"',
        "  },",
        '  "e": {},',
        '  "l": []',
        "}",
      ].join("\n"),
    );
  });
});
