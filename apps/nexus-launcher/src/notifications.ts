import { invoke } from "./desktop";

const STORAGE_KEY = "nexus.notifications.enabled";

export function notificationsEnabledPreference(): boolean {
  try {
    return window.localStorage.getItem(STORAGE_KEY) !== "0";
  } catch {
    return true;
  }
}

export function setNotificationsEnabledPreference(enabled: boolean): void {
  void invoke("set_native_notifications", { enabled }).catch(() => undefined);
  try {
    window.localStorage.setItem(STORAGE_KEY, enabled ? "1" : "0");
  } catch {
    // localStorage unavailable (web dev): preference simply not persisted
  }
}

export async function notify(title: string, body?: string): Promise<void> {
  try {
    if (window.nexusDesktop) {
      await invoke("notify", { title, body });
      return;
    }
  } catch {
    // Notifications are best-effort: outside Electron (web dev) or denied
    // permission must never break the caller.
  }
}
