import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useRef, useState } from "react";
import { toAppMessage } from "./i18n";
import type { AppMessage, HistoryWindowSnapshot } from "./types";

export function newerHistorySnapshot(current: HistoryWindowSnapshot | null, next: HistoryWindowSnapshot) {
  return !current || next.sessionId === current.sessionId && next.revision > current.revision;
}
export function useHistoryWindowState(onError: (message: AppMessage) => void) {
  const [snapshot, setSnapshot] = useState<HistoryWindowSnapshot | null>(null);
  const current = useRef<HistoryWindowSnapshot | null>(null);
  const errorRef = useRef(onError); errorRef.current = onError;
  function receive(next: HistoryWindowSnapshot) {
    if (!newerHistorySnapshot(current.current, next)) return;
    current.current = next; setSnapshot(next);
  }
  useEffect(() => {
    let disposed = false;
    const accept = (next: HistoryWindowSnapshot) => { if (!disposed) receive(next); };
    const unlisten = listen<HistoryWindowSnapshot>("history-window-state", event => accept(event.payload)).then(dispose => {
      if (disposed) { dispose(); return () => {}; }
      void invoke<HistoryWindowSnapshot>("history_window_snapshot").then(accept).catch(reason => { if (!disposed) errorRef.current(toAppMessage(reason)); });
      return dispose;
    }).catch(reason => { if (!disposed) errorRef.current(toAppMessage(reason)); return () => {}; });
    return () => { disposed = true; void unlisten.then(dispose => dispose()); };
  }, []);
  return { snapshot, current, receive };
}
