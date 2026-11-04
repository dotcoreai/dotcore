/**
 * OKLCH color math for verifying the console token palette.
 *
 * Parses the `oklch(L C H)` strings written in the token stylesheet and computes WCAG relative
 * luminance and contrast ratios directly from them, so palette contrast can be asserted by a test
 * against the same values the stylesheet declares rather than a hand-copied duplicate.
 */

export interface Oklch {
  l: number;
  c: number;
  h: number;
}

const OKLCH_PATTERN = /oklch\(\s*(\d*\.?\d+)\s+(\d*\.?\d+)\s+(\d*\.?\d+)\s*\)/i;

export function parseOklch(css: string): Oklch {
  const match = OKLCH_PATTERN.exec(css);
  if (!match) {
    throw new Error(`Not an oklch(L C H) value: ${css}`);
  }

  const [, l, c, h] = match;
  const oklch = { l: Number(l), c: Number(c), h: Number(h) };
  if (!Number.isFinite(oklch.l) || !Number.isFinite(oklch.c) || !Number.isFinite(oklch.h)) {
    throw new Error(`Not an oklch(L C H) value: ${css}`);
  }

  return oklch;
}

// Inverse of Björn Ottosson's OKLab transform: OKLCH -> linear sRGB, unclamped.
function oklchToLinearSrgb({ l, c, h }: Oklch): [number, number, number] {
  const hueRadians = (h * Math.PI) / 180;
  const a = c * Math.cos(hueRadians);
  const b = c * Math.sin(hueRadians);

  const lPrime = l + 0.3963377774 * a + 0.2158037573 * b;
  const mPrime = l - 0.1055613458 * a - 0.0638541728 * b;
  const sPrime = l - 0.0894841775 * a - 1.291485548 * b;

  const lCubed = lPrime ** 3;
  const mCubed = mPrime ** 3;
  const sCubed = sPrime ** 3;

  return [
    4.0767416621 * lCubed - 3.3077115913 * mCubed + 0.2309699292 * sCubed,
    -1.2684380046 * lCubed + 2.6097574011 * mCubed - 0.3413193965 * sCubed,
    -0.0041960863 * lCubed - 0.7034186147 * mCubed + 1.707614701 * sCubed,
  ];
}

// A browser gamut-maps an out-of-sRGB OKLCH color, preserving hue, rather than clipping each
// channel independently. Per-channel clipping computes a contrast ratio for a color the browser
// never renders, so an out-of-gamut token must be reported here rather than quietly approximated.
const GAMUT_EPSILON = 1e-4;

function clampChannel(value: number): number {
  if (value < -GAMUT_EPSILON || value > 1 + GAMUT_EPSILON) {
    throw new Error(
      `Linear sRGB channel ${value} is out of the [0, 1] gamut by more than the epsilon; the ` +
        "source OKLCH token is out of sRGB gamut and per-channel clamping would not match how a " +
        "browser renders it.",
    );
  }

  return Math.min(1, Math.max(0, value));
}

/**
 * Perceptual distance between two six-digit hex colors: the Euclidean distance between their
 * OKLab coordinates. About 0.02 is the smallest difference people notice side by side.
 */
export function oklabDistance(a: string, b: string): number {
  const labA = hexToOklab(a);
  const labB = hexToOklab(b);
  return Math.hypot(labA[0] - labB[0], labA[1] - labB[1], labA[2] - labB[2]);
}

// Björn Ottosson's OKLab transform, from a gamma-encoded hex color.
function hexToOklab(hex: string): [number, number, number] {
  const [r, g, b] = linearChannelsFromHex(hex);
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);

  return [
    0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
    1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
    0.0259040371 * l + 0.7827717662 * m - 0.808675778 * s,
  ];
}

// WCAG relative luminance from a linear-light sRGB channel (already 0-1, no gamma applied).
function relativeLuminance(oklch: Oklch): number {
  const [r, g, b] = oklchToLinearSrgb(oklch).map(clampChannel);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function relativeLuminanceFromHex(hex: string): number {
  const linear = linearChannelsFromHex(hex);
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}

function linearChannelsFromHex(hex: string): number[] {
  const match = /^#([0-9a-f]{6})$/i.exec(hex);
  if (!match) throw new Error(`Not a six-digit hex color: ${hex}`);
  const channels = [0, 2, 4].map(
    (offset) => Number.parseInt(match[1].slice(offset, offset + 2), 16) / 255,
  );
  return channels.map((channel) =>
    channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4,
  );
}

function relativeLuminanceFromCss(css: string): number {
  return css.startsWith("#") ? relativeLuminanceFromHex(css) : relativeLuminance(parseOklch(css));
}

/** WCAG contrast ratio (1-21) between oklch() or six-digit hex CSS colors. */
export function contrastRatio(a: string, b: string): number {
  const luminanceA = relativeLuminanceFromCss(a);
  const luminanceB = relativeLuminanceFromCss(b);
  const [lighter, darker] =
    luminanceA > luminanceB ? [luminanceA, luminanceB] : [luminanceB, luminanceA];

  return (lighter + 0.05) / (darker + 0.05);
}
