import { describe, expect, it } from "vitest";

import {
  integerBounds,
  parameterInputError,
  parameterInputValue,
  type ParameterView,
} from "./parameter-view";

describe("text and secret parameter inputs", () => {
  const password: ParameterView = {
    id: "password",
    label: "Password",
    type: { kind: "secret", maximumLength: 72, minimumLength: 12 },
  };
  const username: ParameterView = {
    id: "username",
    label: "Username",
    type: { kind: "text", minimumLength: 1 },
  };

  it("counts UTF-8 bytes against the declared maximum", () => {
    expect(parameterInputValue(password, "p".repeat(72))).toBe("p".repeat(72));
    for (const tooLong of ["p".repeat(73), "é".repeat(72)]) {
      expect(parameterInputValue(password, tooLong)).toBeUndefined();
      expect(parameterInputError(password, tooLong)).toBe("Password must be 12 to 72 bytes.");
    }
  });

  it("accepts `=` and any length when no maximum is declared", () => {
    expect(parameterInputValue(password, "c2VjcmV0LXBhc3N3b3Jk==")).toBe("c2VjcmV0LXBhc3N3b3Jk==");
    expect(parameterInputValue(username, "u".repeat(200_000))).toBe("u".repeat(200_000));
    expect(parameterInputError(username, "")).toBe("Username must be at least 1 byte.");
  });

  it("refuses control characters", () => {
    expect(parameterInputValue(username, "ad\nmin")).toBeUndefined();
    expect(parameterInputError(username, "ad\tmin")).toBe(
      "Username can't contain control characters.",
    );
  });
});

describe("unsigned integer parameter inputs", () => {
  const bounded: ParameterView = {
    id: "parallel-requests",
    label: "Parallel requests",
    type: { kind: "unsignedInteger", maximum: 2048n, minimum: 1n, unit: "requests" },
  };
  const open: ParameterView = {
    id: "kv-offloading-size-gib",
    label: "KV offloading size",
    type: { kind: "unsignedInteger", minimum: 0n, unit: "GiB" },
  };

  it("bounds a value by the declared range, with digit separators in the error", () => {
    expect(parameterInputValue(bounded, "2048")).toBe(2048n);
    expect(parameterInputValue(bounded, "2049")).toBeUndefined();
    expect(parameterInputError(bounded, "0")).toBe(
      "Parallel requests must be an integer from 1 to 2,048 requests.",
    );
    expect(integerBounds({ maximum: 2048n, minimum: 1n })).toBe("1–2,048");
  });

  it("accepts any value from the minimum when no maximum is declared", () => {
    expect(parameterInputValue(open, "18446744073709551615")).toBe(18446744073709551615n);
    expect(parameterInputError(open, "1.5")).toBe(
      "KV offloading size must be an integer of 0 or more GiB.",
    );
    expect(integerBounds({ minimum: 1024n })).toBe("1,024 or more");
  });
});
