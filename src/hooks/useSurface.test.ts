import { describe, expect, it } from "vitest";
import { effectiveSurface, normalizeSurface } from "./useSurface";

describe("normalizeSurface", () => {
  it("keeps the known finishes and treats anything else as solid", () => {
    expect(normalizeSurface("frost")).toBe("frost");
    expect(normalizeSurface("glass")).toBe("glass");
    expect(normalizeSurface("solid")).toBe("solid");
    expect(normalizeSurface("liquid")).toBe("solid");
    expect(normalizeSurface(null)).toBe("solid");
    expect(normalizeSurface(undefined)).toBe("solid");
  });
});

describe("effectiveSurface", () => {
  it("paints the pick when blur works and transparency is welcome", () => {
    expect(effectiveSurface("glass", false, true)).toBe("glass");
  });

  it("falls back to solid when the OS reduces transparency", () => {
    expect(effectiveSurface("frost", true, true)).toBe("solid");
  });

  it("falls back to solid when the webview cannot blur", () => {
    expect(effectiveSurface("glass", false, false)).toBe("solid");
  });
});
