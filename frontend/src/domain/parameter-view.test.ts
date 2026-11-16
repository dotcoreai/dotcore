import { describe, expect, it } from "vitest";

import { parameterInputError, parameterInputValue, type ParameterView } from "./parameter-view";

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
