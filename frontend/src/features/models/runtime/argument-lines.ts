/**
 * Splits custom engine arguments typed one per line, or pasted as a command line, into argument
 * tokens. Each line splits with shell word rules: whitespace separates tokens, single quotes keep
 * their text as is, double quotes keep spaces and take a backslash before `"` or `\`, and a
 * backslash outside quotes keeps the next character. A backslash ending a line continues the command
 * on the next one. Empty lines give no token.
 */
export function parseArgumentLines(text: string): string[] {
  // An odd run of backslashes before a newline ends in an unescaped one, a shell continuation.
  const joined = text.replace(/(?<!\\)((?:\\\\)*)\\\r?\n/g, "$1 ");
  return joined.split("\n").flatMap(parseLine);
}

/**
 * Writes argument tokens one per line, single-quoting a token that holds whitespace, quotes or
 * backslashes, or is empty, so `parseArgumentLines` reads back the same tokens.
 */
export function formatArgumentLines(tokens: readonly string[]): string {
  return tokens.map(quoteArgument).join("\n");
}

/** One argument as a shell reads it back: as is when plain, otherwise single-quoted. */
export function quoteArgument(token: string): string {
  if (token && !/[\s'"\\]/.test(token)) return token;
  return `'${token.replaceAll("'", `'\\''`)}'`;
}

function parseLine(line: string): string[] {
  const tokens: string[] = [];
  let token = "";
  // Whether a token has started, so a quoted empty argument still counts.
  let started = false;
  let quote: "'" | '"' | undefined;

  for (let index = 0; index < line.length; index += 1) {
    const char = line[index];
    if (quote === "'") {
      if (char === "'") quote = undefined;
      else token += char;
    } else if (quote === '"') {
      if (char === '"') quote = undefined;
      else if (char === "\\" && (line[index + 1] === '"' || line[index + 1] === "\\")) {
        index += 1;
        token += line[index];
      } else token += char;
    } else if (/\s/.test(char)) {
      if (started) tokens.push(token);
      token = "";
      started = false;
    } else if (char === "'" || char === '"') {
      quote = char;
      started = true;
    } else if (char === "\\" && index + 1 < line.length) {
      index += 1;
      token += line[index];
      started = true;
    } else {
      token += char;
      started = true;
    }
  }
  if (started) tokens.push(token);
  return tokens;
}
