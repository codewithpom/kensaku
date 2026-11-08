// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { formatComma } from "./format";

/* Regression test for #149: formatComma hardcoded "en-US", so the Russian UI
   grouped counts with commas. It must follow the active UI locale, which
   useLanguage mirrors onto <html lang>. */

describe("formatComma", () => {
  afterEach(() => {
    document.documentElement.lang = "";
  });

  it("follows the active UI locale (Russian groups with a space)", () => {
    document.documentElement.lang = "ru";
    expect(formatComma(48213)).toBe("48 213");
  });

  it("keeps comma grouping for English", () => {
    document.documentElement.lang = "en";
    expect(formatComma(48213)).toBe("48,213");
  });
});
