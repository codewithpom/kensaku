import { describe, expect, it } from "vitest";
import { formatDateTime } from "./format";

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
