import { useCallback, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/**
 * Surface finish, independent of light/dark: "solid" is the default look,
 * "frost" is a soft frosted blur and "glass" a deeper blur with a specular
 * edge. The picked value is stored; what the page shows is `effectiveSurface`,
 * which falls back to solid when the OS asks for less transparency (where the
 * webview reports it) or when the webview cannot blur.
 */
export type Surface = "solid" | "frost" | "glass";

export const SURFACES: readonly Surface[] = ["solid", "frost", "glass"];

const STORAGE_KEY = "kensaku.surface";

export function normalizeSurface(raw: string | null | undefined): Surface {
  return raw === "frost" || raw === "glass" ? raw : "solid";
}

/** What to paint for a pick, given the platform's transparency settings. */
export function effectiveSurface(pick: Surface, reducedTransparency: boolean, canBlur: boolean): Surface {
  return reducedTransparency || !canBlur ? "solid" : pick;
}

function reducedTransparency(): boolean {
  try {
    // shortcut: Chromium (WebView2) ignores prefers-reduced-transparency, so on
    // Windows only high-contrast users fall back automatically. Read the OS
    // setting in Rust if that proves to matter.
    return (
      window.matchMedia("(prefers-reduced-transparency: reduce)").matches ||
      window.matchMedia("(prefers-contrast: more)").matches
    );
  } catch {
    return false;
  }
}

function canBlur(): boolean {
  try {
    return CSS.supports("backdrop-filter", "blur(1px)") || CSS.supports("-webkit-backdrop-filter", "blur(1px)");
  } catch {
    return false;
  }
}

let current: Surface = "solid";
let listening = false;

/** Paints a pick on the page and keeps it in step with OS setting changes. */
export function applySurface(pick: Surface) {
  current = pick;
  document.documentElement.dataset.surface = effectiveSurface(pick, reducedTransparency(), canBlur());
  if (listening) return;
  listening = true;
  try {
    for (const query of ["(prefers-reduced-transparency: reduce)", "(prefers-contrast: more)"]) {
      window.matchMedia(query).addEventListener("change", () => applySurface(current));
    }
  } catch {
    /* no matchMedia: the pick stays as painted */
  }
}

export function storeSurface(pick: Surface) {
  try {
    localStorage.setItem(STORAGE_KEY, pick);
  } catch {
    /* storage unavailable (e.g. webview restrictions) */
  }
}

function readStored(): Surface {
  try {
    return normalizeSurface(localStorage.getItem(STORAGE_KEY));
  } catch {
    return "solid";
  }
}

export function useSurface() {
  const [surface, setSurfaceState] = useState<Surface>(readStored);

  const setSurface = useCallback((pick: Surface) => {
    setSurfaceState(pick);
    storeSurface(pick);
    applySurface(pick);
    invoke("set_surface", { surface: pick }).catch((err) => console.warn("set_surface failed:", err));
  }, []);

  return { surface, setSurface };
}
