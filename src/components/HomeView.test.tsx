// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { HomeData } from "../types";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("../lib/thumbnails", () => ({
  isThumbImage: () => false,
  useThumbnail: () => ({ src: null, failed: false }),
}));

import { HomeView } from "./HomeView";

describe("HomeView", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("renders fresh install state when recent files and recent queries are empty", () => {
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);

    const data: HomeData = {
      recent: [],
      pins: [],
      saved_searches: [],
      recent_queries: [],
    };

    act(() => {
      root.render(
        <HomeView
          data={data}
          onSearch={() => {}}
          onOpen={() => {}}
        />,
      );
    });

    expect(container.textContent).toContain("emptyHint");
    expect(container.textContent).toContain("suggestions.resume");
    expect(container.textContent).not.toContain("recentSearches");

    act(() => root.unmount());
    container.remove();
  });

  it("renders recent search pills and suggestions when recent files is empty and queries < 3", () => {
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);

    const onSearch = vi.fn();
    const onClearRecentQueries = vi.fn();

    const data: HomeData = {
      recent: [],
      pins: [],
      saved_searches: [],
      recent_queries: ["quarterly report"],
    };

    act(() => {
      root.render(
        <HomeView
          data={data}
          onSearch={onSearch}
          onOpen={() => {}}
          onClearRecentQueries={onClearRecentQueries}
        />,
      );
    });

    expect(container.textContent).toContain("recentSearches");
    expect(container.textContent).toContain("clearRecentSearches");
    expect(container.textContent).toContain("quarterly report");
    // Fewer than 3 queries: suggestions kept below
    expect(container.textContent).toContain("emptyHint");
    expect(container.textContent).toContain("suggestions.resume");

    // Click pill
    const pill = Array.from(container.querySelectorAll("button")).find(
      (b) => b.textContent?.trim() === "quarterly report",
    );
    expect(pill).toBeDefined();
    act(() => {
      pill?.click();
    });
    expect(onSearch).toHaveBeenCalledWith("quarterly report");

    // Click clear
    const clearBtn = Array.from(container.querySelectorAll("button")).find(
      (b) => b.textContent?.trim() === "clearRecentSearches",
    );
    expect(clearBtn).toBeDefined();
    act(() => {
      clearBtn?.click();
    });
    expect(onClearRecentQueries).toHaveBeenCalled();

    act(() => root.unmount());
    container.remove();
  });

  it("renders recent search pills without suggestions below when queries >= 3", () => {
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);

    const data: HomeData = {
      recent: [],
      pins: [],
      saved_searches: [],
      recent_queries: ["query 1", "query 2", "query 3"],
    };

    act(() => {
      root.render(
        <HomeView
          data={data}
          onSearch={() => {}}
          onOpen={() => {}}
        />,
      );
    });

    expect(container.textContent).toContain("recentSearches");
    expect(container.textContent).toContain("query 1");
    expect(container.textContent).toContain("query 2");
    expect(container.textContent).toContain("query 3");
    // When >= 3, suggestions are omitted
    expect(container.textContent).not.toContain("emptyHint");

    act(() => root.unmount());
    container.remove();
  });

  it("renders quiet row for recent searches when recent files exist", () => {
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);

    const onSearch = vi.fn();
    const onClearRecentQueries = vi.fn();

    const data: HomeData = {
      recent: [
        {
          path: "/test/file.pdf",
          filename: "file.pdf",
          size: 1024,
          modified: 1000,
          is_dir: false,
          last_open: 1000,
          opens: 1,
        },
      ],
      pins: [],
      saved_searches: [{ name: "Invoices", query: "invoice" }],
      recent_queries: ["tax 2025"],
    };

    act(() => {
      root.render(
        <HomeView
          data={data}
          onSearch={onSearch}
          onOpen={() => {}}
          onClearRecentQueries={onClearRecentQueries}
        />,
      );
    });

    // Contains saved search row
    expect(container.textContent).toContain("saved");
    expect(container.textContent).toContain("Invoices");

    // Contains quiet recent searches row
    expect(container.textContent).toContain("recentSearches");
    expect(container.textContent).toContain("tax 2025");
    expect(container.textContent).toContain("clearRecentSearches");

    // Contains recent files section
    expect(container.textContent).toContain("recent");
    expect(container.textContent).toContain("file.pdf");

    // Click recent query link
    const queryLink = Array.from(container.querySelectorAll("button")).find(
      (b) => b.textContent?.trim() === "tax 2025",
    );
    expect(queryLink).toBeDefined();
    act(() => {
      queryLink?.click();
    });
    expect(onSearch).toHaveBeenCalledWith("tax 2025");

    act(() => root.unmount());
    container.remove();
  });
});
