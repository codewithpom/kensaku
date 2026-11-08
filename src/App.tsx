import { useState, useEffect, useRef, useCallback, useMemo } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  SearchResult,
  SearchResponse,
  SnippetDto,
  ActiveFilter,
  IndexingStatus,
  CommandOutput,
  InterpretationChip,
  Facets,
  HomeData,
  KensakuConfig,
  DoctorReport,
} from "./types";
import { SetupModal } from "./components/SetupModal";
import { DuplicatesModal } from "./components/DuplicatesModal";
import { SlashMenu, SlashCommand } from "./components/SlashMenu";
import { ScopeMenu } from "./components/ScopeMenu";
import { HomeView } from "./components/HomeView";
import { ResultItem, ResultView } from "./components/ResultItem";
import { Settings } from "./components/SettingsModal";
import { FilterChips } from "./components/FilterChips";
import { KeyHintStrip } from "./components/KeyHintStrip";
import { ContextMenu, ContextMenuItem } from "./components/ContextMenu";
import { DetailsDialog } from "./components/DetailsDialog";
import { HelpDialog } from "./components/HelpDialog";
import { SearchBar } from "./components/SearchBar";
import { CategoryChips } from "./components/CategoryChips";
import { ResultControls } from "./components/ResultControls";
import { VirtualGrid } from "./components/VirtualGrid";
import { FileRow } from "./components/FileRow";
import { normalizeTheme } from "./hooks/useTheme";
import { useLanguage } from "./hooks/useLanguage";
import { PreviewDialog } from "./components/PreviewDialog";
import { classify } from "./lib/categories";
import { shouldHideOnBlur } from "./lib/dismiss";
import { useMeasuredHeight } from "./lib/useMeasuredHeight";
import { interpretationLabel } from "./lib/interpretation";
import { inFilter } from "./lib/query";
import { isNewerVersion } from "./lib/version";
import { checkForUpdates } from "./lib/updater";
import { getVersion } from "@tauri-apps/api/app";
import {
  Settings as SettingsIcon,
  RefreshCw,
  Copy,
  FileText,
  Image as ImageIcon,
  FileCode,
  Film,
  Clock,
  Terminal,
  Stethoscope,
  TriangleAlert,
  X,
} from "lucide-react";
import "./App.css";

const SNIPPET_LRU_LIMIT = 200;

/** Small insertion-order LRU for snippets: recent hits move to the end. */
function putSnippet(
  cache: Map<string, string | null>,
  key: string,
  value: string | null,
) {
  cache.delete(key);
  cache.set(key, value);
  while (cache.size > SNIPPET_LRU_LIMIT) {
    const oldest = cache.keys().next().value;
    if (oldest === undefined) break;
    cache.delete(oldest);
  }
}

function App() {
  const { t } = useTranslation("search");
  const { t: tc } = useTranslation("common");
  const { t: te } = useTranslation("errors");
  /* Apply the persisted/auto language once at the root. Settings has its own
     picker; this keeps startup and every later window in the same language. */
  useLanguage();
  const [isSetupComplete, setIsSetupComplete] = useState<boolean | null>(null);
  const [query, setQuery] = useState("");

  // The window is recreated on every show, so restore the last query from Rust
  // and keep it there as the user types.
  useEffect(() => {
    invoke<string>("last_query")
      .then((q) => {
        if (q) setQuery(q);
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    invoke("remember_query", { query }).catch(() => {});
  }, [query]);
  const [results, setResults] = useState<SearchResult[]>([]);
  const [total, setTotal] = useState(0);
  const [activeFilters, setActiveFilters] = useState<ActiveFilter[]>([]);
  const [hints, setHints] = useState<string[]>([]);
  const [interpretation, setInterpretation] = useState<InterpretationChip[]>([]);
  const [facets, setFacets] = useState<Facets | undefined>(undefined);
  const [home, setHome] = useState<HomeData | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [saveOpen, setSaveOpen] = useState(false);
  const [saveName, setSaveName] = useState("");
  const [facetInclude, setFacetInclude] = useState<Set<string>>(new Set());
  const [facetExclude, setFacetExclude] = useState<Set<string>>(new Set());
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [indexingStatus, setIndexingStatus] = useState<IndexingStatus | null>(null);
  const [doctorReport, setDoctorReport] = useState<DoctorReport | null>(null);
  const [updateAvailable, setUpdateAvailable] = useState<string | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [settingsSection, setSettingsSection] = useState<string | undefined>(undefined);
  const [showDuplicates, setShowDuplicates] = useState(false);
  const [slashIndex, setSlashIndex] = useState(0);
  const [scopeItems, setScopeItems] = useState<string[]>([]);
  const [scopeIndex, setScopeIndex] = useState(0);
  const [commandOutput, setCommandOutput] = useState<string | null>(null);
  const [sort, setSort] = useState("best");
  /* Right-click menu and the details dialog replace the old preview pane. */
  // Thumbnail setting, read from config. Reloaded whenever Settings closes.
  const [config, setConfig] = useState<KensakuConfig | null>(null);
  useEffect(() => {
    invoke<KensakuConfig>("get_config")
      .then(setConfig)
      .catch(() => undefined);
    // Reload after setup too: the wizard saves the chosen window mode, and a
    // stale launcher-mode snapshot would make app mode hide on the first open
    // or Escape press.
  }, [showSettings, isSetupComplete]);
  /* The backend config is the durable copy of the theme — localStorage does
     not survive the launcher destroying its webview on hide. Re-apply the
     persisted pick on every (re)mount so close/open keeps the chosen finish. */
  useEffect(() => {
    if (config?.theme === undefined || config?.theme === null) return;
    const normalized = normalizeTheme(config.theme);
    document.documentElement.dataset.theme = normalized;
    try {
      localStorage.setItem("kensaku.theme", normalized);
    } catch {
      /* ignore */
    }
  }, [config?.theme]);
  useEffect(() => {
    if (isSetupComplete !== true) return;
    invoke<DoctorReport>("run_doctor")
      .then(setDoctorReport)
      .catch(() => {});
  }, [isSetupComplete]);

  useEffect(() => {
    if (config && config.auto_update === false) {
      setUpdateAvailable(null);
      return;
    }
    /* Compare against the running app's real version, and only flag a
       strictly newer release: "not equal" offered a downgrade whenever the
       latest published release was older than this build. */
    /* Once every 12 hours, with the answer remembered in between. */
    const KEY = "kensaku.updateCheck";
    const TTL_MS = 12 * 60 * 60 * 1000;
    let cached: { at: number; tag: string } | null = null;
    try {
      cached = JSON.parse(localStorage.getItem(KEY) || "null");
    } catch {
      cached = null;
    }
    const fresh = cached && Date.now() - cached.at < TTL_MS;
    const latestTag: Promise<string> = fresh
      ? Promise.resolve(cached!.tag)
      : checkForUpdates()
          .then((info) => {
            const tag = info.latest_version;
            try {
              localStorage.setItem(KEY, JSON.stringify({ at: Date.now(), tag }));
            } catch {
              /* storage unavailable */
            }
            return tag;
          })
          .catch(() => "");
    Promise.all([getVersion(), latestTag])
      .then(([current, tag]) => {
        if (tag && isNewerVersion(tag, current)) setUpdateAvailable(tag);
      })
      .catch(() => {});
  }, [config?.auto_update]);

  const showThumbs = config?.show_thumbnails !== false;
  const view: ResultView = config?.results_view === "list" ? "list" : "grid";

  /* Switching layout is a preference, so it goes to config like the sort
     order does. Extracted from the toolbar handler so the keyboard shortcut
     and the button share one path. */
  const setResultView = useCallback((next: ResultView) => {
    setConfig((c) => (c ? { ...c, results_view: next } : c));
    invoke<KensakuConfig>("get_config")
      .then((cfg) => invoke("save_config", { config: { ...cfg, results_view: next } }))
      .catch(() => undefined);
  }, []);
  const [menu, setMenu] = useState<{ x: number; y: number; result: SearchResult } | null>(null);
  const [details, setDetails] = useState<SearchResult | null>(null);
  const [showHelp, setShowHelp] = useState(false);
  const [preview, setPreview] = useState<SearchResult | null>(null);


  const inputRef = useRef<HTMLInputElement>(null);
  /* The bar floats over the content, so the results are inset by its measured
     height. That height is not constant: the interpretation row, the category
     chips and the toolbar appear only once a query has results, making the bar
     two to three times taller than on the empty screen. See the hook for why
     this cannot be a mount-only measurement. */
  const [barHeight, setBarNode] = useMeasuredHeight<HTMLDivElement>(88);
  const resultsContainerRef = useRef<HTMLDivElement>(null);

  /** Number of cards in the first grid row (1 in list view). Measured from
   *  the DOM so it stays correct across window resizes and auto-fill.
   *  Only `data-index` cells count — full-span labels ("Recent") sit in
   *  the grid too but span every column. */
  const columnsRef = useRef(1);
  const gridColumnCount = useCallback(() => {
    if (columnsRef.current > 0) return columnsRef.current;
    const el = resultsContainerRef.current;
    if (!el) return 1;
    const children = Array.from(el.children).filter((child) =>
      child.hasAttribute("data-index"),
    ) as HTMLElement[];
    if (children.length === 0) return 1;
    const firstTop = children[0].offsetTop;
    let count = 0;
    for (const child of children) {
      if (child.offsetTop !== firstTop) break;
      count += 1;
    }
    return Math.max(1, count);
  }, []);
  const currentSeqRef = useRef(0);
  /* The query the rows on screen were produced for. Kept apart from `query`
     so a keypress during the debounce window cannot act on a stale row. */
  const resultsQueryRef = useRef("");
  /** Query at the time the current effect run scheduled its requests —
   *  snippet merges are discarded if the user has typed since. */
  const queryRef = useRef(query);
  /** Snippets already fetched for a path, across queries ("path → snippet,
   *  null = fetched and none exists"). Kept so the same path never pays for
   *  extraction twice within a session; pruned when it grows past 200. */
  const fetchedSnippetsRef = useRef(new Map<string, string | null>()).current;
  /** Cache keyed by path + mtime + query so a changed file or query refetches. */
  const snippetCacheRef = useRef(new Map<string, string | null>()).current;

  /* ---------------- setup gating ---------------- */

  const checkSetup = useCallback(async () => {
    try {
      const done = await invoke<boolean>("is_setup_complete");
      setIsSetupComplete(done);
    } catch (err) {
      console.error("Failed to check setup:", err);
      setIsSetupComplete(false);
    }
  }, []);

  useEffect(() => {
    checkSetup();
  }, [checkSetup]);

  /* ---------------- home data ---------------- */

  const recordQuery = useCallback((q: string) => {
    const trimmed = q.trim();
    if (!trimmed || trimmed.startsWith("/")) return;
    invoke("record_recent_query", { query: trimmed }).catch(() => {});
  }, []);

  const loadHome = useCallback(async () => {
    try {
      setHome(await invoke<HomeData>("get_home_data"));
    } catch (err) {
      console.error("Home data error:", err);
    }
  }, []);

  const handleClearRecentQueries = useCallback(async () => {
    try {
      await invoke("clear_recent_queries");
      setHome((prev) => (prev ? { ...prev, recent_queries: [] } : prev));
    } catch (err) {
      console.error("Failed to clear recent queries:", err);
    }
  }, []);

  useEffect(() => {
    if (isSetupComplete) loadHome();
  }, [isSetupComplete, loadHome, indexingStatus?.is_indexing]);

  /* ---------------- indexing status (events, not polling) ---------------- */

  useEffect(() => {
    if (!isSetupComplete) return;
    invoke<IndexingStatus>("get_indexing_status")
      .then(setIndexingStatus)
      .catch((err) => console.error("Initial status error:", err));

    let unlisten: (() => void) | undefined;
    listen<IndexingStatus>("indexing-status", (event) => setIndexingStatus(event.payload))
      .then((off) => {
        unlisten = off;
      })
      .catch((err) => console.error("Event listen error:", err));

    return () => {
      if (unlisten) unlisten();
    };
  }, [isSetupComplete]);

  /* ---------------- search (stale-while-revalidate) ---------------- */

  useEffect(() => {
    setFacetInclude(new Set());
    setFacetExclude(new Set());
    setSaveOpen(false);
  }, [query]);

  useEffect(() => {
    if (!isSetupComplete) return;

    const trimmed = query.trim();
    if (trimmed === "") {
      resultsQueryRef.current = "";
      setResults([]);
      setActiveFilters([]);
      setHints([]);
      setInterpretation([]);
      setFacets(undefined);
      setSelectedIndex(0);
      loadHome();
      return;
    }

    if (query.trim().startsWith("/")) {
      resultsQueryRef.current = "";
      setResults([]);
      setActiveFilters([]);
      setHints([]);
      setInterpretation([]);
      setFacets(undefined);
      return;
    }

    const timer = setTimeout(async () => {
      const seq = ++currentSeqRef.current;
      queryRef.current = query;
      try {
        const res = await invoke<SearchResponse>("search", { query: query.trim(), sort });
        if (seq >= currentSeqRef.current) {
          // Seed rows with snippets already fetched for these paths so a
          // repeated path never flashes snippet-less. Existing snippets are
          // never cleared mid-flight — old rows keep theirs until these
          // replace them.
          const seen = new Set<string>();
          const merged = res.results.map((r) => {
            seen.add(r.path);
            const cacheKey = `${r.path}\u0000${r.modified}\u0000${query}`;
            if (r.snippet == null && snippetCacheRef.has(cacheKey)) {
              const cached = snippetCacheRef.get(cacheKey) ?? null;
              putSnippet(snippetCacheRef, cacheKey, cached);
              fetchedSnippetsRef.set(r.path, cached);
              return { ...r, snippet: cached };
            }
            if (r.snippet == null && fetchedSnippetsRef.has(r.path)) {
              return { ...r, snippet: fetchedSnippetsRef.get(r.path) ?? null };
            }
            return r;
          });
          if (fetchedSnippetsRef.size > 200) {
            for (const p of fetchedSnippetsRef.keys()) {
              if (!seen.has(p)) fetchedSnippetsRef.delete(p);
            }
          }
          resultsQueryRef.current = query.trim();
          setResults(merged);
          setTotal(res.total ?? merged.length);
          setActiveFilters(res.active_filters);
          setHints(res.hints);
          setInterpretation(res.interpretation ?? []);
          setFacets(res.facets);
          setSelectedIndex(0);
          if (merged.length > 0 && query.trim()) {
            recordQuery(query);
          }

          // Snippets are fetched by the visibility observer below (or the
          // no-IntersectionObserver fallback) so off-screen rows never pay
          // for content reads.
        }
      } catch (err) {
        console.error("Search error:", err);
      }
    }, 40);

    return () => clearTimeout(timer);
  }, [query, sort, isSetupComplete, loadHome]);

  // Fetch snippets only for rows that are actually visible (or the first 20
  // when IntersectionObserver is unavailable). Results are patched by path
  // and stale responses for superseded queries are discarded.
  useEffect(() => {
    if (!isSetupComplete || results.length === 0) return;
    const container = resultsContainerRef.current;
    if (!container) return;

    const seq = currentSeqRef.current;
    const q = query;
    const pending = new Set<string>();

    const flush = () => {
      const paths = Array.from(pending).slice(0, 20);
      pending.clear();
      if (paths.length === 0) return;
      invoke<SnippetDto[]>("get_snippets", { paths, query: q })
        .then((dtos) => {
          if (seq !== currentSeqRef.current || queryRef.current !== q) return;
          if (!Array.isArray(dtos)) return;
          const patch = new Set<string>();
          for (const dto of dtos) {
            if (dto && typeof dto.path === "string") {
              const result = results.find((r) => r.path === dto.path);
              const key = `${dto.path}\u0000${result?.modified ?? 0}\u0000${q}`;
              putSnippet(
                snippetCacheRef,
                key,
                typeof dto.snippet === "string" ? dto.snippet : null,
              );
              fetchedSnippetsRef.set(
                dto.path,
                typeof dto.snippet === "string" ? dto.snippet : null,
              );
              if (dto.snippet) patch.add(dto.path);
            }
          }
          setResults((prev) => {
            if (seq !== currentSeqRef.current || queryRef.current !== q) return prev;
            let changed = false;
            const next = prev.map((r) => {
              if (r.snippet != null || !patch.has(r.path)) return r;
              changed = true;
              return { ...r, snippet: fetchedSnippetsRef.get(r.path) ?? null };
            });
            return changed ? next : prev;
          });
        })
        .catch(() => {
          /* snippets are optional decoration */
        });
    };

    if (typeof IntersectionObserver === "undefined") {
      const want: string[] = [];
      for (const r of results) {
        if (r.snippet != null) continue;
        const key = `${r.path}\u0000${r.modified}\u0000${q}`;
        if (snippetCacheRef.has(key)) continue;
        want.push(r.path);
        if (want.length >= 20) break;
      }
      if (want.length > 0) {
        for (const path of want) pending.add(path);
        flush();
      }
      return;
    }

    let timer: ReturnType<typeof setTimeout> | null = null;
    const schedule = () => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(flush, 80);
    };

    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (!entry.isIntersecting) continue;
          const index = Number((entry.target as HTMLElement).getAttribute("data-index"));
          const result = results[index];
          if (!result || result.snippet != null) continue;
          const key = `${result.path}\u0000${result.modified}\u0000${q}`;
          if (snippetCacheRef.has(key)) continue;
          if (pending.has(result.path)) continue;
          pending.add(result.path);
        }
        if (pending.size > 0) schedule();
      },
      // Rows are virtualized: the grid only mounts the visible window, so a
      // root of the outer wrapper never intersected anything and rows
      // scrolled into view were never observed. The viewport is the honest
      // root here, and a mutation observer picks up rows mounted later.
      { root: null, threshold: 0.01 },
    );

    const observed = new WeakSet<Element>();
    const observeRows = () => {
      container.querySelectorAll("[data-index]").forEach((el) => {
        if (observed.has(el)) return;
        observed.add(el);
        observer.observe(el);
      });
    };
    observeRows();
    const mutations = new MutationObserver(observeRows);
    mutations.observe(container, { childList: true, subtree: true });

    return () => {
      mutations.disconnect();
      observer.disconnect();
      if (timer) clearTimeout(timer);
    };
  }, [results, query, isSetupComplete]);

  /* Infinite scroll guard. A page that adds nothing new marks this query as
     exhausted, so the grid stops asking. In-flight requests are ref-guarded,
     so repeated near-end signals can't stack up requests. */
  const pageKey = `${query}\u0000${sort}`;
  const [exhaustedKey, setExhaustedKey] = useState<string | null>(null);
  const loadingRef = useRef(false);
  const resultsLenRef = useRef(0);
  resultsLenRef.current = results.length;

  const loadMore = useCallback(async () => {
    if (loadingRef.current || !query.trim() || exhaustedKey === pageKey) return;
    loadingRef.current = true;
    setLoadingMore(true);
    const requestKey = pageKey;
    try {
      const offset = resultsLenRef.current;
      const res = await invoke<SearchResponse>("search", { query: query.trim(), sort, offset });
      // Discard a page for a query the user has already left.
      if (queryRef.current !== query) return;
      let added = 0;
      setResults((prev) => {
        const seen = new Set(prev.map((r) => r.path));
        const fresh = res.results.filter((r) => !seen.has(r.path));
        added = fresh.length;
        return fresh.length ? [...prev, ...fresh] : prev;
      });
      setTotal(res.total ?? 0);
      if (added === 0 || res.results.length === 0) setExhaustedKey(requestKey);
    } catch (err) {
      console.error("Load more error:", err);
      setExhaustedKey(requestKey);
    } finally {
      loadingRef.current = false;
      setLoadingMore(false);
    }
  }, [query, sort, pageKey, exhaustedKey]);

  // Results never reorder while typing — sort is backend-provided and stable;
  // nothing needed here beyond identity-stable render keys.

  /* ---------------- focus + persistence ---------------- */

  useEffect(() => {
    if (isSetupComplete) inputRef.current?.focus();
  }, [isSetupComplete]);


  useEffect(() => {
    if (!resultsContainerRef.current) return;
    const el = resultsContainerRef.current.querySelector<HTMLElement>(
      `[data-index="${selectedIndex}"]`,
    );
    el?.scrollIntoView({ block: "nearest" });
  }, [selectedIndex, view]);

  /* ---------------- actions ---------------- */

  // Launcher mode gets out of the way once a file opens; a desktop-app window
  // stays where it is, like any other app. Until the config arrives, do not
  // hide: an app-mode window must never be mistaken for a launcher.
  const hideAfterOpen = config ? config.app_mode !== "app" : false;

  const handleOpenFile = useCallback(async (path: string) => {
    if (query.trim()) recordQuery(query);
    try {
      await invoke("open_file", { path });
      if (hideAfterOpen) await invoke("hide_window");
    } catch (err) {
      console.error("Open file error:", err);
    }
  }, [hideAfterOpen, query, recordQuery]);

  const handleOpenFolder = useCallback(async (path: string) => {
    if (query.trim()) recordQuery(query);
    try {
      await invoke("open_folder", { path });
      if (hideAfterOpen) await invoke("hide_window");
    } catch (err) {
      console.error("Open folder error:", err);
    }
  }, [hideAfterOpen, query, recordQuery]);

  const handleRunCommand = useCallback(async (cmdStr: string) => {
    try {
      const out = await invoke<CommandOutput>("execute_command", { command: cmdStr });
      if (out.message === "SETTINGS") {
        setShowSettings(true);
      } else if (out.message === "DUPLICATES") {
        setShowDuplicates(true);
      } else {
        setCommandOutput(out.message);
      }
    } catch (err) {
      setCommandOutput(String(err));
    }
  }, []);

  const handleRemoveFilter = useCallback((filter: ActiveFilter) => {
    setQuery((prev) =>
      prev
        .replace(filter.raw, "")
        .replace(/\s+/g, " ")
        .trim(),
    );
    inputRef.current?.focus();
  }, []);

  const handleApplyHint = useCallback((hint: string) => {
    const match = hint.match(/📁\s*([^\?]+)/);
    setQuery((prev) =>
      match && match[1]
        ? `${prev.trim()} ${inFilter(match[1].trim())}`
        : `${prev.trim()} ${hint}`.trim(),
    );
    inputRef.current?.focus();
  }, []);

  /* Category facet: click includes, Alt-click excludes, All clears. */
  const handleFacetToggle = useCallback((id: string, alt: boolean) => {
    if (id === "all") {
      setFacetInclude(new Set());
      setFacetExclude(new Set());
      return;
    }
    setFacetInclude((prev) => {
      const next = new Set(prev);
      if (alt) {
        next.delete(id);
        setFacetExclude((ex) => {
          const e = new Set(ex);
          e.has(id) ? e.delete(id) : e.add(id);
          return e;
        });
      } else {
        next.has(id) ? next.delete(id) : next.add(id);
        setFacetExclude((ex) => {
          const e = new Set(ex);
          e.delete(id);
          return e;
        });
      }
      return next;
    });
  }, []);

  const openSettings = useCallback((section?: string) => {
    setSettingsSection(section);
    setShowSettings(true);
  }, []);

  const [toast, setToast] = useState<string | null>(null);
  const toastTimer = useRef<number | undefined>(undefined);
  const flash = useCallback((msg: string) => {
    setToast(msg);
    window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(null), 1600);
  }, []);
  const handleCopyPath = useCallback(
    (path: string) => {
      navigator.clipboard.writeText(path).then(
        () => flash(t("toast.pathCopied")),
        () => flash(te("copyPath")),
      );
    },
    [flash, t, te],
  );


  /* ---------------- `/` commands ---------------- */

  const slashCommands = useMemo((): SlashCommand[] => {
    const focus = (q: string) => {
      setQuery(q);
      inputRef.current?.focus();
    };
    return [
      { id: "images", name: "images", label: "type:image", description: t("slash.images"), icon: ImageIcon, run: () => focus("type:image ") },
      { id: "pdf", name: "pdf", label: "ext:pdf", description: t("slash.pdf"), icon: FileText, run: () => focus("ext:pdf ") },
      { id: "docs", name: "docs", label: "type:doc", description: t("slash.docs"), icon: FileText, run: () => focus("type:doc ") },
      { id: "code", name: "code", label: "type:code", description: t("slash.code"), icon: FileCode, run: () => focus("type:code ") },
      { id: "media", name: "media", label: "type:media", description: t("slash.media"), icon: Film, run: () => focus("type:media ") },
      { id: "recent", name: "recent", label: "modified:<7d", description: t("slash.recent"), icon: Clock, run: () => focus("modified:<7d ") },
      { id: "errors", name: "errors", label: "error", description: t("slash.errors"), icon: TriangleAlert, run: () => focus("error ") },
      { id: "diagrams", name: "diagrams", label: "diagram", description: t("slash.diagrams"), icon: FileCode, run: () => focus("diagram ") },
      { id: "dupes", name: "dupes", label: "review", description: t("slash.dupes"), icon: Copy, run: () => { setQuery(""); setShowDuplicates(true); } },
      { id: "reindex", name: "reindex", label: "action", description: t("slash.reindex"), icon: RefreshCw, run: () => { setQuery(""); handleRunCommand("/reindex"); } },
      { id: "doctor", name: "doctor", label: "action", description: t("slash.doctor"), icon: Stethoscope, run: () => { setQuery(""); handleRunCommand("/doctor"); } },
      { id: "settings", name: "settings", label: "⌘,", description: t("slash.settings"), icon: SettingsIcon, run: () => { setQuery(""); openSettings(); } },
    ];
  }, [handleRunCommand, openSettings, t]);

  const trimmedQuery = query.trim();
  const slashFilter = trimmedQuery.startsWith("/") ? trimmedQuery.slice(1).trim().toLowerCase() : null;
  const slashItems = useMemo(() => {
    if (slashFilter === null) return [];
    return slashCommands.filter(
      (c) => slashFilter === "" || c.name.includes(slashFilter) || c.description.toLowerCase().includes(slashFilter),
    );
  }, [slashCommands, slashFilter]);

  useEffect(() => {
    setSlashIndex(0);
  }, [slashFilter]);

  /* ---------------- `@` scopes ---------------- */

  const scopePrefix = useMemo(() => {
    if (query.startsWith("/")) return null;
    const m = query.match(/(?:^|\s)@([^\s]*)$/);
    return m ? m[1] : null;
  }, [query]);

  useEffect(() => {
    if (scopePrefix === null) {
      setScopeItems([]);
      return;
    }
    let cancelled = false;
    const timer = setTimeout(() => {
      invoke<string[]>("get_scope_suggestions", { prefix: scopePrefix, limit: 12 })
        .then((items) => {
          if (!cancelled) {
            setScopeItems(items);
            setScopeIndex(0);
          }
        })
        .catch(() => {
          if (!cancelled) setScopeItems([]);
        });
    }, 120);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [scopePrefix]);

  const applyScope = useCallback((path: string) => {
    setQuery((prev) => prev.replace(/@[^\s]*$/, () => `${inFilter(path)} `));
    inputRef.current?.focus();
  }, []);

  /* The result list both the keyboard navigation and the render operate over.
     Declared before the key handlers because their closures read it even on
     renders that return early below (first-launch setup screen): the setup
     render registers the handlers while never reaching the declaration, so a
     late `const navList` made every Space press throw a TDZ error. */
  const displayResults = useMemo(
    () =>
      results.filter((r) => {
        const cat = classify(r);
        if (facetExclude.has(cat)) return false;
        if (facetInclude.size > 0 && !facetInclude.has(cat)) return false;
        return true;
      }),
    [results, facetInclude, facetExclude],
  );
  // The list keyboard navigation operates over — search results while a query
  // is typed, otherwise the home screen's recent tiles. Home items are shaped
  // as search results (no snippet, FileName match) so preview, details and
  // open all share one code path with the search list.
  const hasQuery = query.trim() !== "";
  const homeNavList = useMemo(
    () =>
      (home?.recent ?? []).slice(0, 18).map((item) => ({
        path: item.path,
        filename: item.filename,
        snippet: null,
        score: 0,
        file_size: item.size,
        modified: item.modified,
        is_dir: item.is_dir,
        match_type: "FileName" as const,
      })),
    [home],
  );
  const navList = hasQuery ? displayResults : homeNavList;

  /* Space previews only once the user has moved into the results with the
     arrow keys. Typing clears it, so Space still types a space in a query. */
  const navigatingRef = useRef(false);
  /* Whether the user has entered the list with a navigation key. The home
     tiles draw their focus ring only after this — at first paint the ring
     would sit on tile 0 and the first Tab press would look like a no-op. */
  const [navActive, setNavActive] = useState(false);
  useEffect(() => {
    navigatingRef.current = false;
    setNavActive(false);
  }, [query]);

  // Home and search are different lists: never carry a selection index
  // across the boundary.
  useEffect(() => {
    setSelectedIndex(0);
  }, [hasQuery]);

  const handleKeyDown = useCallback(
    async (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      // An IME is composing a candidate (Chinese, Japanese, Korean). The Enter
      // that commits the candidate is not "open the selected file", and the
      // arrows paging the candidate window are not list navigation. Acting on
      // them opened a random result on every CJK search.
      if (e.isComposing || e.keyCode === 229) return;
      if (document.body.dataset.ksCapture === "1") return;
      // The setup wizard owns the keyboard while it is open.
      if (isSetupComplete !== true) return;
      // Overlays own the keyboard while they are open.
      if (showSettings || showDuplicates || showHelp || preview || details || menu) return;
      const inInput = document.activeElement === inputRef.current;
      const inOtherField =
        !inInput &&
        (document.activeElement instanceof HTMLInputElement || document.activeElement instanceof HTMLTextAreaElement);
      if (inOtherField) return;

      // Rows on screen can still belong to the previous query while the
      // debounce and the search round trip are in flight. Opening, previewing
      // or copying one of them acts on a file the user is no longer looking
      // at, so the result actions below are gated on this.
      const resultsAreStale = resultsQueryRef.current !== query.trim();

      // "/" focuses search when the focus is elsewhere.
      if (e.key === "/" && document.activeElement !== inputRef.current) {
        e.preventDefault();
        inputRef.current?.focus();
        return;
      }

      if (e.key === "," && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        openSettings();
        return;
      }

      if (e.key === "?" && document.activeElement !== inputRef.current) {
        e.preventDefault();
        setShowHelp(true);
        return;
      }

      // `/` command menu
      if (query.trim().startsWith("/")) {
        if (e.key === "ArrowDown") {
          e.preventDefault();
          setSlashIndex((i) => (slashItems.length ? (i + 1) % slashItems.length : 0));
        } else if (e.key === "ArrowUp") {
          e.preventDefault();
          setSlashIndex((i) =>
            slashItems.length ? (i - 1 + slashItems.length) % slashItems.length : 0,
          );
        } else if (e.key === "Enter") {
          e.preventDefault();
          const item = slashItems[slashIndex] ?? slashItems[0];
          if (item) item.run();
        } else if (e.key === "Escape") {
          e.preventDefault();
          setQuery("");
        }
        return;
      }

      // `@` scope menu
      if (scopePrefix !== null && scopeItems.length > 0) {
        if (e.key === "ArrowDown") {
          e.preventDefault();
          setScopeIndex((i) => (i + 1) % scopeItems.length);
          return;
        }
        if (e.key === "ArrowUp") {
          e.preventDefault();
          setScopeIndex((i) => (i - 1 + scopeItems.length) % scopeItems.length);
          return;
        }
        if (e.key === "Enter") {
          e.preventDefault();
          applyScope(scopeItems[scopeIndex] ?? scopeItems[0]);
          return;
        }
        if (e.key === "Escape") {
          e.preventDefault();
          setQuery((prev) => prev.replace(/@[^\s]*$/, "").trimEnd());
          return;
        }
      }

      // Left/Right keep moving the caret while the query is being edited.
      if ((e.key === "ArrowLeft" || e.key === "ArrowRight") && inInput && !navigatingRef.current) return;

      // Tab walks the results only once the user is in the list. Taking every
      // Tab meant nothing in the interface was reachable without a mouse:
      // focus could not leave the result list for the toolbar, chips or sort.
      // When navList is empty (empty search state), let Tab focus into pill buttons.
      if (e.key === "Tab") {
        if (navList.length === 0) return;
        if (!inInput && !navigatingRef.current) return;
        if (inInput && e.shiftKey && !navigatingRef.current) return;
      }

      if (e.key === "ArrowDown" || e.key === "ArrowUp" || e.key === "ArrowLeft" || e.key === "ArrowRight" || e.key === "Tab") {
        // When navList is empty, do not intercept Tab so focus can navigate suggestions/chips.
        if (navList.length === 0) {
          if (e.key === "Tab") return;
          setSelectedIndex(0);
          return;
        }
        // Whether the list was already being walked matters for Tab: entering
        // from the search field lands on the first result instead of skipping
        // to the second. Capture before the flag flips below.
        const enteringFromSearch = !navigatingRef.current;
        e.preventDefault();
        navigatingRef.current = true;
        setNavActive(true);
        // Tab walks the list linearly from the search field: the first press
        // lands on the first result, later presses step forward (Shift+Tab
        // back) with wrap. Grid geometry is deliberately ignored — Tab is
        // always a single-step walk.
        if (e.key === "Tab") {
          if (enteringFromSearch) {
            setSelectedIndex(e.shiftKey ? navList.length - 1 : 0);
          } else {
            setSelectedIndex((prev) =>
              e.shiftKey
                ? (prev - 1 + navList.length) % navList.length
                : (prev + 1) % navList.length,
            );
          }
          return;
        }
        // Grid view navigates by rows first: Up/Down jump a whole row,
        // Left/Right step within the row. List view stays linear with wrap.
        // The home screen is always a linear walk — its tile grid is a
        // different component, so the measured column count may be stale.
        const grid = view === "grid" && hasQuery;
        if (grid) {
          const cols = gridColumnCount();
          if (e.key === "ArrowLeft") {
            setSelectedIndex((prev) => Math.max(0, prev - 1));
          } else if (e.key === "ArrowRight") {
            setSelectedIndex((prev) => Math.min(navList.length - 1, prev + 1));
          } else if (e.key === "ArrowDown") {
            setSelectedIndex((prev) => Math.min(navList.length - 1, prev + cols));
          } else {
            setSelectedIndex((prev) => Math.max(0, prev - cols));
          }
        } else {
          if (e.key === "ArrowDown") {
            setSelectedIndex((prev) => (prev + 1) % navList.length);
          } else if (e.key === "ArrowUp") {
            setSelectedIndex((prev) => (prev - 1 + navList.length) % navList.length);
          }
        }
      } else if (
        navList[selectedIndex] &&
        ((e.key === " " && (!inInput || navigatingRef.current || !query.trim())) ||
          ((e.key === "y" || e.key === "Y") && (e.metaKey || e.ctrlKey)))
      ) {
        // Space after arrowing into results, or Cmd/Ctrl+Y anywhere (Quick Look).
        if (resultsAreStale) return;
        e.preventDefault();
        setPreview(navList[selectedIndex]);
      } else if (e.key === "Enter" && navList[selectedIndex]) {
        if (resultsAreStale) return;
        e.preventDefault();
        const target = navList[selectedIndex];
        if (e.shiftKey) {
          handleOpenFolder(target.path);
        } else {
          handleOpenFile(target.path);
        }
      } else if (
        (e.key === "c" || e.key === "C") &&
        (e.metaKey || e.ctrlKey) &&
        navList[selectedIndex] &&
        // In the search box, only take over when no text is selected.
        (!inInput || inputRef.current!.selectionStart === inputRef.current!.selectionEnd)
      ) {
        // Copy the selected result's path (Cmd/Ctrl+C outside the search box).
        if (resultsAreStale) return;
        e.preventDefault();
        handleCopyPath(navList[selectedIndex].path);
      } else if (
        (e.key === "l" || e.key === "L") &&
        (e.metaKey || e.ctrlKey) &&
        e.shiftKey
      ) {
        // Cmd/Ctrl+Shift+L flips grid and list. Cmd+L alone belongs to the
        // browser/OS and must not be taken.
        e.preventDefault();
        setResultView(view === "grid" ? "list" : "grid");
      } else if ((e.key === "i" || e.key === "I") && (e.metaKey || e.ctrlKey) && navList[selectedIndex]) {
        // Details for the selected result.
        if (resultsAreStale) return;
        e.preventDefault();
        setDetails(navList[selectedIndex]);
      }
    },
    [isSetupComplete, showSettings, showDuplicates, showHelp, details, menu, query, results, selectedIndex, commandOutput, handleRunCommand, handleOpenFile, handleOpenFolder, openSettings, view, setResultView, gridColumnCount, slashItems, slashIndex, scopePrefix, scopeItems, scopeIndex, applyScope, preview],
  );

  // Window-level, so shortcuts keep working after a dialog closes and focus
  // falls back to the page body.
  const keyHandlerRef = useRef(handleKeyDown);
  keyHandlerRef.current = handleKeyDown;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => keyHandlerRef.current(e);
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Master Escape: window-level so it fires even when focus is on body.
  useEffect(() => {
    const onEsc = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      // Escape also cancels an IME candidate; that is not "clear the query".
      if (e.isComposing || e.keyCode === 229) return;
      if (e.defaultPrevented) return;
      // The setup wizard owns the keyboard (its shortcut capture handles Esc).
      if (isSetupComplete !== true) return;
      if (document.body.dataset.ksCapture === "1") return;
      // Let open overlays handle their own Escape.
      if (showSettings || showDuplicates || showHelp || preview || details || saveOpen) return;

      if (slashFilter !== null) {
        e.preventDefault();
        setQuery("");
        return;
      }
      if (scopePrefix !== null) {
        e.preventDefault();
        setQuery((prev) => prev.replace(/@[^\s]*$/, "").trimEnd());
        return;
      }
      if (commandOutput) {
        e.preventDefault();
        setCommandOutput(null);
        return;
      }
      if (query.trim()) {
        e.preventDefault();
        setQuery("");
        inputRef.current?.focus();
        return;
      }
      // Only hide once the mode is known and it is a launcher; an app-mode
      // window keeps its content when the user presses Escape.
      if (!config || config.app_mode === "app") return;
      e.preventDefault();
      invoke("hide_window").catch(() => undefined);
    };
    window.addEventListener("keydown", onEsc);
    return () => window.removeEventListener("keydown", onEsc);
  }, [isSetupComplete, showSettings, showDuplicates, showHelp, preview, details, saveOpen, slashFilter, scopePrefix, commandOutput, query, config?.app_mode]);

  // Dismiss like a launcher: hide when the window loses focus (clicking away,
  // switching apps), unless a modal overlay is open. The setup wizard and
  // native dialogs (folder picker, shortcut capture) also take focus away,
  // and hiding there would destroy the window mid-decision.
  //
  // A blur is only a dismissal when the user moved focus to another
  // application. Right-click and the OS-rendered popups some controls used to
  // open also fire blur while the window is still the active one, and hiding
  // on those made the launcher vanish mid-interaction. Two guards: skip when
  // the document still reports focus, and skip for a short window after a
  // pointer press or context menu, which is when a popup would be opening.
  useEffect(() => {
    // Set on pointerdown / contextmenu. A popup that steals focus opens
    // within a frame or two of the press, so a short quiet period covers it
    // while a genuine click-away still hides normally.
    let lastInteractionAt = Number.NEGATIVE_INFINITY;
    const markInteraction = () => {
      lastInteractionAt = Date.now();
    };

    const onBlur = () => {
      const hide = shouldHideOnBlur({
        setupComplete: isSetupComplete === true,
        appMode: config ? (config.app_mode ?? "launcher") : null,
        overlayOpen: Boolean(
          showSettings || showDuplicates || showHelp || preview || details || saveOpen || menu,
        ),
        documentFocused: document.hasFocus(),
        msSinceInteraction: Date.now() - lastInteractionAt,
      });
      if (!hide) return;
      invoke("hide_window").catch(() => undefined);
    };

    window.addEventListener("pointerdown", markInteraction, true);
    window.addEventListener("contextmenu", markInteraction, true);
    window.addEventListener("blur", onBlur);
    return () => {
      window.removeEventListener("pointerdown", markInteraction, true);
      window.removeEventListener("contextmenu", markInteraction, true);
      window.removeEventListener("blur", onBlur);
    };
  }, [isSetupComplete, showSettings, showDuplicates, showHelp, preview, details, saveOpen, menu, config?.app_mode]);

  /* ---------------- render ---------------- */

  if (isSetupComplete === null) {
    return (
      <div className="flex h-screen items-center justify-center bg-bg" />
    );
  }

  const isCommandMode = query.trim().startsWith("/");

  const noResults = hasQuery && !isCommandMode && displayResults.length === 0;

  return (
    <div
      className="relative flex h-screen w-screen select-none flex-col overflow-hidden bg-bg text-fg"
      onContextMenu={(e) => e.preventDefault()}
    >
      {isSetupComplete === false && (
        <SetupModal
          onSetupCompleted={() => {
            setIsSetupComplete(true);
            window.setTimeout(() => inputRef.current?.focus(), 120);
          }}
        />
      )}

      {isSetupComplete === true && (
        <>
          {showDuplicates && <DuplicatesModal onClose={() => setShowDuplicates(false)} />}
          {menu && (
            <ContextMenu
              x={menu.x}
              y={menu.y}
              onClose={() => setMenu(null)}
              items={[
                { label: t("contextMenu.open"), onSelect: () => handleOpenFile(menu.result.path) },
                { label: t("contextMenu.preview"), onSelect: () => setPreview(menu.result) },
                { label: t("contextMenu.reveal"), onSelect: () => handleOpenFolder(menu.result.path) },
                { label: t("contextMenu.copyPath"), onSelect: () => handleCopyPath(menu.result.path) },
                { label: t("contextMenu.details"), onSelect: () => setDetails(menu.result), separatorBefore: true },
              ] as ContextMenuItem[]}
            />
          )}
          {details && <DetailsDialog result={details} onClose={() => setDetails(null)} />}
          {showHelp && <HelpDialog onClose={() => setShowHelp(false)} />}
          {preview && (
            <PreviewDialog
              result={preview}
              onOpen={() => handleOpenFile(preview.path)}
              onClose={() => setPreview(null)}
            />
          )}

          {/* Search bar */}
          {showSettings && (
            <div className="absolute inset-0 z-40">
              <Settings
                onClose={() => setShowSettings(false)}
                initialSection={settingsSection}
                onConfigSaved={() => {
                  invoke<IndexingStatus>("get_indexing_status").then(setIndexingStatus);
                }}
              />
            </div>
          )}

          {!showSettings && (
            <>
             <div className="relative min-h-0 flex-1">
              <div ref={setBarNode} className="bar absolute inset-x-0 top-0 z-20">
              <SearchBar
                query={query}
                setQuery={setQuery}
                inputRef={inputRef}
                onFocusInput={() => {}}
                resultCount={results.length}
                total={total}
                trailing={
                  <button
                    type="button"
                    title={t("settingsTitle")}
                    onClick={() => openSettings()}
                    className="btn btn-ghost btn-icon h-9 w-9"
                  >
                    <SettingsIcon className="h-4 w-4" strokeWidth={1.75} />
                  </button>
                }
              />

              {slashFilter !== null && (
                <SlashMenu items={slashItems} activeIndex={slashIndex} onPick={(c) => c.run()} />
              )}
              {scopePrefix !== null && (
                <ScopeMenu items={scopeItems} activeIndex={scopeIndex} onPick={applyScope} />
              )}


              {!isCommandMode && interpretation.length > 0 && (
                <div className="flex shrink-0 flex-wrap items-center gap-1.5 px-3.5 pb-1.5">
                  <span className="t-meta mr-1 text-subtle">{t("understoodAs")}</span>
                  {interpretation.map((c, i) => (
                    <button
                      key={`${c.raw}-${i}`}
                      type="button"
                      title={t("removeInterpretation", { source: c.source })}
                      onClick={() =>
                        setQuery((prev) =>
                          prev.replace(c.source, "").replace(/\s+/g, " ").trim(),
                        )
                      }
                      className="group flex h-6 items-center gap-1.5 rounded-full bg-accent-soft px-2.5 text-[12px] text-fg shadow-[inset_0_0_0_1px_var(--border)] transition-colors hover:text-danger"
                    >
                      {interpretationLabel(c, t)}
                      <X className="h-2.5 w-2.5 opacity-50 group-hover:opacity-100" />
                    </button>
                  ))}
                </div>
              )}

              {/* One control bar: category chips on the left, layout and save on the right. */}
              {!isCommandMode && hasQuery && (
                <div className="fade-in flex shrink-0 items-center gap-2 px-3.5 pb-1.5">
                  <CategoryChips
                    results={results}
                    facets={facets}
                    selected={facetInclude}
                    excluded={facetExclude}
                    onToggle={handleFacetToggle}
                  />
                  <ResultControls
                    sort={sort}
                    onSort={setSort}
                    view={view}
                    onView={setResultView}
                    canSave={displayResults.length > 0}
                    onSave={() => setSaveOpen((v) => !v)}
                  />
                </div>
              )}

              {saveOpen && (
                <div className="overlay-in flex shrink-0 items-center gap-2 px-3.5 pb-1.5">
                  <input
                    autoFocus
                    value={saveName}
                    placeholder={t("saveName")}
                    onChange={(e) => setSaveName(e.target.value)}
                    onKeyDown={async (e) => {
                      if (e.key !== "Enter") return;
                      const name = saveName.trim();
                      if (!name) return;
                      try {
                        const cfg = await invoke<KensakuConfig>("get_config");
                        cfg.saved_searches = [
                          ...(cfg.saved_searches ?? []),
                          { name, query, sort },
                        ];
                        await invoke("save_config", { config: cfg });
                        setSaveName("");
                        setSaveOpen(false);
                        loadHome();
                      } catch (err) {
                        console.error(err);
                      }
                    }}
                    className="field h-8 min-w-0 flex-1"
                  />
                  <button
                    type="button"
                    onClick={async () => {
                      const name = saveName.trim();
                      if (!name) return;
                      try {
                        const cfg = await invoke<KensakuConfig>("get_config");
                        cfg.saved_searches = [
                          ...(cfg.saved_searches ?? []),
                          { name, query, sort },
                        ];
                        await invoke("save_config", { config: cfg });
                        setSaveName("");
                        setSaveOpen(false);
                        loadHome();
                      } catch (err) {
                        console.error(err);
                      }
                    }}
                    className="btn btn-primary h-8"
                  >
                    {tc("actions.save")}
                  </button>
                </div>
              )}

              {/* Active backend filters + hints */}
              {!isCommandMode && (
                <FilterChips
                  filters={activeFilters}
                  hints={hints}
                  onRemoveFilter={handleRemoveFilter}
                  onApplyHint={handleApplyHint}
                />
              )}

              {/* Command output banner */}
              {commandOutput && (
                <div className="surface overlay-in mx-3.5 mb-1.5 flex shrink-0 items-start justify-between gap-3 px-3.5 py-2.5">
                  <div className="flex min-w-0 items-start gap-2">
                    <Terminal className="mt-0.5 h-3.5 w-3.5 shrink-0 text-muted" />
                    <pre className="select-text whitespace-pre-wrap break-all font-mono text-[11px] leading-relaxed text-muted">
                      {commandOutput}
                    </pre>
                  </div>
                  <button
                    type="button"
                    onClick={() => setCommandOutput(null)}
                    className="shrink-0 text-subtle transition-colors hover:text-fg"
                  >
                    <X className="h-3.5 w-3.5" />
                  </button>
                </div>
              )}

              </div>

              {/* Content scrolls underneath the bar. */}
              <div className="absolute inset-0 z-10 flex overflow-hidden">
                <div
                  ref={resultsContainerRef}
                  style={displayResults.length > 0 ? undefined : { paddingTop: barHeight }}
                  className={`flex min-h-0 min-w-0 flex-1 flex-col ${
                    !hasQuery || displayResults.length === 0 ? "overflow-y-auto" : ""
                  }`}
                >
                  {displayResults.length > 0 ? (
                    <VirtualGrid
                      items={displayResults}
                      getKey={(res) => res.path}
                      cardHeight={(w) => w * 0.625 + 56}
                      list={view === "list"}
                      insetTop={barHeight}
                      selectedIndex={selectedIndex}
                      hasMore={results.length < total && exhaustedKey !== pageKey}
                      loading={loadingMore}
                      onNearEnd={loadMore}
                      onColumns={(cols) => {
                        columnsRef.current = cols;
                      }}
                      renderItem={(res, idx) =>
                        view === "list" ? (
                          <FileRow
                            item={{ path: res.path, filename: res.filename, file_size: res.file_size, modified: res.modified, is_dir: res.is_dir }}
                            query={query}
                            selected={idx === selectedIndex}
                            showThumbs={showThumbs}
                            onSelect={() => setSelectedIndex(idx)}
                            onOpen={() => handleOpenFile(res.path)}
                            onContextMenu={(x, y) => setMenu({ x, y, result: res })}
                          />
                        ) : (
                          <ResultItem
                            result={res}
                            query={query}
                            index={idx}
                            isSelected={idx === selectedIndex}
                            view="grid"
                            onSelect={() => setSelectedIndex(idx)}
                            onOpen={() => handleOpenFile(res.path)}
                            onReveal={() => handleOpenFolder(res.path)}
                            onCopyPath={() => handleCopyPath(res.path)}
                            onContextMenu={(x, y) => setMenu({ x, y, result: res })}
                            showThumbs={showThumbs}
                          />
                        )
                      }
                    />
                  ) : hasQuery ? (noResults ? (
                    <div className="flex h-full flex-col items-start justify-end gap-4 px-4 pb-8 select-none">
                      <p className="t-moment rise-in max-w-lg text-muted">
                        {t("noResultsNothing", { query: query.trim() })}
                      </p>
                      <div className="flex flex-wrap items-center gap-1.5">
                        {hasActiveFilters(query) && (
                          <SuggestChip label={t("removeFilters")} onClick={() => setQuery("")} />
                        )}
                        <SuggestChip
                          label={t("tryPdf")}
                          onClick={() => {
                            setQuery("type:pdf");
                            inputRef.current?.focus();
                          }}
                        />
                        <SuggestChip
                          label={t("onlyFolders")}
                          onClick={() => {
                            setQuery("type:dir");
                            inputRef.current?.focus();
                          }}
                        />
                      </div>
                    </div>
                  ) : null
                  ) : (
                    <HomeView
                      data={home}
                      recentQueries={home?.recent_queries}
                      onClearRecentQueries={handleClearRecentQueries}
                      onOpen={(path) => handleOpenFile(path)}
                      showThumbs={showThumbs}
                      focusIndex={hasQuery || !navActive ? null : selectedIndex}
                      onSearch={(q) => {
                        recordQuery(q);
                        setQuery(q);
                        inputRef.current?.focus();
                      }}
                    />
                  )}

                </div>
              </div>

             </div>

              {toast && (
                <div className="pointer-events-none absolute inset-x-0 bottom-12 z-[90] flex justify-center">
                  <span className="elevated overlay-in !rounded-full px-4 py-2 text-[13px] font-medium text-fg">{toast}</span>
                </div>
              )}

              <KeyHintStrip status={indexingStatus} doctor={doctorReport} updateAvailable={updateAvailable} onDoctor={() => { setShowSettings(true); setSettingsSection("doctor"); }} onUpdate={() => { setShowSettings(true); setSettingsSection("about"); }} />
            </>
          )}
        </>
      )}
    </div>
  );
}

/* Chips rendered under a no-results illustration. */
function SuggestChip({ label, onClick }: { label: string; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="btn btn-secondary rise-in h-8 !rounded-full"
    >
      {label}
    </button>
  );
}

function hasActiveFilters(q: string): boolean {
  return /\b(type|ext|in|modified|size):/i.test(q);
}

export default App;
