import { createEffect, createMemo, createSignal, on, type Accessor } from "solid-js";
import { tripKey } from "../lib/format";
import type { LeaderboardEntry } from "../lib/types";

export interface Selection {
  index: Accessor<number>;
  current: Accessor<LeaderboardEntry | null>;
  /** Move to an index, wrapping — the rotation is a loop, not a queue. */
  select: (i: number) => void;
}

/**
 * Which entry is on screen.
 *
 * The selection is held by **trip identity**, not index: a 15s push reorders the
 * board, and a trip must not be yanked off the screen because something worse
 * appeared above it. A trip that falls off the board entirely leaves the view
 * holding the position it occupied.
 */
export function createSelection(entries: Accessor<LeaderboardEntry[]>): Selection {
  const [index, setIndex] = createSignal(0);
  let selectedKey: string | null = null;

  createEffect(
    on(entries, (list) => {
      if (!list.length) return;
      const at = list.findIndex((e) => tripKey(e) === selectedKey);
      const next = at >= 0 ? at : Math.min(index(), list.length - 1);
      setIndex(next);
      selectedKey = tripKey(list[next]);
    }),
  );

  const select = (i: number) => {
    const list = entries();
    if (!list.length) return;
    const n = ((i % list.length) + list.length) % list.length;
    setIndex(n);
    selectedKey = tripKey(list[n]);
  };

  const current = createMemo(() => {
    const list = entries();
    if (!list.length) return null;
    return list[Math.min(index(), list.length - 1)] ?? null;
  });

  return { index, current, select };
}
