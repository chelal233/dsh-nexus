import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";

const STORAGE_KEY = "nexus.notifications.enabled";

export function notificationsEnabledPreference(): boolean {
  try {
    return window.localStorage.getItem(STORAGE_KEY) !== "0";
  } catch {
    return true;
  }
}

export function setNotificationsEnabledPreference(enabled: boolean): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, enabled ? "1" : "0");
  } catch {
    // localStorage unavailable (web dev): preference simply not persisted
  }
}

export async function notify(title: string, body?: string): Promise<void> {
  if (!notificationsEnabledPreference()) return;
  try {
    let granted = await isPermissionGranted();
    if (!granted) {
      const permission = await requestPermission();
      granted = permission === "granted";
    }
    if (granted) sendNotification({ title, body });
  } catch {
    // Notifications are best-effort: outside Tauri (web dev) or denied
    // permission must never break the caller.
  }
}
