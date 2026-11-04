/**
 * JSON values whose objects keep their members as written: in order, duplicates included. The
 * inference API reads question order from key order and rejects duplicate names, which
 * `JSON.parse` would hide by moving integer-like keys first and keeping only the last duplicate.
 */
export type JsonValue = string | number | boolean | null | JsonValue[] | JsonObject;

/** A JSON object as its members, in the order written. */
export interface JsonObject {
  fields: [string, JsonValue][];
}

export function jsonObject(fields: [string, JsonValue][]): JsonObject {
  return { fields };
}

export function isJsonObject(value: JsonValue): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Parses JSON text keeping object members as written, or nothing when the text isn't JSON or is
 * nested too deeply to read.
 */
export function parseJson(text: string): JsonValue | undefined {
  try {
    // The standard parser checks the syntax, so the reader only walks valid JSON. The reader
    // recurses per nesting level, so nesting the standard parser accepts can exhaust the stack.
    JSON.parse(text);
    return new Reader(text).value();
  } catch {
    return undefined;
  }
}

/** Writes a value with two-space indentation, as `JSON.stringify(value, null, 2)` would. */
export function writeJson(value: JsonValue, indent = ""): string {
  if (!Array.isArray(value) && !isJsonObject(value)) return JSON.stringify(value);

  const inner = `${indent}  `;
  const items = Array.isArray(value)
    ? value.map((item) => `${inner}${writeJson(item, inner)}`)
    : value.fields.map(
        ([key, item]) => `${inner}${JSON.stringify(key)}: ${writeJson(item, inner)}`,
      );
  const [open, close] = Array.isArray(value) ? ["[", "]"] : ["{", "}"];
  return items.length === 0
    ? `${open}${close}`
    : `${open}\n${items.join(",\n")}\n${indent}${close}`;
}

/** A JSON number, `true`, `false` or `null`, matched where the reader stands. */
const literalPattern = /true|false|null|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/y;

/** Walks text already known to be valid JSON. */
class Reader {
  private position = 0;

  constructor(private readonly text: string) {}

  value(): JsonValue {
    this.skipSpace();
    const char = this.text[this.position];
    if (char === "{") return this.object();
    if (char === "[") return this.array();
    if (char === '"') return this.string();
    return this.literal();
  }

  private object(): JsonObject {
    const fields: [string, JsonValue][] = [];
    this.position += 1;
    this.skipSpace();
    if (this.text[this.position] === "}") {
      this.position += 1;
      return { fields };
    }
    for (;;) {
      this.skipSpace();
      const key = this.string();
      this.skipSpace();
      this.position += 1; // The colon.
      fields.push([key, this.value()]);
      this.skipSpace();
      if (this.text[this.position++] === "}") return { fields };
    }
  }

  private array(): JsonValue[] {
    const items: JsonValue[] = [];
    this.position += 1;
    this.skipSpace();
    if (this.text[this.position] === "]") {
      this.position += 1;
      return items;
    }
    for (;;) {
      items.push(this.value());
      this.skipSpace();
      if (this.text[this.position++] === "]") return items;
    }
  }

  private string(): string {
    const start = this.position;
    this.position += 1;
    while (this.text[this.position] !== '"') {
      this.position += this.text[this.position] === "\\" ? 2 : 1;
    }
    this.position += 1;
    return JSON.parse(this.text.slice(start, this.position)) as string;
  }

  private literal(): JsonValue {
    literalPattern.lastIndex = this.position;
    const token = literalPattern.exec(this.text)![0];
    this.position += token.length;
    return JSON.parse(token) as JsonValue;
  }

  private skipSpace() {
    while (/\s/.test(this.text[this.position] ?? "")) this.position += 1;
  }
}
