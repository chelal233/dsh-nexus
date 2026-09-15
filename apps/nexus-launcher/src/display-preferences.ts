const KEY = "nexus.launcher.zoom";
export const ZOOM_CHANGED = "nexus-zoom-changed";
export const ZOOM_LEVELS = [80, 90, 100, 110, 125, 150, 175, 200];
let sessionZoom: number | undefined;

export function displayZoom(): number {
  if (sessionZoom !== undefined) return sessionZoom;
  try {
    const value = Number(window.localStorage.getItem(KEY));
    return (sessionZoom = ZOOM_LEVELS.includes(value) ? value : 100);
  } catch {
    return 100;
  }
}

export function setDisplayZoom(value: number): void {
  if (!ZOOM_LEVELS.includes(value)) return;
  sessionZoom = value;
  document.documentElement.style.zoom = String(value / 100);
  try {
    window.localStorage.setItem(KEY, String(value));
  } catch {
    /* The current page can still scale without storage. */
  }
  window.dispatchEvent(new CustomEvent(ZOOM_CHANGED, { detail: value }));
}
