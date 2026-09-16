import { useEffect, useState } from "react";
import { invoke, listen } from "./desktop";

export type DesktopUpdate = {
  enabled: boolean;
  phase: "idle" | "checking" | "available" | "downloading" | "ready" | "installing" | "error";
  version?: string;
  percent?: number;
  error?: string;
};

export function useDesktopUpdate(): DesktopUpdate | null {
  const [state, setState] = useState<DesktopUpdate | null>(null);
  useEffect(() => {
    if (!window.nexusDesktop) return;
    let active = true;
    const receive = (value: DesktopUpdate) => {
      if (active) setState(value);
    };
    // Subscribe before reading so a completed background download is not lost.
    const subscription = listen<DesktopUpdate>("nexus-update", (event) => receive(event.payload));
    void invoke<DesktopUpdate>("update_status")
      .then(receive)
      .catch(() => {});
    return () => {
      active = false;
      void subscription.then((stop) => stop());
    };
  }, []);
  return state;
}
