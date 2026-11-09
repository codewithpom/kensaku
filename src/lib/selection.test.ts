import { describe, expect, it } from "vitest";
import { rangeSelect, toggle } from "./selection";

describe("toggle", () => {
  it("adds a missing path and removes a present one without mutating", () => {
    const base = new Set(["a"]);
    expect([...toggle(base, "b")]).toEqual(["a", "b"]);
    expect([...toggle(base, "a")]).toEqual([]);
    expect([...base]).toEqual(["a"]);
  });
});

describe("rangeSelect", () => {
  const paths = ["a", "b", "c", "d", "e"];

  it("selects from the anchor to the target in either direction", () => {
    expect([...rangeSelect(paths, 1, 3)]).toEqual(["b", "c", "d"]);
    expect([...rangeSelect(paths, 3, 1)]).toEqual(["b", "c", "d"]);
  });

  it("is just the one row when anchor and target match", () => {
    expect([...rangeSelect(paths, 2, 2)]).toEqual(["c"]);
  });

  it("clamps to the list", () => {
    expect([...rangeSelect(paths, 3, 99)]).toEqual(["d", "e"]);
  });
});
