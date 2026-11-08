import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  Bot,
  Bug,
  Check,
  Copy,
  Filter,
  FolderTree,
  Info,
  Plus,
  RefreshCw,
  Save,
  Download,
  CheckCircle2,
  AlertCircle,
  ExternalLink,
  Search,
  SlidersHorizontal,
  Sparkles,
  Stethoscope,
  X,
} from "lucide-react";
import {
  KensakuConfig,
  ClientStatus,
  InstallOutcome,
} from "../types";
import { useTheme } from "../hooks/useTheme";
import { useLanguage } from "../hooks/useLanguage";
import type { SupportedLanguage } from "../i18n";
import { useStats } from "../hooks/useStats";
import { Card, Field, Switch, Stepper, SectionHeader, reasonFrom } from "./settings/kit";
import { WindowModePicker } from "./WindowModePicker";
import { OcrField } from "./settings/ocr";
import { StatsForNerds } from "./settings/stats";
import { DoctorSection } from "./settings/doctor";
import { ModelFilesCard } from "./settings/models";
import { formatBytes, formatComma } from "../lib/format";
import { openGitHubBugReport } from "../lib/githubIssue";
import {
  checkForUpdates,
  downloadUpdate,
  getUpdaterStatus,
  installUpdateAndRestart,
  formatReleaseDate,
} from "../lib/updater";
import type { UpdateInfo, UpdateStatus } from "../types";
import { IndexingBanner } from "./IndexingBanner";

export interface SettingsProps {
  onClose: () => void;
  initialSection?: string;
  onConfigSaved: () => void;
}

type SectionId =
  | "general"
  | "folders"
  | "exclusions"
  | "search"
  | "ai"
  | "beta"
  | "agents"
  | "stats"
  | "doctor"
  | "about";

const SECTIONS = [
  { id: "general", labelKey: "sections.general", icon: SlidersHorizontal },
  { id: "folders", labelKey: "sections.folders", icon: FolderTree },
  { id: "exclusions", labelKey: "sections.exclusions", icon: Filter },
  { id: "search", labelKey: "sections.search", icon: Search },
  { id: "ai", labelKey: "sections.ai", icon: Sparkles },
  { id: "agents", labelKey: "sections.agents", icon: Bot },
  { id: "doctor", labelKey: "sections.doctor", icon: Stethoscope },
  { id: "about", labelKey: "sections.about", icon: Info },
] as const;

const EXCLUSION_PRESETS: Record<string, string[]> = {
  Developer: [
    "node_modules", ".git", "target", "dist", "build", "out",
    ".next", ".nuxt", ".output", ".cache", ".venv", "venv",
    "__pycache__", "vendor", ".gradle", ".idea", ".vscode",
  ],
  Creative: [
    "node_modules", ".git", ".Trash", ".cache", "Backups",
    "Time Machine", "Library", "lost+found", ".dub", "_archive",
  ],
  "All-round": [
    "node_modules", ".git", "target", "dist", "build", "out",
    ".cache", ".venv", "__pycache__", ".Trash", "Backups",
    "lost+found", ".idea", ".vscode", "Library",
  ],
};

/* Full overlay settings page: 200px rail + bento sections.
   Config edits are local until Save; theme applies immediately. */

export function Settings({ onClose, initialSection, onConfigSaved }: SettingsProps) {
  const { t } = useTranslation("settings");
  const { t: te } = useTranslation("errors");
  const [section, setSection] = useState<SectionId>(
    (SECTIONS.find((s) => s.id === initialSection)?.id ?? "general") as SectionId,
  );
  const [config, setConfig] = useState<KensakuConfig | null>(null);
  /* Loaded snapshot — serialized once. A JSON compare marks the save bar. */
  const [snapshot, setSnapshot] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const [discardOpen, setDiscardOpen] = useState(false);
  const dirty = config !== null && snapshot !== null && JSON.stringify(config) !== snapshot;

  useEffect(() => {
    invoke<KensakuConfig>("get_config")
      .then((cfg) => {
        setConfig(cfg);
        setSnapshot(JSON.stringify(cfg));
      })
      .catch((err) => console.error("Failed to load config:", err));
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      // Escape used to close the panel and throw the edits away with no
      // prompt. Ask first when there is something to lose.
      if (dirty) {
        setDiscardOpen(true);
        return;
      }
      onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, dirty]);

  const handleSave = async (reindex: boolean) => {
    if (!config) return;
    setSaving(true);
    try {
      await invoke("save_config", { config });
      setSnapshot(JSON.stringify(config));
      if (reindex) {
        await invoke("complete_setup", {
          watchedDirs: config.watched_dirs,
          excludedDirs: config.excluded_dirs,
          shortcut: config.shortcut,
        });
      }
      onConfigSaved();
      if (reindex) {
        onClose();
      } else {
        setSaved(true);
        window.setTimeout(() => setSaved(false), 1600);
      }
    } catch (err) {
      console.error("Save failed:", err);
      alert(te("saveSettings", { error: String(err) }));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="fade-in absolute inset-0 z-40 flex bg-bg">
      <nav data-tauri-drag-region className="relative z-10 flex w-[200px] shrink-0 flex-col overflow-y-auto px-3 pb-3 pt-5">
        <p data-tauri-drag-region className="t-title px-3 pb-4 text-fg">{t("title")}</p>
        <div className="flex flex-col gap-0.5">
          {SECTIONS.map((s) => {
            const active = section === s.id;
            const Icon = s.icon;
            return (
              <button
                key={s.id}
                type="button"
                onClick={() => setSection(s.id)}
                aria-current={active ? "page" : undefined}
                className={`relative flex h-8 w-full items-center gap-2.5 rounded-control px-2.5 text-[13px] transition-colors duration-150 ${
                  active ? "control font-medium text-fg" : "text-muted hover:text-fg"
                }`}
              >
                <Icon className={`h-4 w-4 shrink-0 ${active ? "text-fg" : "text-subtle"}`} strokeWidth={1.75} />
                {t(s.labelKey)}
              </button>
            );
          })}
        </div>
      </nav>

      <main className="surface relative z-10 m-2 ml-0 flex min-w-0 flex-1 flex-col overflow-hidden rounded-sheet">
        <button
          type="button"
          title={t("close")}
          onClick={onClose}
          className="btn btn-ghost btn-icon absolute right-4 top-4 z-20 h-8 !rounded-full"
        >
          <X className="h-4 w-4" strokeWidth={1.75} />
        </button>
        <div className="relative min-h-0 flex-1 overflow-y-auto px-6 pb-6 pt-6" style={{ scrollbarGutter: "stable" }}>
          <div className="mx-auto max-w-[640px]">
            {config ? (
              <SectionShell section={section}>{renderSection(section, config, setConfig)}</SectionShell>
            ) : (
              <ConfigSkeleton />
            )}
          </div>
        </div>

        <footer className="relative z-10 flex shrink-0 flex-col gap-2 pb-3">
          <div className="control mx-auto flex items-center gap-2 rounded-full py-1.5 pl-4 pr-1.5">
            <span className="flex items-center gap-2 pr-2 text-[12px] text-muted">
              <span className={`h-1.5 w-1.5 shrink-0 rounded-full transition-colors ${dirty ? "bg-warning" : "bg-success/70"}`} />
              {dirty ? t("footer.unsaved") : saved ? t("footer.saved") : t("footer.upToDate")}
            </span>
            <button type="button" onClick={() => handleSave(true)} disabled={saving || !config} className="btn btn-ghost h-8 !rounded-full px-3 text-[12px]">
              <RefreshCw className="h-3.5 w-3.5" strokeWidth={1.75} />
              {t("footer.saveReindex")}
            </button>
            <button type="button" onClick={() => handleSave(false)} disabled={saving || !config || !dirty} className="btn btn-primary h-8 !rounded-full px-4 text-[12px]">
              {saved ? <Check className="h-3.5 w-3.5" /> : <Save className="h-3.5 w-3.5" strokeWidth={1.75} />}
              {saved ? t("footer.saved") : saving ? t("footer.saving") : t("footer.save")}
            </button>
          </div>

          {/* Indexing is announced at the bottom of the panel. The top edge is
              where the close button lives, and the main window's status line is
              hidden behind this overlay, so this is the one place a running
              scan can be seen without getting in the way. */}
          <IndexingBanner />
        </footer>
        {discardOpen && (
          <DiscardDialog
            onKeep={() => setDiscardOpen(false)}
            onDiscard={() => {
              setDiscardOpen(false);
              onClose();
            }}
          />
        )}
      </main>
    </div>
  );
}

function renderSection(
  section: SectionId,
  config: KensakuConfig,
  setConfig: React.Dispatch<React.SetStateAction<KensakuConfig | null>>,
): React.ReactNode {
  switch (section) {
    case "general":
      return <GeneralSection config={config} setConfig={setConfig} />;
    case "folders":
      return <FoldersSection config={config} setConfig={setConfig} />;
    case "exclusions":
      return <ExclusionsSection config={config} setConfig={setConfig} />;
    case "search":
      return <SearchSection config={config} setConfig={setConfig} />;
    case "ai":
      return <AiSection config={config} setConfig={setConfig} />;
    case "agents":
      return <AgentsSection config={config} setConfig={setConfig} />;
    case "doctor":
      // Health groups what used to be Doctor, Stats for Nerds and Beta.
      return (
        <>
          <DoctorSection />
          <StatsForNerds />
          <BetaSection config={config} setConfig={setConfig} />
        </>
      );
    case "about":
      return <AboutSection config={config} setConfig={setConfig} />;
  }
}

function ConfigSkeleton() {
  return (
    <div className="space-y-3">
      {[0, 1, 2].map((i) => (
        <div key={i} className="surface h-16 animate-pulse opacity-60" />
      ))}
    </div>
  );
}

/** Wraps every section: header block first, then the cards. */
function SectionShell({ section, children }: { section: SectionId; children: React.ReactNode }) {
  const { t } = useTranslation("settings");
  return (
    // Keyed by section so each switch replays a short ease-in.
    <div key={section} className="section-enter">
      <SectionHeader
        title={t(`meta.${section}.title`)}
        description={t(`meta.${section}.description`)}
      />
      {children}
    </div>
  );
}

/* ---------------------------------------------------------------- */
/* General                                                           */
/* ---------------------------------------------------------------- */

function GeneralSection({
  config,
  setConfig,
}: {
  config: KensakuConfig;
  setConfig: React.Dispatch<React.SetStateAction<KensakuConfig | null>>;
}) {
  const { t } = useTranslation("settings");
  const { t: te } = useTranslation("errors");
  const { theme, setTheme } = useTheme();
  const { language, setLanguage, supportedLanguages } = useLanguage();
  const [capturing, setCapturing] = useState(false);

  /* null = unknown (backend rejected get_autostart) → switch stays off. */
  const [autostart, setAutostart] = useState<boolean | null>(null);
  const [autostartBusy, setAutostartBusy] = useState(false);
  const [autostartError, setAutostartError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    invoke<boolean>("get_autostart")
      .then((v) => {
        if (!cancelled) setAutostart(v);
      })
      .catch(() => {
        if (!cancelled) setAutostart(null);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  /** Optimistic toggle: flip the switch immediately, revert on failure. */
  const toggleAutostart = async (next: boolean) => {
    if (autostart === null || autostartBusy) return;
    const prev = autostart;
    setAutostart(next);
    setAutostartError(null);
    setAutostartBusy(true);
    try {
      await invoke("set_autostart", { enabled: next });
    } catch (err) {
      setAutostart(prev);
      setAutostartError(reasonFrom(err, te("unknown")));
    } finally {
      setAutostartBusy(false);
    }
  };

  useEffect(() => {
    if (!capturing) return;
    document.body.dataset.ksCapture = "1";
    // Modifiers accumulate across keydowns, so tapping Ctrl then K works as
    // well as holding Ctrl+K.
    const pressed = new Set<string>();
    const normalizeModifier = (key: string) => (key === "Meta" ? "Super" : key);
    const onKey = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.repeat) return;
      if (e.key === "Escape") {
        setCapturing(false);
        return;
      }
      if (["Control", "Super", "Meta", "Alt", "Shift"].includes(e.key)) {
        pressed.add(normalizeModifier(e.key));
        return;
      }
      if (e.ctrlKey) pressed.add("Control");
      if (e.metaKey) pressed.add("Super");
      if (e.altKey) pressed.add("Alt");
      if (e.shiftKey) pressed.add("Shift");
      const order = ["Control", "Super", "Alt", "Shift"];
      const parts = order.filter((m) => pressed.has(m));
      const key = e.key === " " ? "Space" : e.key.length === 1 ? e.key.toUpperCase() : e.key;
      parts.push(key);
      setConfig((c) => (c ? { ...c, shortcut: parts.join("+") } : c));
      setCapturing(false);
    };
    window.addEventListener("keydown", onKey);
    return () => {
      delete document.body.dataset.ksCapture;
      window.removeEventListener("keydown", onKey);
    };
  }, [capturing, setConfig]);

  return (
    <>
      <Card title={t("appearance.title")}>
        <div className="grid grid-cols-2 gap-4 p-4">
          {(["light", "dark"] as const).map((value) => (
            <ThemeSwatch
              key={value}
              value={value}
              active={theme === value}
              onSelect={() => {
                setTheme(value);
                /* Keep the app's config copy in step, or a later Save would
                   write the stale theme back over the persisted pick. */
                setConfig((c) => (c ? { ...c, theme: value } : c));
              }}
            />
          ))}
        </div>
        <div className="flex items-center justify-between border-t border-line px-4 py-3">
          <div className="text-[13px] text-fg">{t("appearance.language")}</div>
          <select
            value={language ?? ""}
            onChange={(e) => {
              const value = e.target.value === "" ? null : (e.target.value as SupportedLanguage);
              setLanguage(value);
              /* Match the ThemeSwatch handling: keep the settings snapshot in
                 step so a later Save does not write the old language back. */
              setConfig((c) => (c ? { ...c, language: value } : c));
            }}
            className="h-8 rounded-control border border-line bg-surface px-2 text-[13px] text-fg"
          >
            <option value="">{t("appearance.auto")}</option>
            {Object.entries(supportedLanguages).map(([code, name]) => (
              <option key={code} value={code}>{name}</option>
            ))}
          </select>
        </div>
      </Card>

      <Card title={t("window.title")}>
        <div className="space-y-3 px-4 py-3.5">
          <div className="space-y-0.5">
            <p className="text-[13px] font-medium text-fg">{t("window.mode")}</p>
            <p className="text-[12px] leading-[18px] text-muted">{t("window.description")}</p>
          </div>
          <WindowModePicker
            compact
            value={config.app_mode === "app" ? "app" : "launcher"}
            onChange={(mode) => setConfig((c) => (c ? { ...c, app_mode: mode } : c))}
            shortcut={config.shortcut}
            labels={{
              app: t("window.app"),
              launcher: t("window.launcher"),
              appDescription: t("window.appCard"),
              launcherDescription: t("window.launcherCard"),
              ariaLabel: t("window.aria"),
            }}
          />
        </div>
        <Field
          label={t("window.keepRunning")}
          description={
            config.app_mode === "app"
              ? t("window.keepRunningApp")
              : t("window.keepRunningLauncher")
          }
        >
          <Switch
            checked={config.close_to_background !== false}
            onChange={(checked) =>
              setConfig((c) => (c ? { ...c, close_to_background: checked } : c))
            }
          />
        </Field>
      </Card>

      <Card title={t("invocation.title")}>
        <Field
          label={t("invocation.shortcut")}
          description={
            config.app_mode === "app"
              ? t("invocation.shortcutApp")
              : t("invocation.shortcutLauncher")
          }
        >
          <button
            type="button"
            onClick={() => setCapturing(true)}
            className={`flex h-8 min-w-40 items-center justify-center rounded-control border px-3 font-mono text-[12px] transition-colors duration-150 ${
              capturing
                ? "border-accent text-accent"
                : "border-line bg-accent-soft text-fg hover:border-line-strong"
            }`}
          >
            {capturing ? t("invocation.pressKeys") : config.shortcut}
          </button>
        </Field>

        <Field label={t("invocation.maxResults")} description={t("invocation.maxResultsDescription")}>
          <Stepper
            value={config.max_results}
            min={25}
            max={500}
            step={25}
            onChange={(v) => setConfig((c) => (c ? { ...c, max_results: v } : c))}
          />
        </Field>

        <Field
          label={t("invocation.startAtLogin")}
          description={
            autostart === null
              ? t("invocation.startAtLoginUnavailable")
              : t("invocation.startAtLoginDescription")
          }
        >
          <Switch
            checked={autostart ?? false}
            disabled={autostart === null || autostartBusy}
            onChange={toggleAutostart}
          />
        </Field>
        {autostartError && (
          <p className="break-words border-t border-line px-4 py-2 font-mono text-[11px] text-danger">
            {t("invocation.loginError", { error: autostartError })}
          </p>
        )}
      </Card>
    </>
  );
}

/* ---------------------------------------------------------------- */
/* Folders                                                           */
/* ---------------------------------------------------------------- */

function FoldersSection({
  config,
  setConfig,
}: {
  config: KensakuConfig;
  setConfig: React.Dispatch<React.SetStateAction<KensakuConfig | null>>;
}) {
  const { t } = useTranslation("settings");
  const [input, setInput] = useState("");
  const { stats } = useStats(null);

  const addDir = () => {
    const t = input.trim();
    if (t && !config.watched_dirs.includes(t)) {
      setConfig((c) => (c ? { ...c, watched_dirs: [...c.watched_dirs, t] } : c));
      setInput("");
    }
  };

  const addDropped = (paths: string[]) =>
    setConfig((c) => {
      if (!c) return c;
      const fresh = paths.filter((p) => !c.watched_dirs.includes(p));
      return fresh.length ? { ...c, watched_dirs: [...c.watched_dirs, ...fresh] } : c;
    });

  return (
    <>
      <Card title={t("folders.title")}>
        {config.watched_dirs.length === 0 && (
          <p className="px-4 py-3 text-[11px] text-subtle">{t("folders.empty")}</p>
        )}
        {config.watched_dirs.map((dir) => (
          <div
            key={dir}
            className="flex items-center justify-between gap-3 border-b border-line px-4 py-2.5 last:border-b-0"
          >
            <div className="min-w-0">
              <p className="truncate text-[12px] text-fg">
                {dir.split("/").filter(Boolean).pop() || dir}
              </p>
              <p className="truncate font-mono text-[11px] text-subtle">{dir}</p>
            </div>
            <button
              type="button"
              title={t("folders.stopWatching", { dir })}
              onClick={() =>
                setConfig((c) =>
                  c ? { ...c, watched_dirs: c.watched_dirs.filter((d) => d !== dir) } : c,
                )
              }
              className="flex h-6 w-6 shrink-0 items-center justify-center rounded text-subtle transition-colors hover:bg-accent-soft hover:text-danger"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </div>
        ))}

        {/* Drop zone */}
        <div
          onDragOver={(e) => {
            e.preventDefault();
            e.currentTarget.classList.add("border-accent", "text-fg");
          }}
          onDragLeave={(e) => {
            e.currentTarget.classList.remove("border-accent", "text-fg");
          }}
          onDrop={async (e) => {
            e.preventDefault();
            e.currentTarget.classList.remove("border-accent", "text-fg");
            const paths: string[] = [];
            for (const item of Array.from(e.dataTransfer.files)) {
              const p = (item as File & { path?: string }).path;
              if (p) paths.push(p);
            }
            if (paths.length === 0 && input.trim()) paths.push(input.trim());
            addDropped(paths);
          }}
          className="mx-4 my-2.5 flex h-16 items-center justify-center rounded-control border border-dashed border-line text-[11px] text-subtle transition-colors duration-150"
        >
          {t("folders.drop")}
        </div>

        <div className="flex gap-1.5 border-t border-line p-3">
          <input
            type="text"
            value={input}
            placeholder={t("folders.placeholder")}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && addDir()}
            spellCheck={false}
            className="field h-8 min-w-0 flex-1 font-mono"
          />
          <button
            type="button"
            onClick={addDir}
            className="btn btn-secondary h-8 px-3 text-[12px]"
          >
            <Plus className="h-3.5 w-3.5" />
            {t("folders.add")}
          </button>
        </div>
        {stats && stats.index.files > 0 && (
          <p className="border-t border-line px-4 py-2 font-mono text-[11px] text-subtle">
            {t("folders.summary", {
              files: formatComma(stats.index.files),
              folders: formatComma(stats.index.dirs),
            })}
          </p>
        )}
      </Card>
    </>
  );
}

/* ---------------------------------------------------------------- */
/* Exclusions                                                        */
/* ---------------------------------------------------------------- */

function ExclusionsSection({
  config,
  setConfig,
}: {
  config: KensakuConfig;
  setConfig: React.Dispatch<React.SetStateAction<KensakuConfig | null>>;
}) {
  const { t } = useTranslation("settings");
  const [dirInput, setDirInput] = useState("");
  const [extInput, setExtInput] = useState("");

  const removeDir = (d: string) =>
    setConfig((c) => (c ? { ...c, excluded_dirs: c.excluded_dirs.filter((x) => x !== d) } : c));

  return (
    <>
      <Card title={t("exclusions.presets")}>
        <div className="flex flex-wrap gap-1.5 px-4 py-3">
          {Object.keys(EXCLUSION_PRESETS).map((p) => {
            const key = p === "Developer" ? "developer" : p === "Creative" ? "creative" : "allRound";
            return (
              <button
                key={p}
                type="button"
                onClick={() => setConfig((c) => (c ? { ...c, excluded_dirs: [...EXCLUSION_PRESETS[p]] } : c))}
                className="flex h-7 items-center rounded-full border border-line px-3 text-[12px] text-muted transition-colors duration-150 hover:border-line-strong hover:text-fg"
              >
                {t(`exclusions.${key}`)}
              </button>
            );
          })}
          <span className="self-center text-[11px] text-subtle">{t("exclusions.replaces")}</span>
        </div>
      </Card>

      <Card title={t("exclusions.directories")}>
        <div className="flex flex-wrap gap-1.5 px-4 py-3">
          {config.excluded_dirs.map((d) => (
            <button
              key={d}
              type="button"
              title={t("exclusions.remove", { name: d })}
              onClick={() => removeDir(d)}
              className="group flex h-6 items-center gap-1.5 rounded-full bg-accent-soft px-2.5 font-mono text-[11px] text-muted transition-colors hover:text-danger"
            >
              {d}
              <X className="h-3 w-3 opacity-50 group-hover:opacity-100" />
            </button>
          ))}
        </div>
        <div className="border-t border-line p-3">
          <input
            value={dirInput}
            placeholder={t("exclusions.directoryPlaceholder")}
            onChange={(e) => setDirInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && dirInput.trim()) {
                const t = dirInput.trim();
                setConfig((c) =>
                  c && !c.excluded_dirs.includes(t) ? { ...c, excluded_dirs: [...c.excluded_dirs, t] } : c,
                );
                setDirInput("");
              }
            }}
            spellCheck={false}
            className="field h-8 w-full font-mono"
          />
        </div>
      </Card>

      <Card title={t("exclusions.extensions")}>
        <div className="flex flex-wrap gap-1.5 px-4 py-3">
          {config.excluded_extensions.map((x) => (
            <button
              key={x}
              type="button"
              title={t("exclusions.remove", { name: x })}
              onClick={() =>
                setConfig((c) =>
                  c ? { ...c, excluded_extensions: c.excluded_extensions.filter((e) => e !== x) } : c,
                )
              }
              className="group flex h-6 items-center gap-1.5 rounded-full bg-accent-soft px-2.5 font-mono text-[11px] text-muted transition-colors hover:text-danger"
            >
              {x}
              <X className="h-3 w-3 opacity-50 group-hover:opacity-100" />
            </button>
          ))}
        </div>
        <div className="border-t border-line p-3">
          <input
            value={extInput}
            placeholder={t("exclusions.extensionPlaceholder")}
            onChange={(e) => setExtInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && extInput.trim()) {
                const t = extInput.trim();
                setConfig((c) =>
                  c && !c.excluded_extensions.includes(t)
                    ? { ...c, excluded_extensions: [...c.excluded_extensions, t] }
                    : c,
                );
                setExtInput("");
              }
            }}
            spellCheck={false}
            className="field h-8 w-full font-mono"
          />
        </div>
      </Card>

      <Card title={t("exclusions.hidden")}>
        <Field
          label={t("exclusions.skipHidden")}
          description={t("exclusions.skipHiddenDescription")}
        >
          <Switch
            checked={config.skip_hidden ?? true}
            onChange={(v) => setConfig((c) => (c ? { ...c, skip_hidden: v } : c))}
          />
        </Field>
      </Card>
    </>
  );
}

/* ---------------------------------------------------------------- */
/* Search                                                            */
/* ---------------------------------------------------------------- */

const CHEATSHEET = [
  ["type:image,dir,doc,code,media", "type"],
  ["ext:png", "ext"],
  ['in:"~/Projects"', "in"],
  ["modified:<7d", "modified"],
  ["size:>2mb", "size"],
  ["-ext:log", "negate"],
  [">reindex", "reindex"],
  [">doctor", "doctor"],
  [">model", "model"],
  [">add folder", "addFolder"],
] as const;

function SearchSection({
  config,
  setConfig,
}: {
  config: KensakuConfig;
  setConfig: React.Dispatch<React.SetStateAction<KensakuConfig | null>>;
}) {
  const { t } = useTranslation("settings");
  return (
    <>
      <Card title={t("search.results")}>
        <Field
          label={t("search.showThumbnails")}
          description={t("search.showThumbnailsDescription")}
        >
          <Switch
            checked={config.show_thumbnails ?? true}
            onChange={(v) => setConfig((c) => (c ? { ...c, show_thumbnails: v } : c))}
          />
        </Field>
        <Field
          label={t("search.layout")}
          description={t("search.layoutDescription")}
        >
          <div role="radiogroup" aria-label={t("search.layoutAria")} className="pill-group">
            {(["grid", "list"] as const).map((v) => {
              const active = (config.results_view ?? "grid") === v;
              return (
                <button
                  key={v}
                  type="button"
                  role="radio"
                  aria-checked={active}
                  onClick={() => setConfig((c) => (c ? { ...c, results_view: v } : c))}
                  className="pill capitalize"
                >
                  {v === "grid" ? t("search.grid") : t("search.list")}
                </button>
              );
            })}
          </div>
        </Field>
      </Card>

      <Card title={t("search.ranking")}>
        <Field
          label={t("search.semantic")}
          description={t("search.semanticDescription")}
        >
          <Switch
            checked={config.semantic_enabled ?? true}
            onChange={(v) => setConfig((c) => (c ? { ...c, semantic_enabled: v } : c))}
          />
        </Field>
        <OcrField config={config} setConfig={setConfig} />
      </Card>

      <Card title={t("search.background")}>
        <Field
          label={t("search.crawlMode")}
          description={t("search.crawlModeDescription")}
        >
          <select
            value={config.crawl_mode ?? "full"}
            onChange={(e) =>
              setConfig((c) =>
                c
                  ? {
                      ...c,
                      crawl_mode: e.target.value as
                        | "full"
                        | "balanced"
                        | "eco",
                    }
                  : c,
              )
            }
            className="field h-8"
          >
            <option value="full">{t("search.fullSpeed")}</option>
            <option value="balanced">{t("search.balanced")}</option>
            <option value="eco">{t("search.eco")}</option>
          </select>
        </Field>
        <Field
          label={t("search.pause")}
          description={t("search.pauseDescription")}
        >
          <Switch
            checked={config.pause_indexing ?? false}
            onChange={(v) =>
              setConfig((c) => (c ? { ...c, pause_indexing: v } : c))
            }
          />
        </Field>
      </Card>

      <Card title={t("search.aliases")}>
        <div className="space-y-2 p-4">
          <p className="text-[12px] leading-[18px] text-muted">
            {t("search.aliasesDescription")}
          </p>
          {(config.aliases ?? []).map((alias, i) => (
            <div key={i} className="flex items-center gap-2">
              <input
                type="text"
                value={alias.term}
                placeholder={t("search.term")}
                onChange={(e) =>
                  setConfig((c) => {
                    if (!c) return c;
                    const aliases = [...(c.aliases ?? [])];
                    aliases[i] = { ...aliases[i], term: e.target.value };
                    return { ...c, aliases };
                  })
                }
                className="field h-8 w-28 shrink-0 font-mono"
              />
              <span className="text-subtle">→</span>
              <input
                type="text"
                value={alias.expansions.join(", ")}
                placeholder={t("search.expansions")}
                onChange={(e) =>
                  setConfig((c) => {
                    if (!c) return c;
                    const aliases = [...(c.aliases ?? [])];
                    aliases[i] = {
                      ...aliases[i],
                      expansions: e.target.value.split(",").map((s) => s.trim()).filter(Boolean),
                    };
                    return { ...c, aliases };
                  })
                }
                className="field h-8 min-w-0 flex-1 font-mono"
              />
              <button
                type="button"
                title={t("search.removeAlias")}
                onClick={() =>
                  setConfig((c) =>
                    c ? { ...c, aliases: (c.aliases ?? []).filter((_, j) => j !== i) } : c,
                  )
                }
                className="flex h-7 w-7 shrink-0 items-center justify-center rounded text-subtle transition-colors hover:bg-accent-soft hover:text-danger"
              >
                <X className="h-3.5 w-3.5" />
              </button>
            </div>
          ))}
          <button
            type="button"
            onClick={() =>
              setConfig((c) =>
                c ? { ...c, aliases: [...(c.aliases ?? []), { term: "", expansions: [] }] } : c,
              )
            }
            className="flex h-7 items-center gap-1 rounded-control border border-line px-2.5 text-[12px] text-muted transition-colors hover:border-line-strong hover:text-fg"
          >
            <Plus className="h-3 w-3" /> {t("search.addAlias")}
          </button>
        </div>
      </Card>

      <Card title={t("search.queryLanguage")}>
        <div className="grid grid-cols-2 gap-x-8 px-4 py-2.5">
          {CHEATSHEET.map(([token, key], idx) => (
            <div
              key={token}
              className={`flex h-8 items-center justify-between gap-3 ${
                idx < CHEATSHEET.length - 2 ? "border-b border-line/70" : ""
              }`}
            >
              <code className="shrink-0 whitespace-nowrap text-fg">{token}</code>
              <span className="truncate text-right text-[11px] text-subtle">{t(`search.cheat.${key}`)}</span>
            </div>
          ))}
        </div>
      </Card>
    </>
  );
}

/* ---------------------------------------------------------------- */
/* AI & Semantic                                                     */
/* ---------------------------------------------------------------- */

function AiSection({ config, setConfig }: { config: KensakuConfig; setConfig: React.Dispatch<React.SetStateAction<KensakuConfig | null>> }) {
  const { t } = useTranslation("settings");
  const { stats, live } = useStats(2000);
  const [reembedState, setReembedState] = useState<"idle" | "pending">("idle");

  const modelId =
    stats?.runtime?.model_id || config.semantic_model || t("ai.modelUnknown");
  const queueDepth = stats?.ingestion?.embed_queue_depth ?? 0;

  const reembed = async () => {
    setReembedState("pending");
    try {
      await invoke("reembed_all");
    } catch {
      console.warn("reembed_all not available yet");
    }
    window.setTimeout(() => setReembedState("idle"), 1500);
  };

  return (
    <>
      <Card title={t("ai.modelCard")}>
        <Field label={t("ai.model")} description={t("ai.modelDescription")}>
          <span className="max-w-48 truncate font-mono text-[11px] text-fg">{modelId}</span>
        </Field>
        <Field label={t("ai.dimensions")} description={t("ai.dimensionsDescription")}>
          <span className="font-mono text-[11px] text-muted">—</span>
        </Field>
        <Field label={t("ai.vector")} description={t("ai.vectorDescription")}>
          <span className="font-mono text-[11px] text-muted">{formatBytes(stats?.index?.vector_bytes ?? 0)}</span>
        </Field>
        <Field label={t("ai.readiness")} description={t("ai.readinessDescription")}>
          <span
            className={`font-mono text-[11px] ${
              stats?.runtime?.semantic_ready ? "text-success" : "text-warning"
            }`}
          >
            {stats?.runtime?.semantic_ready ? t("ai.ready") : live ? t("ai.warming") : t("ai.pendingBackend")}
          </span>
        </Field>
      </Card>

      <Card title={t("ai.table")}>
        <Field
          label={t("ai.f32")}
          description={t("ai.f32Description")}
        >
          <Switch
            checked={config.semantic_f32_table ?? false}
            onChange={(v) => setConfig((c) => (c ? { ...c, semantic_f32_table: v } : c))}
          />
        </Field>
      </Card>

      <Card title={t("ai.queue")}>
        <div className="space-y-2 p-4">
          <div className="flex items-center justify-between text-[11px]">
            <span className="text-muted">{t("ai.queuedUnits")}</span>
            <span className="font-mono text-fg">{formatComma(queueDepth)}</span>
          </div>
          <div className="h-1.5 w-full overflow-hidden rounded-full bg-accent-soft">
            <div
              className="h-full rounded-full bg-accent transition-[width] duration-500"
              style={{ width: `${queueDepth > 0 ? Math.max(4, Math.min(100, queueDepth)) : 0}%` }}
            />
          </div>
          <div className="flex items-center justify-between pt-1">
            <span className="font-mono text-[11px] text-subtle">
              {live ? t("ai.polled") : t("ai.statsPending")}
            </span>
            <button
              type="button"
              onClick={reembed}
              className="flex h-7 items-center gap-1.5 rounded-control border border-line px-2.5 text-[11px] text-fg transition-colors hover:border-line-strong"
            >
              <RefreshCw className="h-3 w-3" />
              {reembedState === "pending" ? t("ai.queued") : t("ai.reembed")}
            </button>
          </div>
        </div>
      </Card>

      <ModelFilesCard />
    </>
  );
}

/* ---------------------------------------------------------------- */
/* Agents                                                            */
/* ---------------------------------------------------------------- */

const MCP_CONFIG = `{
  "mcpServers": {
    "kensaku": { "command": "kensaku-cli", "args": ["mcp"] }
  }
}`;

const CLI_EXAMPLES = [
  'kensaku-cli search "docker error" --json --limit 10',
  'kensaku-cli search "type:pdf invoice" --path ~/Documents',
  'kensaku-cli state --json',
];

const MCP_TOOLS: [string, string][] = [
  ["health_check", "daemon · db · index · model"],
  ["search", "query, page_size ≤ 50, cursor, mode, filters"],
  ["read_snippet", "path, query?, max_tokens ≤ 1024"],
  ["index_state", "counts, phase, queue, freshness"],
  ["regex_search", "bounded pattern, page_size, cursor"],
  ["find_symbol", "symbol name, language?, page_size ≤ 50"],
  ["reindex", "opt-in only; requires daemon"],
];

/** One copyable mono row — command text with a trailing copy button. */
function CopyRow({
  text,
  copied,
  onCopied,
}: {
  text: string;
  copied: boolean;
  onCopied: () => void;
}) {
  const { t } = useTranslation("common");
  return (
    <div className="flex items-center gap-2 border-b border-line/60 px-3 py-2 last:border-b-0">
      <code className="no-scrollbar min-w-0 flex-1 select-text overflow-x-auto whitespace-nowrap font-mono text-[11px] leading-5 text-fg">
        {text}
      </code>
      <button
        type="button"
        title={t("actions.copy")}
        onClick={() => {
          navigator.clipboard.writeText(text);
          onCopied();
        }}
        className="flex h-6 w-6 shrink-0 items-center justify-center rounded text-subtle transition-colors hover:bg-elevated hover:text-fg"
      >
        {copied ? <Check className="h-3 w-3 text-accent" /> : <Copy className="h-3 w-3" />}
      </button>
    </div>
  );
}

/* ---------------------------------------------------------------- */
/* Beta                                                              */
/* ---------------------------------------------------------------- */

function BetaSection({
  config,
  setConfig,
}: {
  config: KensakuConfig;
  setConfig: React.Dispatch<React.SetStateAction<KensakuConfig | null>>;
}) {
  const { t } = useTranslation("settings");
  void config;
  void setConfig;
  return (
    <Card title={t("beta.visual")}>
      <div className="flex items-center justify-between gap-4 px-4 py-3 opacity-50">
        <div className="min-w-0">
          <p className="text-[12px] font-medium text-fg">{t("beta.visual")}</p>
          <p className="text-[12px] leading-[18px] text-muted">{t("beta.visualDescription")}</p>
        </div>
        <span className="shrink-0 rounded-small border border-line px-2 py-0.5 text-[12px] text-muted">{t("beta.comingLater")}</span>
      </div>
    </Card>
  );
}

function AgentConnectCard() {
  const { t } = useTranslation("settings");
  const { t: te } = useTranslation("errors");
  const [statuses, setStatuses] = useState<ClientStatus[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const refresh = async () => {
    try {
      setStatuses(await invoke<ClientStatus[]>("agent_connect_status"));
    } catch {
      setStatuses([]);
    }
  };

  useEffect(() => {
    void refresh();
  }, []);

  const install = async (id: string) => {
    setBusy(id);
    setNotice(null);
    try {
      const outcome = await invoke<InstallOutcome>("agent_connect_install", { client: id });
      setNotice(outcome.message);
    } catch (err) {
      setNotice(reasonFrom(err, te("unknown")));
    } finally {
      setBusy(null);
      void refresh();
    }
  };

  const statusLabel = (status: ClientStatus): string => {
    if (status.configured) return t("agents.connected");
    if (status.installed) return t("agents.found");
    if (status.config_path) return t("agents.notSetUp");
    return t("agents.notDetected");
  };

  return (
    <Card title={t("agents.connect")}>
      <div>
        {statuses.length === 0 && (
          <p className="px-4 py-3 text-[12px] text-muted">
            {t("agents.detecting")}
          </p>
        )}
        {statuses.map((status) => (
          <div
            key={status.id}
            className="flex items-center justify-between gap-4 border-b border-line/60 px-4 py-2.5 last:border-b-0"
          >
            <div className="min-w-0">
              <p className="text-[12px] font-medium text-fg">{status.name}</p>
              <p className="truncate font-mono text-[11px] text-subtle">
                {status.config_path ?? t("agents.configNotFound")}
              </p>
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <span
                className={`rounded-full px-2 py-0.5 font-mono text-[11px] ${
                  status.configured
                    ? "bg-accent-soft text-accent"
                    : "border border-line text-subtle"
                }`}
              >
                {statusLabel(status)}
              </span>
              {!status.configured && (
                <button
                  type="button"
                  disabled={busy === status.id || !status.config_path}
                  onClick={() => void install(status.id)}
                  className="flex h-6 items-center rounded-control border border-line px-2 text-[11px] text-fg transition-colors hover:border-line-strong disabled:opacity-50"
                >
                  {busy === status.id ? t("agents.adding") : t("agents.addKensaku")}
                </button>
              )}
            </div>
          </div>
        ))}
      </div>
      {notice && (
        <p className="border-t border-line px-4 py-2 text-[12px] text-muted">{notice}</p>
      )}
      <p className="border-t border-line px-4 py-2.5 text-[12px] leading-[18px] text-muted">
        {t("agents.connectNote")}
      </p>
    </Card>
  );
}

function AgentsSection({
  config,
  setConfig,
}: {
  config: KensakuConfig;
  setConfig: React.Dispatch<React.SetStateAction<KensakuConfig | null>>;
}) {
  const { t } = useTranslation("settings");
  const [configCopied, setConfigCopied] = useState(false);
  const [copiedRow, setCopiedRow] = useState<string | null>(null);

  const flashRow = (text: string) => {
    setCopiedRow(text);
    window.setTimeout(() => setCopiedRow((r) => (r === text ? null : r)), 1500);
  };

  return (
    <>
      <p className="pb-3 text-[12px] leading-[18px] text-muted">
        {t("agents.intro")}
      </p>

      <AgentConnectCard />

      {/* MCP */}
      <Card title={t("agents.mcp")}>
        <div className="px-4 py-3">
          <p className="text-[12px] leading-[18px] text-muted">{t("agents.mcpDescription")}</p>
        </div>
        <div className="relative border-t border-line bg-accent-soft p-3 pr-10">
          <pre className="select-text whitespace-pre-wrap break-all font-mono text-[11px] leading-[18px] text-muted">
            {MCP_CONFIG}
          </pre>
          <button
            type="button"
            title={t("agents.copyConfig")}
            onClick={() => {
              navigator.clipboard.writeText(MCP_CONFIG);
              setConfigCopied(true);
              window.setTimeout(() => setConfigCopied(false), 1500);
            }}
            className="absolute right-2 top-2 flex h-6 w-6 items-center justify-center rounded text-subtle transition-colors hover:bg-elevated hover:text-fg"
          >
            {configCopied ? <Check className="h-3.5 w-3.5 text-accent" /> : <Copy className="h-3.5 w-3.5" />}
          </button>
        </div>
        <p className="border-t border-line px-4 py-2.5 text-[12px] leading-[18px] text-muted">
          {t("agents.pathNote")}
        </p>
      </Card>

      {/* CLI */}
      <Card title={t("agents.cli")}>
        <div>
          {CLI_EXAMPLES.map((cmd) => (
            <CopyRow
              key={cmd}
              text={cmd}
              copied={copiedRow === cmd}
              onCopied={() => flashRow(cmd)}
            />
          ))}
        </div>
        <p className="border-t border-line px-4 py-2.5 text-[12px] leading-[18px] text-muted">
          {t("agents.cliNote")}
        </p>
      </Card>

      {/* Tools */}
      <Card title={t("agents.tools")}>
        <div className="px-4 py-1.5">
          {MCP_TOOLS.map(([name, args]) => (
            <div
              key={name}
              className="flex items-baseline justify-between gap-4 border-b border-line/60 py-2.5 last:border-b-0"
            >
              <code className="shrink-0 whitespace-nowrap font-mono text-[11px] font-medium text-fg">
                {name}
              </code>
              <span className="min-w-0 truncate text-right font-mono text-[11px] text-subtle">
                {args}
              </span>
            </div>
          ))}
        </div>
      </Card>

      {/* Write access */}
      <Card title={t("agents.writeAccess")}>
        <Field
          label={t("agents.allowReindex")}
          description={t("agents.allowReindexDescription")}
        >
          <Switch
            checked={config.mcp_allow_reindex ?? false}
            onChange={(v) =>
              setConfig((c) => (c ? { ...c, mcp_allow_reindex: v } : c))
            }
          />
        </Field>
      </Card>

      {/* Security */}
      <Card title={t("agents.security")}>
        <p className="p-4 text-[12px] leading-[18px] text-muted">
          {t("agents.securityDescription")}
        </p>
      </Card>
    </>
  );
}

/* ---------------------------------------------------------------- */
/* About + Danger zone                                               */
/* ---------------------------------------------------------------- */

const CONFIRM_WORD = "RESET";

/** Best-effort: the backend may not ship get_reset_pending yet. */
function useResetPending(): boolean {
  const [pending, setPending] = useState(false);
  useEffect(() => {
    invoke<boolean>("get_reset_pending")
      .then(setPending)
      .catch(() => setPending(false));
  }, []);
  return pending;
}

/** Tracks keys the app persists in localStorage, so a reset starts clean. */
function clearClientState() {
  try {
    for (const key of ["kensaku.theme", "kensaku.view", "kensaku.preview"]) {
      localStorage.removeItem(key);
    }
  } catch {
    /* storage unavailable — the backend reset owns the rest */
  }
}

/**
 * Type-RESET confirmation. Traps focus, Esc cancels, Enter submits only
 * when the word matches exactly. On reject the error surfaces inline and
 * nothing is cleared; on resolve the app is expected to restart.
 */
function ResetDialog({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation("settings");
  const [text, setText] = useState("");
  const [running, setRunning] = useState(false);
  const [removePacks, setRemovePacks] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const dialogRef = React.useRef<HTMLDivElement>(null);
  const matches = text === CONFIRM_WORD;

  /* Window-level capture so Esc/Tab are handled before the settings
     overlay's own Escape-to-close listener sees them. */
  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    const focusables = () =>
      Array.from(dialog.querySelectorAll<HTMLElement>("input, button:not([disabled])"));
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        onClose();
        return;
      }
      if (e.key === "Tab") {
        const list = focusables();
        if (list.length === 0) return;
        const first = list[0];
        const last = list[list.length - 1];
        const active = document.activeElement;
        if (e.shiftKey && active === first) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && (active === last || !dialog.contains(active))) {
          e.preventDefault();
          first.focus();
        }
      }
    };
    window.addEventListener("keydown", onKey, true);
    const input = dialog.querySelector<HTMLInputElement>("input");
    input?.focus();
    input?.select();
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const submit = async () => {
    if (!matches || running) return;
    setRunning(true);
    setError(null);
    try {
      await invoke("reset_all", { confirm: CONFIRM_WORD, removePacks });
      // Resolves → the app restarts. Clear client-only view state so a
      // relaunched window comes up fresh; show nothing.
      clearClientState();
      onClose();
    } catch (err) {
      // Rejects → keep everything, surface the reason inline.
      console.error("reset_all failed:", err);
      setError(String(err));
      setRunning(false);
    }
  };

  return (
    <div
      className="fade-in absolute inset-0 z-50 flex items-center justify-center p-6 backdrop-blur-[2px]"
      style={{ background: "var(--scrim)" }}
      onMouseDown={(e) => e.target === e.currentTarget && onClose()}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-label={t("about.resetDialog.aria")}
        className="overlay-in elevated w-full max-w-[400px] overflow-hidden border-danger/30"
      >
        <div className="flex items-center justify-between border-b border-line bg-accent-soft px-4 py-2.5">
          <p className="text-[11px] font-medium text-danger">{t("about.danger")}</p>
          <button
            type="button"
            title={t("about.resetDialog.cancel")}
            onClick={onClose}
            className="flex h-6 w-6 items-center justify-center rounded text-muted transition-colors hover:bg-elevated hover:text-fg"
          >
            <X className="h-3.5 w-3.5" />
          </button>
        </div>

        <form
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <div className="p-4">
            <p className="text-[13px] font-semibold text-fg">{t("about.resetDialog.title")}</p>
            <p className="pt-0.5 text-[12px] leading-[18px] text-muted">
              {t("about.resetDialog.body")}
            </p>
          </div>

          <div className="px-4 pb-4">
            {/* Downloaded OCR and model packs survive a reset by default: they
                are ~55 MB of downloads, not the user's own data, and removing
                them means waiting for them again before search works. */}
            <label className="mb-3 flex cursor-pointer items-start gap-2 rounded-control border border-line p-2.5">
              <input
                type="checkbox"
                checked={removePacks}
                onChange={(e) => setRemovePacks(e.target.checked)}
                className="mt-0.5 h-3.5 w-3.5 shrink-0 accent-[var(--danger)]"
              />
              <span className="min-w-0">
                <span className="block text-[12px] font-medium text-fg">
                  {t("about.resetDialog.packsTitle")}
                </span>
                <span className="block text-[11px] leading-[16px] text-muted">
                  {t("about.resetDialog.packsBody")}
                </span>
              </span>
            </label>
            <label htmlFor="reset-confirm" className="eyebrow pb-1.5 block">
              {t("about.resetDialog.typeReset")} <span className="font-mono normal-case text-danger">RESET</span> {t("about.resetDialog.toConfirm")}
            </label>
            <input
              id="reset-confirm"
              type="text"
              value={text}
              onChange={(e) => setText(e.target.value)}
              autoComplete="off"
              spellCheck={false}
              className={`h-9 w-full rounded-control border bg-accent-soft px-2.5 font-mono text-[12px] text-fg outline-none transition-colors focus:border-accent ${
                text && !matches ? "border-danger/50" : "border-line"
              }`}
            />
            {error && (
              <p className="break-words pt-2 font-mono text-[11px] text-danger">{error}</p>
            )}
          </div>

          <div className="flex items-center justify-end gap-1.5 border-t border-line bg-accent-soft px-3 py-2.5">
            <button
              type="button"
              onClick={onClose}
              className="flex h-8 items-center rounded-control border border-line px-3 text-[12px] text-fg transition-colors hover:border-line-strong"
            >
              {t("about.resetDialog.cancelButton")}
            </button>
            <button
              type="submit"
              disabled={!matches || running}
              className="flex h-8 items-center rounded-control bg-danger px-3 text-[12px] font-medium text-bg transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-40"
            >
              {running ? t("about.resetDialog.resetting") : t("about.resetDialog.resetAndRestart")}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

function AboutSection({
  config,
  setConfig,
}: {
  config: KensakuConfig | null;
  setConfig: React.Dispatch<React.SetStateAction<KensakuConfig | null>>;
}) {
  const { t } = useTranslation("settings");
  const resetPending = useResetPending();
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [version, setVersion] = useState<string>("…");
  const [checking, setChecking] = useState(false);
  const [updateInfo, setUpdateInfo] = useState<UpdateInfo | null>(null);
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus | null>(null);
  const [downloading, setDownloading] = useState(false);
  const [updaterError, setUpdaterError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    getVersion()
      .then((v) => {
        if (!cancelled) setVersion(v);
      })
      .catch(() => {
        if (!cancelled) setVersion("unknown");
      });
    return () => {
      cancelled = true;
    };
  }, []);

  /* Poll updater progress while active */
  useEffect(() => {
    if (!downloading) return;
    const interval = window.setInterval(async () => {
      try {
        const st = await getUpdaterStatus();
        setUpdateStatus(st);
        if (!st.busy) {
          setDownloading(false);
          if (st.error) setUpdaterError(st.error);
        }
      } catch (err) {
        console.warn("Update status polling failed:", err);
      }
    }, 500);
    return () => window.clearInterval(interval);
  }, [downloading]);

  const checkUpdatesNow = async () => {
    setChecking(true);
    setUpdaterError(null);
    try {
      const info = await checkForUpdates();
      setUpdateInfo(info);
    } catch (err) {
      setUpdaterError(String(err));
    } finally {
      setChecking(false);
    }
  };

  const startDownload = async () => {
    if (!updateInfo?.asset_url || !updateInfo?.asset_name) return;
    setDownloading(true);
    setUpdaterError(null);
    try {
      const st = await downloadUpdate(updateInfo.asset_url, updateInfo.asset_name);
      setUpdateStatus(st);
      if (st.ready_to_install) {
        setDownloading(false);
      }
    } catch (err) {
      setDownloading(false);
      setUpdaterError(String(err));
    }
  };

  const runInstall = async () => {
    try {
      await installUpdateAndRestart();
    } catch (err) {
      setUpdaterError(String(err));
    }
  };

  return (
    <>
      {resetPending && (
        <div className="flex items-center gap-2 rounded-control border border-warning/40 bg-elevated px-4 py-2.5">
          <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-warning" />
          <p className="text-[12px] text-muted">{t("about.resetPending")}</p>
        </div>
      )}
      {resetPending && <div className="h-3" />}

      <Card title={t("about.version")}>
        <div className="flex items-center justify-between p-4">
          <div>
            <p className="text-[15px] font-semibold text-fg">
              Kensaku <span className="font-normal text-muted">検索</span>
            </p>
            <p className="font-mono text-[11px] text-subtle">{version}</p>
            <p className="mt-1 text-[11px] text-subtle">
              {t("about.autoUpdate")}
            </p>
          </div>
          <div className="flex items-center gap-1.5">
            {["Rust", "Tauri 2", "MIT"].map((b) => (
              <span
                key={b}
                className="rounded-full border border-line px-2.5 py-1 font-mono text-[11px] text-muted"
              >
                {b}
              </span>
            ))}
          </div>
        </div>

        <Field
          label={t("about.autoUpdateSetting")}
          description={t("about.autoUpdateSettingDesc")}
        >
          <Switch
            checked={config?.auto_update ?? true}
            onChange={(v) =>
              setConfig((c) => (c ? { ...c, auto_update: v } : c))
            }
          />
        </Field>

        <div className="flex items-center justify-between border-t border-line px-4 py-3">
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={checkUpdatesNow}
              disabled={checking || downloading}
              className="btn btn-secondary h-8 !rounded-control"
            >
              <RefreshCw
                className={`mr-1.5 h-3.5 w-3.5 ${checking ? "animate-spin text-accent" : "text-muted"}`}
              />
              {checking ? t("about.checking") : t("about.checkUpdates")}
            </button>
            <button
              type="button"
              onClick={() => openGitHubBugReport()}
              className="btn btn-secondary h-8 !rounded-control"
            >
              <Bug className="mr-1.5 h-3.5 w-3.5 text-muted" />
              {t("about.reportIssue")}
            </button>
          </div>
          <button
            type="button"
            onClick={() => openUrl("https://github.com/yjid/kensaku/releases/latest")}
            className="flex items-center gap-1 text-[11px] text-subtle hover:text-fg transition-colors"
          >
            <span>{t("about.opensRelease")}</span>
            <ExternalLink className="h-3 w-3" />
          </button>
        </div>
      </Card>

      {/* Live In-App Update Card */}
      {updaterError && (
        <div className="rounded-control border border-warning/40 bg-elevated p-4">
          <div className="flex items-start gap-2.5">
            <AlertCircle className="h-4 w-4 shrink-0 text-warning mt-0.5" />
            <div className="space-y-1">
              <p className="text-[12px] font-medium text-warning">
                {t("about.downloadFailed", { error: updaterError })}
              </p>
              <button
                type="button"
                onClick={() => openUrl("https://github.com/yjid/kensaku/releases/latest")}
                className="btn btn-secondary h-7 text-[11px] mt-2 !rounded-control"
              >
                {t("about.viewReleaseNotes")}
              </button>
            </div>
          </div>
        </div>
      )}

      {updateInfo && !updaterError && (
        <Card title={t("about.updateAvailable", { version: updateInfo.latest_version })}>
          <div className="p-4 space-y-3">
            {!updateInfo.has_update ? (
              <div className="flex items-center gap-2 text-[13px] text-fg">
                <CheckCircle2 className="h-4 w-4 text-emerald-500 shrink-0" />
                <span>{t("about.upToDate")} (v{updateInfo.current_version})</span>
              </div>
            ) : (
              <>
                <div className="flex items-start justify-between gap-3">
                  <div>
                    <div className="flex items-center gap-2">
                      <span className="rounded-full bg-accent/15 px-2 py-0.5 text-[11px] font-medium text-accent">
                        v{updateInfo.latest_version}
                      </span>
                      {updateInfo.published_at && (
                        <span className="text-[11px] text-subtle">
                          {t("about.publishedOn", { date: formatReleaseDate(updateInfo.published_at) })}
                        </span>
                      )}
                    </div>
                    <p className="text-[13px] font-medium text-fg mt-1">
                      {updateInfo.release_name}
                    </p>
                    {updateInfo.asset_name && (
                      <p className="font-mono text-[11px] text-muted mt-0.5">
                        {updateInfo.asset_name} {updateInfo.asset_size ? `(${formatBytes(updateInfo.asset_size)})` : ""}
                      </p>
                    )}
                  </div>
                </div>

                {/* Download Progress Bar */}
                {downloading && updateStatus?.progress && (
                  <div className="space-y-1.5 pt-1">
                    <div className="flex items-center justify-between text-[11px] text-muted">
                      <span>
                        {t("about.downloadPercent", {
                          percent: updateStatus.progress.percent,
                          downloaded: formatBytes(updateStatus.progress.downloaded),
                          total: formatBytes(updateStatus.progress.total),
                        })}
                      </span>
                      <span>{updateStatus.progress.percent}%</span>
                    </div>
                    <div className="h-2 w-full overflow-hidden rounded-full bg-surface-2">
                      <div
                        className="h-full bg-accent transition-all duration-300"
                        style={{ width: `${updateStatus.progress.percent}%` }}
                      />
                    </div>
                  </div>
                )}

                {/* Action Buttons */}
                <div className="flex items-center gap-2 pt-2 border-t border-line">
                  {updateStatus?.ready_to_install ? (
                    <button
                      type="button"
                      onClick={runInstall}
                      className="btn btn-primary h-8 !rounded-control flex items-center gap-1.5"
                    >
                      <Sparkles className="h-3.5 w-3.5" />
                      {t("about.installAndRestart")}
                    </button>
                  ) : downloading ? (
                    <button
                      type="button"
                      disabled
                      className="btn btn-secondary h-8 !rounded-control flex items-center gap-1.5 opacity-75"
                    >
                      <RefreshCw className="h-3.5 w-3.5 animate-spin" />
                      {t("about.downloading")}
                    </button>
                  ) : updateInfo.asset_url ? (
                    <button
                      type="button"
                      onClick={startDownload}
                      className="btn btn-primary h-8 !rounded-control flex items-center gap-1.5"
                    >
                      <Download className="h-3.5 w-3.5" />
                      {t("about.downloadAndInstall")}
                    </button>
                  ) : (
                    <span className="text-[12px] text-muted">
                      {t("about.noAssetFound")}
                    </span>
                  )}

                  <button
                    type="button"
                    onClick={() => openUrl(updateInfo.release_url)}
                    className="btn btn-secondary h-8 !rounded-control flex items-center gap-1.5"
                  >
                    <ExternalLink className="h-3.5 w-3.5 text-muted" />
                    {t("about.viewReleaseNotes")}
                  </button>
                </div>
              </>
            )}
          </div>
        </Card>
      )}

      <Card title={t("about.howTo")}>
        <div className="space-y-2 p-4 text-[12px] leading-[18px] text-muted">
          <p className="text-fg">{t("about.howToIntro")}</p>
          <ul className="list-disc space-y-1 pl-5">
            <li><b>{t("about.tipLabels.tab")}</b> {t("about.tipTab")}</li>
            <li><b>{t("about.tipLabels.aliases")}</b> {t("about.tipAliases")}</li>
            <li>
              <b>{t("about.tipLabels.filters")}</b>{" "}
              {t("about.tipFilters", {
                type: "type:pdf",
                ext: "ext:png",
                in: "in:~/Projects",
                modified: "modified:<7d",
              })}
            </li>
            <li>
              <b>/</b>{" "}
              {t("about.tipSlash", {
                dupes: "/dupes",
                doctor: "/doctor",
                settings: "/settings",
              })}
            </li>
            <li><b>@</b> {t("about.tipAt")}</li>
            <li><b>{t("about.tipLabels.sort")}</b> {t("about.tipSort")}</li>
            <li><b>{t("about.tipLabels.pin")}</b> {t("about.tipPin")}</li>
            <li><b>{t("about.tipLabels.ocr")}</b> {t("about.tipOcr")}</li>
          </ul>
          <p>{t("about.staysLocal")}</p>
        </div>
      </Card>

      <Card title={t("about.thesis")}>
        <div className="p-4 text-center">
          <p className="font-serif text-[15px] italic leading-[18px] text-muted">
            {t("about.thesisQuote")}
          </p>
        </div>
      </Card>

      <Card title={t("about.setup")}>
        <div className="flex items-center justify-between gap-4 p-4">
          <div className="min-w-0">
            <p className="text-[13px] font-medium text-fg">{t("about.rerun")}</p>
            <p className="line-clamp-2 pt-0.5 text-[12px] leading-[18px] text-muted">
              {t("about.rerunDescription")}
            </p>
          </div>
          <button
            type="button"
            onClick={async () => {
              try {
                await invoke("run_setup_again");
                window.location.reload();
              } catch (e) {
                console.error(e);
              }
            }}
            className="h-8 shrink-0 rounded-control border border-line px-3 text-[11px] font-medium text-fg transition-colors hover:border-line-strong"
          >
            {t("about.rerunButton")}
          </button>
        </div>
      </Card>

      {/* Danger zone — visually distinct but calm: danger hairline, no fills. */}
      <div>
        <p className="pb-2 text-[11px] font-medium text-danger">
          {t("about.danger")}
        </p>
        <div className="overflow-hidden rounded-panel border border-danger/30 bg-elevated">
          <div className="flex items-center justify-between gap-4 p-4">
            <div className="min-w-0">
              <p className="text-[13px] font-medium text-fg">{t("about.reset")}</p>
              <p className="line-clamp-2 pt-0.5 text-[12px] leading-[18px] text-muted">
                {t("about.resetDescription")}
              </p>
            </div>
            <button
              type="button"
              onClick={() => setConfirmOpen(true)}
              className="h-8 shrink-0 rounded-control border border-danger/40 px-3 text-[11px] font-medium text-danger transition-colors hover:bg-danger/10"
            >
              {t("about.resetButton")}
            </button>
          </div>
        </div>
      </div>

      {confirmOpen && <ResetDialog onClose={() => setConfirmOpen(false)} />}
    </>
  );
}

/* Escape with unsaved edits: keep them or lose them, explicitly. */
function DiscardDialog({
  onKeep,
  onDiscard,
}: {
  onKeep: () => void;
  onDiscard: () => void;
}) {
  const { t } = useTranslation("settings");
  const dialogRef = React.useRef<HTMLDivElement>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        onKeep();
      }
    };
    window.addEventListener("keydown", onKey, true);
    dialogRef.current?.querySelector<HTMLButtonElement>("button")?.focus();
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onKeep]);

  return (
    <div
      className="fade-in absolute inset-0 z-[60] flex items-center justify-center p-6"
      style={{ background: "var(--scrim)" }}
      onMouseDown={(e) => e.target === e.currentTarget && onKeep()}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="discard-title"
        className="elevated w-[380px] rounded-panel p-5"
      >
        <h2 id="discard-title" className="text-[14px] font-semibold text-fg">
          {t("discard.title")}
        </h2>
        <p className="mt-1.5 text-[12px] leading-[18px] text-muted">
          {t("discard.body")}
        </p>
        <div className="mt-4 flex justify-end gap-2">
          <button
            type="button"
            onClick={onKeep}
            className="h-8 rounded-control border border-line px-3 text-[12px] font-medium text-fg transition-colors hover:bg-accent-soft"
          >
            {t("discard.keep")}
          </button>
          <button
            type="button"
            onClick={onDiscard}
            className="h-8 rounded-control bg-danger px-3 text-[12px] font-medium text-bg transition-opacity hover:opacity-90"
          >
            {t("discard.discard")}
          </button>
        </div>
      </div>
    </div>
  );
}

/* A tiny rendering of the app in each finish: field, rule, three tiles.
   Colors are the real tokens, scoped by data-theme on the swatch. */
function ThemeSwatch({ value, active, onSelect }: { value: "light" | "dark"; active: boolean; onSelect: () => void }) {
  const { t } = useTranslation("settings");
  const label = { light: t("appearance.light"), dark: t("appearance.dark") }[value];
  return (
    <button type="button" onClick={onSelect} aria-pressed={active} className="group flex flex-col gap-2.5 text-left outline-none">
      <span
        data-theme={value}
        className={`relative block aspect-[16/10] w-full overflow-hidden rounded-panel bg-bg transition-[outline-color,transform] duration-200 group-hover:-translate-y-0.5 ${
          active ? "outline-2 outline-offset-[3px] outline-ink" : "outline-0"
        }`}
        style={{ outlineStyle: active ? "solid" : "none", boxShadow: "inset 0 0 0 1px var(--border)" }}
      >
        <span className="absolute bottom-3 left-3 right-3 top-9 grid grid-cols-3 gap-1.5">
          {[0, 1, 2, 3, 4, 5].map((i) => (
            <span
              key={i}
              className="block rounded-small"
              style={{
                background: i % 3 === 1 ? "var(--fg-subtle)" : "var(--bg-elevated)",
                opacity: i % 3 === 1 ? 0.5 : 1,
                boxShadow: "var(--panel-edge)",
              }}
            />
          ))}
        </span>
        <span className="control absolute left-3 right-3 top-2.5 flex h-5 items-center rounded-full px-2">
          <span className="h-1 w-1/3 rounded-full bg-fg opacity-70" />
        </span>
      </span>
      <span className={`px-0.5 text-[13px] ${active ? "font-medium text-fg" : "text-muted"}`}>{label}</span>
    </button>
  );
}
