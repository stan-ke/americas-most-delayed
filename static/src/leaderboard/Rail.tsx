import { For, createEffect } from "solid-js";
import { fmtSpan, peak } from "../lib/format";
import type { LeaderboardEntry } from "../lib/types";

/** The rest of the board: one numbered chip per rank, jump to any of them. */
export function Rail(props: {
  entries: LeaderboardEntry[];
  index: number;
  onSelect: (i: number) => void;
}) {
  let rail!: HTMLDivElement;

  // Keep the current chip visible when the board is wider than the rail.
  createEffect(() => {
    const el = rail.children[props.index] as HTMLElement | undefined;
    el?.scrollIntoView({ block: "nearest", inline: "nearest" });
  });

  return (
    <div id="rail" ref={rail}>
      <For each={props.entries}>
        {(e, i) => (
          <button
            classList={{ on: i() === props.index, done: e.ended_at != null }}
            title={
              `${e.agency} · route ${e.route} · ${fmtSpan(peak(e))} late` +
              (e.ended_at != null ? " · finished" : "")
            }
            onClick={() => props.onSelect(i())}
          >
            {e.rank}
          </button>
        )}
      </For>
    </div>
  );
}
