type DesktopApi = {
  readonly systemLanguages?: readonly string[];
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  listen<T>(event: string, callback: (event: { payload: T }) => void): () => void;
};
declare global {
  interface Window {
    nexusDesktop?: DesktopApi;
  }
}
export async function invoke<T = void>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  if (window.nexusDesktop) return window.nexusDesktop.invoke<T>(command, args);
  throw new Error("Desktop host unavailable");
}
export async function listen<T>(
  event: string,
  callback: (event: { payload: T }) => void,
): Promise<() => void> {
  if (window.nexusDesktop) return window.nexusDesktop.listen<T>(event, callback);
  return () => undefined;
}
