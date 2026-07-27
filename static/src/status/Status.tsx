import { For, Match, Show, Switch, createEffect, createMemo, createSignal } from "solid-js";
import type { SourceStatus } from "../lib/types";
import { age, shortStatus, stateClass, statusText, tipText } from "./derive";
import { createReport } from "./report";

/** A column of the table. `status`, `age` and `hot` are derived, not sent. */
interface Column {
  key: string;
  label: string;
  num?: boolean;
}

const COLS: Column[] = [
  { key: "display_name", label: "Agency" },
  { key: "status", label: "Status" },
  { key: "vehicles_now", label: "Vehicles", num: true },
  { key: "late_trips", label: "Late now", num: true },
  { key: "vetted_out", label: "Unvouched", num: true },
  { key: "peak_vehicles", label: "Peak", num: true },
  { key: "total_trips", label: "Sched trips", num: true },
  { key: "poll_interval_seconds", label: "Poll (s)", num: true },
  { key: "age", label: "Last (s)", num: true },
  { key: "hot", label: "Board", num: true },
];

function valueOf(
  s: SourceStatus,
  col: Column,
  generatedAt: number,
): string | number | null {
  if (col.key === "status") return shortStatus(s);
  if (col.key === "age") return age(s, generatedAt);
  if (col.key === "hot") return s.hot ? 1 : 0;
  return (s as unknown as Record<string, string | number | null>)[col.key] ?? null;
}

/**
 * One dot. It owns the "freshly polled" blink, which is why it's a component: the
 * blink is a one-shot on a *change* of `last_poll`, and never on first render.
 */
function Led(props: { source: SourceStatus; onHover: (s: SourceStatus | null) => void }) {
  let el!: HTMLDivElement;
  let lastPoll: number | null = null;

  createEffect(() => {
    const poll = props.source.last_poll ?? null;
    if (poll != null && lastPoll != null && poll > lastPoll) {
      el.animate([{ filter: "brightness(1.9)" }, { filter: "brightness(1)" }], {
        duration: 800,
        easing: "ease-out",
      });
    }
    if (poll != null) lastPoll = poll;
  });

  return (
    <div
      ref={el}
      class={`led ${stateClass(props.source)}`}
      classList={{ hot: props.source.hot }}
      onMouseEnter={() => props.onHover(props.source)}
      onMouseLeave={() => props.onHover(null)}
    />
  );
}

/** One cell. Only the status cell is tinted; the rest is just a value. */
function Cell(props: { source: SourceStatus; col: Column; generatedAt: number }) {
  const plain = () => {
    const v = valueOf(props.source, props.col, props.generatedAt);
    if (v == null) return "—";
    return props.col.num ? (v as number).toLocaleString() : v;
  };
  return (
    <Switch fallback={<td classList={{ num: props.col.num }}>{plain()}</td>}>
      <Match when={props.col.key === "status"}>
        <td class={`st ${stateClass(props.source)}`} title={statusText(props.source)}>
          {shortStatus(props.source)}
        </td>
      </Match>
      <Match when={props.col.key === "hot"}>
        <td class="num">{props.source.hot ? "★" : ""}</td>
      </Match>
    </Switch>
  );
}

/** One `<tr>` for a source. */
function Row(props: { source: SourceStatus; generatedAt: number }) {
  return (
    <tr>
      <For each={COLS}>
        {(c) => <Cell source={props.source} col={c} generatedAt={props.generatedAt} />}
      </For>
    </tr>
  );
}

export function Status() {
  const report = createReport();

  const [sort, setSort] = createSignal<{ key: string; dir: 1 | -1 }>({
    key: "vehicles_now",
    dir: -1,
  });

  /**
   * The one ordering both halves of the page use. Keeping the LED grid in the
   * same order as the table is what makes re-sorting the table re-sort the dots.
   *
   * The array holds the store's own row proxies, whose identity is stable per
   * slug — so `<For>` *moves* the existing dots and rows on a re-sort rather than
   * rebuilding them (which would restart every blink).
   */
  const sorted = createMemo(() => {
    const { key, dir } = sort();
    const col = COLS.find((c) => c.key === key) ?? COLS[0];
    const now = report.generatedAt();
    return report
      .slugs()
      .map((slug) => report.sources[slug])
      .sort((a, b) => {
        const x = valueOf(a, col, now);
        const y = valueOf(b, col, now);
        if (col.num)
          return dir * (((x as number) ?? -Infinity) - ((y as number) ?? -Infinity));
        return (
          dir * String(x).localeCompare(String(y), undefined, { sensitivity: "base" })
        );
      });
  });

  const onSort = (c: Column) =>
    setSort((s) =>
      s.key === c.key ? { key: s.key, dir: (s.dir * -1) as 1 | -1 } : { key: c.key, dir: c.num ? -1 : 1 },
    );

  // --- the tooltip ------------------------------------------------------------
  // One custom tooltip, positioned by the cursor. Holding the hovered source
  // (rather than a snapshot of its text) is what keeps it live while it's open.
  const [hovered, setHovered] = createSignal<SourceStatus | null>(null);
  const [pos, setPos] = createSignal({ x: 0, y: 0 });
  let tip!: HTMLDivElement;

  const onMouseMove = (ev: MouseEvent) => {
    if (!hovered()) return;
    setPos({
      x: Math.min(ev.clientX + 14, window.innerWidth - tip.offsetWidth - 8),
      y: ev.clientY + 14,
    });
  };

  const summary = () => report.summary();

  return (
    <>
      <h1>America's Most Delayed — source status</h1>
      <p>
        <a href="index.html">← leaderboard</a>
      </p>

      <div id="summary">
        {/* A dropped connection replaces the summary rather than sitting quietly
            beside a frozen one — the counts would otherwise read as current. */}
        <Show
          when={!report.disconnected() && summary()}
          fallback={report.disconnected() ? "disconnected — retrying…" : "connecting…"}
        >
          {(s) => (
            <>
              <b>{s().total_sources}</b> sources · <b>{s().active}</b> active ·{" "}
              <b>{s().hot}</b> on leaderboard · <b>{s().requires_auth}</b> need auth ·{" "}
              <b>{s().no_realtime}</b> no realtime · <b>{s().no_vehicle_positions}</b> no
              vehicle pos · <b>{s().failed}</b> failed · <b>{s().loading}</b> loading
              GTFS · <b>{s().vehicles_now.toLocaleString()}</b> vehicles tracked now
            </>
          )}
        </Show>
      </div>

      <div class="legend">
        <span>
          <span class="led ok" />
          active
        </span>
        <span>
          <span class="led warn" />
          error
        </span>
        <span>
          <span class="led auth" />
          needs auth
        </span>
        <span>
          <span class="led nort" />
          no realtime
        </span>
        <span>
          <span class="led novp" />
          no vehicle pos
        </span>
        <span>
          <span class="led failed" />
          failed
        </span>
        <span>
          <span class="led loading" />
          loading GTFS
        </span>
        <span>
          <span class="led ok hot" />
          on leaderboard
        </span>
        <span>· hover a dot for details</span>
      </div>

      <div id="grid" onMouseMove={onMouseMove}>
        <For each={sorted()}>
          {(source) => <Led source={source} onHover={setHovered} />}
        </For>
      </div>

      <div
        id="tip"
        ref={tip}
        style={{
          display: hovered() ? "block" : "none",
          left: `${pos().x}px`,
          top: `${pos().y}px`,
        }}
      >
        <Show when={hovered()}>{(s) => tipText(s(), report.generatedAt())}</Show>
      </div>

      <h2>
        All sources{" "}
        <span style={{ "font-weight": 400, color: "#888", "font-size": ".85rem" }}>
          (click a column to sort)
        </span>
      </h2>
      <table id="tbl">
        <thead>
          <tr>
            <For each={COLS}>
              {(c) => (
                <th classList={{ num: c.num }} onClick={() => onSort(c)}>
                  {c.label}
                  {sort().key === c.key ? (sort().dir < 0 ? " ▼" : " ▲") : ""}
                </th>
              )}
            </For>
          </tr>
        </thead>
        <tbody>
          <For each={sorted()}>
            {(source) => <Row source={source} generatedAt={report.generatedAt()} />}
          </For>
        </tbody>
      </table>
    </>
  );
}
