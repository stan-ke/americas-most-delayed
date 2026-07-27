import { createSignal, onCleanup, type Accessor, type Setter } from "solid-js";

export interface Rotation {
  paused: Accessor<boolean>;
  setPaused: Setter<boolean>;
  /** 0..1 through the current dwell; 0 whenever nothing is rotating. */
  progress: Accessor<number>;
  /** Start the dwell over — what a manual step does. */
  reset: () => void;
}

/**
 * The dwell timer.
 *
 * It runs off `requestAnimationFrame` rather than `setTimeout`, for three
 * properties at once: the progress bar stays exactly in step with the countdown,
 * pausing is just "stop accumulating", and a **hidden tab stops rotating
 * entirely** (rAF doesn't fire there) — so a backgrounded wall display doesn't
 * silently race through the board and come back somewhere arbitrary.
 */
export function createRotation(opts: {
  dwellMs: number;
  /** Whether rotation should be running at all (a board of 0 or 1 shouldn't). */
  active: Accessor<boolean>;
  onAdvance: () => void;
}): Rotation {
  const [paused, setPaused] = createSignal(false);
  const [progress, setProgress] = createSignal(0);

  let elapsed = 0;
  let last = performance.now();
  let frame = 0;

  const tick = (now: number) => {
    const dt = now - last;
    last = now;
    const rotating = !paused() && opts.active();
    if (rotating) {
      elapsed += dt;
      if (elapsed >= opts.dwellMs) {
        elapsed = 0;
        opts.onAdvance();
      }
    }
    setProgress(rotating ? Math.min(elapsed / opts.dwellMs, 1) : 0);
    frame = requestAnimationFrame(tick);
  };
  frame = requestAnimationFrame(tick);
  onCleanup(() => cancelAnimationFrame(frame));

  return {
    paused,
    setPaused,
    progress,
    reset: () => {
      elapsed = 0;
      setProgress(0);
    },
  };
}
