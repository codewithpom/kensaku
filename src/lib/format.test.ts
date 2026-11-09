// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { formatComma, formatDateTime } from "./format";

/* Regression test for #149: formatComma hardcoded "en-US", so the Russian UI
   grouped counts with commas. It must follow the active UI locale, which
   useLanguage mirrors onto <html lang>. */

describe("formatComma", () => {
  afterEach(() => {
    document.documentElement.lang = "";
  });

  it("follows the active UI locale (Russian groups with a space)", () => {
    document.documentElement.lang = "ru";
    expect(formatComma(48213)).toBe("48\u00a0213");
  });

  it("keeps comma grouping for English", () => {
    document.documentElement.lang = "en";
    expect(formatComma(48213)).toBe("48,213");
  });
});

describe("formatDateTime", () => {
  it("treats seconds and milliseconds as the same instant", () => {
    const secs = 1_794_055_550;
    expect(formatDateTime(secs, "en")).toBe(formatDateTime(secs * 1000, "en"));
  });

  it("accepts an ISO string", () => {
    expect(formatDateTime("2026-11-07T12:00:00Z", "en")).toMatch(/2026/);
  });

  it("is empty for missing or invalid input", () => {
    expect(formatDateTime(0, "en")).toBe("");
    expect(formatDateTime(null, "en")).toBe("");
    expect(formatDateTime("not a date", "en")).toBe("not a date");
  });

  it("follows the requested language", () => {
    expect(formatDateTime(1_794_055_550, "zh")).not.toBe(formatDateTime(1_794_055_550, "en"));
  });
});
