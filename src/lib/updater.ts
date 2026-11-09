import { invoke } from "@tauri-apps/api/core";
import type { UpdateInfo, UpdateStatus } from "../types";

/**
 * Checks GitHub for the latest Kensaku release.
 */
export async function checkForUpdates(): Promise<UpdateInfo> {
  return await invoke<UpdateInfo>("check_for_updates");
}

/**
 * Starts background download of the specified update asset.
 */
export async function downloadUpdate(assetUrl: string, assetName: string): Promise<UpdateStatus> {
  return await invoke<UpdateStatus>("download_update", {
    assetUrl,
    assetName,
  });
}

/**
 * Polls the current progress/status of the background updater.
 */
export async function getUpdaterStatus(): Promise<UpdateStatus> {
  return await invoke<UpdateStatus>("get_updater_status");
}

/**
 * Spawns the installer detached and terminates Kensaku so the update completes cleanly.
 */
export async function installUpdateAndRestart(): Promise<void> {
  await invoke<void>("install_update_and_restart");
}

/**
 * Formats byte count into human-readable MB / KB string.
 */
export function formatBytes(bytes?: number | null): string {
  if (!bytes || bytes <= 0) return "";
  const mb = bytes / (1024 * 1024);
  if (mb >= 1) {
    return `${mb.toFixed(1)} MB`;
  }
  const kb = bytes / 1024;
  return `${Math.round(kb)} KB`;
}

/**
 * Formats an ISO date into localized short date.
 */
export function formatReleaseDate(iso?: string | null, lang?: string): string {
  if (!iso) return "";
  try {
    const d = new Date(iso);
    return d.toLocaleDateString(lang, {
      year: "numeric",
      month: "short",
      day: "numeric",
    });
  } catch {
    return iso;
  }
}
