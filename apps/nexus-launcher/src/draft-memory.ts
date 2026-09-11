import { createContext, useCallback, useContext, useState, type Dispatch, type SetStateAction } from "react";

// Drafts live only as long as App. Never put snapshots, tokens or this store in browser storage.
export function createDraftMemory() {
  const values = new Map<string, { value: unknown }>();
  return {
    slot<T>(scope: string, key: string, initial: () => T): { value: T } {
      const id = JSON.stringify([scope, key]);
      if (!values.has(id)) values.set(id, { value: initial() });
      return values.get(id) as { value: T };
    },
  };
}
export const DraftMemoryContext = createContext<{ store: ReturnType<typeof createDraftMemory>; scope: string } | null>(null);
export function useDraftState<T>(key: string, initial: T | (() => T)): [T, Dispatch<SetStateAction<T>>] {
  const memory = useContext(DraftMemoryContext);
  const [slot] = useState(() => {
    const init = () => typeof initial === "function" ? (initial as () => T)() : initial;
    return memory ? memory.store.slot(memory.scope, key, init) : { value: init() };
  });
  const [value, render] = useState(slot.value);
  const set = useCallback((next: SetStateAction<T>) => {
    slot.value = typeof next === "function" ? (next as (previous: T) => T)(slot.value) : next;
    render(slot.value);
  }, [slot]);
  return [value, set];
}
export function useDraftReference<T>(key: string, initial: T): { current: T } {
  const [reference] = useDraftState(key, () => ({ current: initial }));
  return reference;
}
