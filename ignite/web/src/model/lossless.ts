// Lossless JSON for server responses (S3B). `JSON.parse` turns every number into an IEEE
// double, which silently corrupts u64 sequence numbers above 2^53. This small, strict parser
// returns sequence-bearing fields as `bigint` straight from their source text; every other
// number is an ordinary JS number (coordinates, counts).

/** Fields that carry ledger sequence numbers (u64 on the protocol side). */
export const SEQUENCE_KEYS = new Set(["server_seq", "head_server_seq", "through_server_seq", "next_after"]);

export class JsonSyntaxError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "JsonSyntaxError";
  }
}

export function parseLossless(text: string): unknown {
  let i = 0;
  const fail = (m: string): never => {
    throw new JsonSyntaxError(`${m} at ${i}`);
  };
  const ws = () => {
    while (i < text.length && " \t\n\r".includes(text[i]!)) i++;
  };
  const value = (key: string | null): unknown => {
    ws();
    const c = text[i];
    if (c === "{") return object();
    if (c === "[") return array(key);
    if (c === '"') return string();
    if (c === "t" && text.startsWith("true", i)) return (i += 4), true;
    if (c === "f" && text.startsWith("false", i)) return (i += 5), false;
    if (c === "n" && text.startsWith("null", i)) return (i += 4), null;
    return number(key);
  };
  const object = () => {
    const o: Record<string, unknown> = {};
    i++;
    ws();
    if (text[i] === "}") return i++, o;
    for (;;) {
      ws();
      if (text[i] !== '"') fail("expected key");
      const k = string();
      ws();
      if (text[i] !== ":") fail("expected ':'");
      i++;
      if (Object.prototype.hasOwnProperty.call(o, k)) fail(`duplicate key ${k}`);
      o[k] = value(k);
      ws();
      if (text[i] === ",") {
        i++;
        continue;
      }
      if (text[i] === "}") return i++, o;
      fail("expected ',' or '}'");
    }
  };
  const array = (key: string | null) => {
    const a: unknown[] = [];
    i++;
    ws();
    if (text[i] === "]") return i++, a;
    for (;;) {
      a.push(value(key));
      ws();
      if (text[i] === ",") {
        i++;
        continue;
      }
      if (text[i] === "]") return i++, a;
      fail("expected ',' or ']'");
    }
  };
  const string = (): string => {
    const start = i;
    i++;
    let escaped = false;
    for (; i < text.length; i++) {
      const ch = text[i]!;
      if (escaped) escaped = false;
      else if (ch === "\\") escaped = true;
      else if (ch === '"') {
        i++;
        return JSON.parse(text.slice(start, i)) as string;
      } else if (ch < " ") fail("control character in string");
    }
    return fail("unterminated string");
  };
  const number = (key: string | null): number | bigint => {
    const m = /^-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?/.exec(text.slice(i));
    if (!m) return fail("unexpected token");
    i += m[0].length;
    if (key !== null && SEQUENCE_KEYS.has(key)) {
      if (m[2] || m[3] || m[0].startsWith("-")) fail(`${key} must be a non-negative integer literal`);
      return BigInt(m[0]);
    }
    return Number(m[0]);
  };
  const v = value(null);
  ws();
  if (i !== text.length) fail("trailing characters");
  return v;
}
