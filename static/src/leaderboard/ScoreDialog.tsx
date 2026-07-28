import { For, Show, createEffect } from "solid-js";
import type { LeaderboardEntry } from "../lib/types";

/**
 * How this entry's rank was computed — the audit trail for a ranking that is
 * otherwise deliberately hidden.
 *
 * The board is ranked on a hidden score: mostly minutes late, adjusted for how
 * badly the delay breaks that particular trip, how many stops it still has to
 * ruin, and how much of the lateness the server actually watched accumulate, then
 * faded out once the trip ends. The score isn't shown on the public page (an
 * unverifiable number undercuts a site whose whole claim is that its delays are
 * real), so this is how you audit the ordering when `AMD_DEBUG` is on.
 */
export function ScoreDialog(props: {
	entry: LeaderboardEntry | null;
	open: boolean;
	onClose: () => void;
}) {
	let dialog!: HTMLDialogElement;

	createEffect(() => {
		if (props.open && !dialog.open) dialog.showModal();
		else if (!props.open && dialog.open) dialog.close();
	});

	const breakdown = () => props.entry?.score_breakdown;

	return (
		<dialog id="scorebox" ref={dialog} onClose={() => props.onClose()}>
			<div class="inner">
				<Show
					when={breakdown()}
					fallback={
						<p>
							No breakdown in this snapshot — is AMD_DEBUG set on the server?
						</p>
					}
				>
					{(b) => (
						<>
							<h3>
								#{props.entry?.rank} {props.entry?.agency} — route{" "}
								{props.entry?.route}
							</h3>
							<table>
								<tbody>
									<For each={b().steps}>
										{(step) => (
											<tr>
												<td>
													{step.name}
													<div class="detail">{step.detail}</div>
												</td>
												<td class="num">{step.value.toFixed(4)}</td>
											</tr>
										)}
									</For>
									<tr class="total">
										<td>= score</td>
										<td class="num">{b().score.toFixed(3)}</td>
									</tr>
								</tbody>
							</table>
							<p class="detail">
								Every factor multiplies the minutes late, so the board stays a
								delay ranking — the factors only reorder trips already in the
								same league. An unknown input scores 1.0 (neutral), never a
								penalty.
							</p>
						</>
					)}
				</Show>
				<button class="close" onClick={() => props.onClose()}>
					close
				</button>
			</div>
		</dialog>
	);
}
