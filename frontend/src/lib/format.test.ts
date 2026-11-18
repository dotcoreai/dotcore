import { describe, expect, it } from "vitest";

import {
  formatByteRange,
  formatBytes,
  formatCompactCount,
  formatCount,
  formatExactSize,
  formatExactTokens,
  formatQuantity,
  formatRelativeTime,
  parseSize,
} from "./format";

describe("formatBytes", () => {
  it("renders zero as whole bytes", () => {
    expect(formatBytes(0n)).toBe("0 bytes");
  });

  it("renders a single byte in the singular", () => {
    expect(formatBytes(1n)).toBe("1 byte");
  });

  it("keeps the plural for every other unit at magnitude one", () => {
    expect(formatBytes(1024n)).toBe("1 KiB");
    expect(formatBytes(2n)).toBe("2 bytes");
  });

  it("stays in bytes just below the KiB boundary", () => {
    expect(formatBytes(1023n)).toBe("1,023 bytes");
  });

  it("crosses each binary-unit boundary at exactly 1024x the previous unit", () => {
    expect(formatBytes(1024n)).toBe("1 KiB");
    expect(formatBytes(1024n ** 2n)).toBe("1 MiB");
    expect(formatBytes(1024n ** 3n)).toBe("1 GiB");
    expect(formatBytes(1024n ** 4n)).toBe("1 TiB");
  });

  it("renders a TiB-scale value with one fractional digit", () => {
    expect(formatBytes(1649267441664n)).toBe("1.5 TiB"); // 1.5 * 1024^4
  });

  it("does not climb past TiB, the largest unit the console formats", () => {
    expect(formatBytes(1024n ** 5n)).toBe("1,024 TiB");
  });
});

describe("formatByteRange", () => {
  it("names a shared unit once", () => {
    expect(formatByteRange(17_288_000_000n, 35_218_000_000n)).toBe("16.1–32.8 GiB");
  });

  it("collapses ends that read the same", () => {
    expect(formatByteRange(1024n ** 3n, 1024n ** 3n + 1n)).toBe("1 GiB");
  });

  it("keeps each unit when the ends differ in unit", () => {
    expect(formatByteRange(512n * 1024n ** 2n, 2n * 1024n ** 3n)).toBe("512 MiB–2 GiB");
  });
});

describe("formatQuantity", () => {
  it("climbs byte units to the largest fitting unit", () => {
    expect(formatQuantity(65536n, "MiB")).toBe("64 GiB");
    expect(formatQuantity(1536, "MiB")).toBe("1.5 GiB");
    expect(formatQuantity(512, "KiB")).toBe("512 KiB");
    expect(formatQuantity(2048n, "bytes")).toBe("2 KiB");
  });

  it("keeps a byte quantity below the next boundary in its unit", () => {
    expect(formatQuantity(1000, "MiB")).toBe("1,000 MiB");
  });

  it("compacts a tokens quantity without its unit name", () => {
    expect(formatQuantity(250000n, "tokens")).toBe("250k");
    expect(formatQuantity(8192, "tokens")).toBe("8k");
  });

  it("groups digits for any other unit and omits an empty unit", () => {
    expect(formatQuantity(12345n, "layers")).toBe("12,345 layers");
    expect(formatQuantity(7, "")).toBe("7");
  });
});

describe("formatCompactCount", () => {
  it("keeps counts under one thousand as is", () => {
    expect(formatCompactCount(0)).toBe("0");
    expect(formatCompactCount(999)).toBe("999");
  });

  it("counts decimal thousands", () => {
    expect(formatCompactCount(250000)).toBe("250k");
    expect(formatCompactCount(30000)).toBe("30k");
  });

  it("counts binary thousands for multiples of 1024", () => {
    expect(formatCompactCount(1024)).toBe("1k");
    expect(formatCompactCount(131072)).toBe("128k");
  });

  it("switches to millions", () => {
    expect(formatCompactCount(1_500_000)).toBe("1.5M");
    expect(formatCompactCount(1024 ** 2)).toBe("1M");
    expect(formatCompactCount(3405 * 1024)).toBe("3.3M");
    expect(formatCompactCount(1_999_872)).toBe("1.9M");
    expect(formatCompactCount(999_600)).toBe("1M");
  });
});

describe("parseSize", () => {
  it("reads a bare number in the given unit", () => {
    expect(parseSize("12595", "MiB")).toBe(12595);
    expect(parseSize(" 233895 ", "tokens")).toBe(233895);
  });

  it("converts binary byte prefixes, with or without b or ib, into the unit", () => {
    expect(parseSize("6g", "MiB")).toBe(6144);
    expect(parseSize("6 GiB", "MiB")).toBe(6144);
    expect(parseSize("6GB", "MiB")).toBe(6144);
    expect(parseSize("512m", "MiB")).toBe(512);
    expect(parseSize("1t", "GiB")).toBe(1024);
    expect(parseSize("2k", "bytes")).toBe(2048);
    expect(parseSize("1.5 KiB", "bytes")).toBe(1536);
    expect(parseSize("100b", "bytes")).toBe(100);
  });

  it("keeps decimals that land on a whole number of the unit", () => {
    expect(parseSize("12.25g", "MiB")).toBe(12544);
    expect(parseSize("0.5t", "GiB")).toBe(512);
  });

  it("refuses a size that isn't whole in the unit", () => {
    expect(parseSize("512m", "GiB")).toBeUndefined();
    expect(parseSize("0.1g", "MiB")).toBeUndefined();
    expect(parseSize("1.5", "MiB")).toBeUndefined();
    expect(parseSize("100b", "MiB")).toBeUndefined();
  });

  it("reads binary k and m token counts", () => {
    expect(parseSize("1.5k", "tokens")).toBe(1536);
    expect(parseSize("128K", "tokens")).toBe(131072);
    expect(parseSize("1m", "tokens")).toBe(1048576);
    expect(parseSize("1.3k", "tokens")).toBeUndefined();
  });

  it("refuses text that isn't a size", () => {
    expect(parseSize("", "MiB")).toBeUndefined();
    expect(parseSize("12 apples", "MiB")).toBeUndefined();
    expect(parseSize("-1g", "MiB")).toBeUndefined();
    expect(parseSize("6g", "tokens")).toBeUndefined();
    expect(parseSize("1,024", "tokens")).toBeUndefined();
  });
});

describe("formatExactSize", () => {
  it("climbs to the largest unit that shows the size with at most two decimals", () => {
    expect(formatExactSize(12288, "MiB")).toBe("12 GiB");
    expect(formatExactSize(12544, "MiB")).toBe("12.25 GiB");
    expect(formatExactSize(2 * 1024 ** 2, "MiB")).toBe("2 TiB");
    expect(formatExactSize(1536, "bytes")).toBe("1.5 KiB");
  });

  it("stays in its own unit when no larger one is exact", () => {
    expect(formatExactSize(12595, "MiB")).toBe("12595 MiB");
    expect(formatExactSize(512, "MiB")).toBe("512 MiB");
    expect(formatExactSize(12, "GiB")).toBe("12 GiB");
    expect(formatExactSize(0, "MiB")).toBe("0 MiB");
  });

  it("reads back to the same size", () => {
    for (const value of [12288, 12544, 12595, 1536])
      expect(parseSize(formatExactSize(value, "MiB"), "MiB")).toBe(value);
  });
});

describe("formatExactTokens", () => {
  it("counts binary thousands and millions when exact", () => {
    expect(formatExactTokens(131072)).toBe("128k");
    expect(formatExactTokens(1_999_872)).toBe("1953k");
    expect(formatExactTokens(1024 ** 2)).toBe("1M");
  });

  it("keeps digits otherwise", () => {
    expect(formatExactTokens(233895)).toBe("233895");
    expect(formatExactTokens(512)).toBe("512");
  });
});

describe("formatCount", () => {
  it("uses the plural noun for zero and for more than one", () => {
    expect(formatCount(0, "service")).toBe("0 services");
    expect(formatCount(2, "service")).toBe("2 services");
  });

  it("uses the singular noun for exactly one", () => {
    expect(formatCount(1, "service")).toBe("1 service");
  });

  it("uses an explicit plural noun when given", () => {
    expect(formatCount(1, "copy", "copies")).toBe("1 copy");
    expect(formatCount(3, "copy", "copies")).toBe("3 copies");
  });
});

describe("formatRelativeTime", () => {
  const now = new Date("2026-08-06T18:35:00.000Z");

  function secondsAgo(seconds: number): { nanos: number; seconds: bigint } {
    return { nanos: 0, seconds: BigInt(Math.round(now.getTime() / 1000) - seconds) };
  }

  it("renders a title of the timestamp's own absolute UTC time, not the reference time's", () => {
    expect(formatRelativeTime(secondsAgo(30), now).title).toBe("2026-08-06 18:34:30 UTC");
  });

  it("renders seconds-range elapsed time", () => {
    expect(formatRelativeTime(secondsAgo(30), now).text).toBe("30 seconds ago");
  });

  it("renders minutes-range elapsed time", () => {
    expect(formatRelativeTime(secondsAgo(5 * 60), now).text).toBe("5 minutes ago");
  });

  it("renders hours-range elapsed time", () => {
    expect(formatRelativeTime(secondsAgo(3 * 60 * 60), now).text).toBe("3 hours ago");
  });

  it("renders days-range elapsed time", () => {
    expect(formatRelativeTime(secondsAgo(2 * 24 * 60 * 60), now).text).toBe("2 days ago");
  });

  it("renders a title matching a timestamp's own date, not the reference time's", () => {
    const twoDaysAgo = secondsAgo(2 * 24 * 60 * 60);
    expect(formatRelativeTime(twoDaysAgo, now).title).toBe("2026-08-04 18:35:00 UTC");
  });
});
