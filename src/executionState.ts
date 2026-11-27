import type { ProcessingSnapshot } from "./types";
// Subscribe first, then query. A delayed query/reply cannot overwrite an event.
// A different application session is not evidence of a newer run.
export function newerProcessingSnapshot(current: ProcessingSnapshot | null, incoming: ProcessingSnapshot): boolean {
  return !current || current.sessionId === incoming.sessionId && incoming.revision > current.revision;
}
