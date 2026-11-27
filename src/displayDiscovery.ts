import type { DisplayInfo } from "./types";

export type DisplayDiscoveryResult =
  | { status: "success"; displays: DisplayInfo[] }
  | { status: "failure"; reason: unknown };

// Prefer stable OS-namespaced identity, not a potentially reused runtime ID.
// A missing preferred screen must be chosen again, never silently replaced.
export function chooseDetectedDisplay(displays: DisplayInfo[], preferredRef: string | null): DisplayInfo | undefined {
  return preferredRef ? displays.find(display => display.displayRef === preferredRef)
    : displays.find(display => display.isMain) ?? displays[0];
}

// One small request gate shared by startup, focus and explicit refresh.
// Coalesce simultaneous calls and discard results from an obsolete mount.
export function createDisplayDiscovery(load: () => Promise<DisplayInfo[]>) {
  let generation = 0;
  let pending: Promise<DisplayDiscoveryResult | null> | null = null;
  return {
    get version() { return generation; },
    refresh(): Promise<DisplayDiscoveryResult | null> {
      if (pending) return pending;
      const request = ++generation;
      pending = Promise.resolve().then(load)
        .then(displays => request === generation ? { status: "success" as const, displays } : null)
        .catch(reason => request === generation ? { status: "failure" as const, reason } : null)
        .finally(() => { if (request === generation) pending = null; });
      return pending;
    },
    invalidate() { generation += 1; pending = null; },
  };
}
