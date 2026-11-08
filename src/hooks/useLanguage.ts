import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import i18n from "../i18n";
import { SUPPORTED_LANGUAGES, type SupportedLanguage } from "../i18n";

const STORAGE_KEY = "kensaku.language";

function systemLanguage(): SupportedLanguage {
  try {
    const raw = (navigator.language || "").toLowerCase();
    if (raw.startsWith("zh")) return "zh";
    if (raw.startsWith("ru")) return "ru";
    return "en";
  } catch {
    return "en";
  }
}

function isSupported(v: string | null | undefined): v is SupportedLanguage {
  return !!v && Object.prototype.hasOwnProperty.call(SUPPORTED_LANGUAGES, v);
}

export function normalizeLanguage(raw: string | null | undefined): SupportedLanguage | null {
  if (raw === null || raw === undefined || raw === "") return null;
  const v = raw.trim().toLowerCase();
  return isSupported(v) ? v : null;
}

function readStored(): SupportedLanguage | null {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored === null) return null;
    return normalizeLanguage(stored);
  } catch {
    return null;
  }
}

export function useLanguage() {
  const [language, setLanguageState] = useState<SupportedLanguage | null>(readStored);

  useEffect(() => {
    const lang = language ?? systemLanguage();
    const target = isSupported(lang) ? lang : "en";
    if (i18n.language !== target) i18n.changeLanguage(target);
    try {
      if (language === null) localStorage.removeItem(STORAGE_KEY);
      else localStorage.setItem(STORAGE_KEY, language);
    } catch {
      /* ignore */
    }
    document.documentElement.lang = target;
  }, [language]);

  useEffect(() => {
    let cancelled = false;
    invoke<{ language?: string | null }>("get_config")
      .then((cfg) => {
        if (cancelled) return;
        const persisted = normalizeLanguage(cfg?.language ?? null);
        if (readStored() === null) {
          setLanguageState(persisted);
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

  const setLanguage = useCallback((lang: SupportedLanguage | null) => {
    // localStorage is the immediate source of truth (the hook writes it in the
    // effect above). Settings also updates its config snapshot and persists it
    // with the normal Save, so a read-modify-write here would only race that
    // Save and could clobber other edits.
    setLanguageState(lang);
  }, []);

  return {
    language,
    resolvedLanguage: (language ?? systemLanguage()) as SupportedLanguage,
    setLanguage,
    supportedLanguages: SUPPORTED_LANGUAGES,
  };
}
