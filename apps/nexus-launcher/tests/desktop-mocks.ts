const listeners = new Map<string, Set<(event: { payload: unknown }) => void>>();
export function mockIPC(handler: (command: string, payload: any) => any, _options?: unknown) {
  window.nexusDesktop = {
    invoke: async (command, args) => handler(command, args ?? {}),
    listen(name, callback) {
      if (!listeners.has(name)) listeners.set(name, new Set());
      const listener = callback as (event: { payload: unknown }) => void;
      listeners.get(name)!.add(listener);
      return () => listeners.get(name)?.delete(listener);
    },
  };
}
export async function emit(name: string, payload: unknown) {
  for (const callback of listeners.get(name) ?? []) callback({ payload });
}
export function clearMocks() { delete window.nexusDesktop; listeners.clear(); }
