import { For, Show } from "solid-js";
import {
  SOURCE_LABEL,
  delayParts,
  endedAgo,
  fmtBirth,
  fmtDelay,
  fmtSpan,
  growth,
  peak,
} from "../lib/format";
import type { LeaderboardEntry } from "../lib/types";

interface Fact {
  term: string;
  value: string;
  title?: string;
}

/**
 * Everything about one trip: the delay first and biggest, then who it belongs to,
 * then the evidence, then the small print.
 */
export function TripCard(props: {
  entry: LeaderboardEntry;
  total: number;
  /** The server's clock, for ageing a finished trip. */
  now: number;
}) {
  const e = () => props.entry;
  const ago = () => endedAgo(e(), props.now);

  const facts = (): Fact[] => {
    const list: Fact[] = [];
    const finished = ago();
    if (finished === null) {
      list.push({ term: "Next stop", value: e().next_stop || "unknown" });
    } else {
      list.push({
        term: "Ended",
        value: `${fmtSpan(finished)} ago`,
        title: "Stamped at its last sighting, not when we noticed it was gone.",
      });
    }
    list.push({ term: "Vehicle", value: e().vehicle || "unidentified" });
    list.push({
      term: "Watched",
      value: `${fmtDelay(e().tracked_seconds)} · +${fmtDelay(growth(e()))}`,
      title:
        `watched for ${fmtDelay(e().tracked_seconds)}; was ${fmtBirth(e())} when first ` +
        `seen, and lost ${fmtDelay(growth(e()))} under observation`,
    });
    list.push({ term: "Delay source", value: SOURCE_LABEL[e().source] ?? e().source });
    list.push({ term: "Agency feed", value: e().slug });
    return list;
  };

  return (
    <>
      <div class="rank">
        <b>#{e().rank}</b>
        {` of ${props.total} most delayed`}
      </div>

      <div class="delay">
        <For each={delayParts(peak(e()))}>
          {([n, u]) => (
            <>
              {String(n)}
              <span class="u">{u}</span>
            </>
          )}
        </For>
      </div>

      <p class="late-label">
        {ago() === null ? "behind schedule" : "behind schedule when it ended"}
        <Show when={ago() === null && peak(e()) !== e().delay_seconds}>
          <span class="re">{` · now reporting ${fmtSpan(e().delay_seconds)}`}</span>
        </Show>
      </p>

      <h2 class="agency">
        {e().agency}
        <Show when={ago() !== null}>
          <span
            class="ended-tag"
            title="This trip has ended. It stays on the board, fading, for up to 24 hours."
          >
            finished {fmtSpan(ago()!)} ago
          </span>
        </Show>
      </h2>

      <p class="route">
        <span class="badge">{e().route || "—"}</span>
        <Show when={e().headsign}>
          <span class="to">→</span>
          {e().headsign}
        </Show>
      </p>

      {/* The receipts. This is the site's actual claim — not "a feed said two
          hours" but "we watched it lose two hours" — so it sits above the small
          print rather than in a tooltip. */}
      <p class="prov">
        It was <b>{fmtBirth(e())}</b> when we first saw it {fmtSpan(e().tracked_seconds)}{" "}
        ago, and has lost <b>{fmtSpan(growth(e()))}</b> since — under observation the
        whole time.
      </p>

      <dl>
        <For each={facts()}>
          {(f) => (
            <>
              <dt title={f.title}>{f.term}</dt>
              <dd title={f.title}>{f.value}</dd>
            </>
          )}
        </For>
      </dl>
    </>
  );
}
