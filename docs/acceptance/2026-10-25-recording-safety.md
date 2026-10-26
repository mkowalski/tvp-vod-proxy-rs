# TVP recording-safety native checkpoint — 2026-10-25

## Scope and outcome

The user requested checking the Polsat-discovered failures in TVP and fixing
confirmed defects. The user explicitly approved replacing established-response
FFmpeg restart/token recovery with a recording-safe no-splice policy and then
approved the [implementation plan](../superpowers/plans/2026-10-25-recording-safety.md).
The fixes were developed in the isolated `tvp-recording-safety` worktree. The
operator subsequently authorized committing/pushing them and consolidating onto
`main`. This source integration does not authorize a TVP deployment: production
containers, WireGuard and Jellyfin remain unchanged by these fixes.

| Failure class | TVP finding and correction |
|---|---|
| Unread responses retain resources | Confirmed indefinite send/drain waits; bounded waits now reset only the owned HTTP/1 connection and reap its child. Raw unread sockets verify reset/capacity recovery without draining. |
| Reset-clock recording concatenation | Confirmed: previous real HLS test required450 frames from a150-frame input. Output now contains one150-frame pass, monotonic DTS on both tracks and a full strict decode. No replacement child or URL resolve is allowed on the established response. |
| HEAD starts media work | Confirmed by failing upstream-call assertion. Explicit HEAD now returns unknown-length metadata without contacting TVP or launching a child. |
| Missing/stalled required track | Confirmed200 for incomplete output, and audio keeping absent video alive past the deadline. Mandatory A/V maps and bounded fixed-PID payload guards now enforce startup and per-track activity. |
| Incomplete-artifact checker certification | Not applicable: this repository has no recording-artifact checker. No unrelated checker was introduced. |

Limits:120s overall startup,2MiB bootstrap/queued media budget,30s downstream or
per-track silence. Quality/default-audio selection and one-input codec-copy remux
are unchanged. HEAD indicates route syntax, not live channel availability.

## Fresh local verification

Apple arm64; scoped operator-approved Homebrew ffmpeg-full on PATH. `CI=true`
requires media tools so real media tests cannot silently skip.

| Command | Result |
|---|---|
| `CI=true PATH="/opt/homebrew/opt/ffmpeg-full/bin:$PATH" cargo test --locked` | 77 tests pass:36 unit+41 integration; zero failed/ignored |
| `cargo fmt --all --check` | Pass |
| `cargo clippy --all-targets --locked -- -D warnings` | Pass |
| `git diff --check` | Pass |

Evidence log retained in the plan workspace as `native-final.log`. Synthetic
subprocess fixtures emit transport-framed payloads to exercise lifecycle and
activity; only actual FFmpeg HLS fixtures establish codec/decode behavior.
Existing restart assertions were changed only for the explicitly approved
no-splice policy, retaining private-playlist, API-error, metrics, capacity and
cross-client cancellation coverage.

## Independent review and fix pass

One independent local whole-change review reran the then75-test suite and checks.
It found a media-deadline gap while forwarding startup buffers. A constrained
socket/large-prefix reproduction failed, then passed after every send/reserve
was bounded by the earlier per-track/downstream deadline. A media timeout aborts
transport rather than waiting another downstream interval.

The reviewer also identified initial resolution not cancelling on shutdown.
Regraded from Minor to Important because it could delay shutdown or permit an
unnecessary subsequent process launch. The delayed-resolver reproduction failed,
then passed after startup began selecting the shutdown token. Both changes are
included in the final77-test run; no second independent review was performed.

**Deferred minor:** bootstrap cancellation (disconnect/deadline) is counted as
`upstream_exit` rather than a distinct cancellation end reason. Cleanup is covered;
the limitation affects telemetry classification, not media/process ownership.
Legacy restart counters/end reasons remain zero for dashboard compatibility.

## Not a deployment or real DVR verdict

No Linux image build, Docker smoke, live TVP request, deployment, service restart,
actual Jellyfin DVR recording or operator lip-sync/seek acceptance ran. Linux
socket behavior and actual scheduled recordings remain unjudged because the
user's server-work safety boundary remains in force. Cost: release acceptance
waits for an explicitly approved safe window.

Polsat source checkpoint `4a26c713340c9c3152f973b5ee67ab94ec5eb0d6` was pushed to
the private Polsat repository's `feat/polsat-proxy` branch before the TVP audit;
TVP source publication/main consolidation was subsequently authorized on2026-10-26;
no image was published and no TVP deployment is implied.

Adjacent follow-up: Polsat's corresponding bootstrap-forwarding path appears to
have the same media-deadline gap identified in this TVP review. The pushed Polsat
checkpoint predates that finding. Polsat subsequently received the regression/fix
and its separate metrics deployment; see that repository's current acceptance
records rather than treating the initial checkpoint as the final source state.

## Source closeout — 2026-10-26

Fresh pre-integration verification again passed77 Rust tests (36 unit/41
integration), formatting, warning-denying Clippy and whitespace checks with
`CI=true` and scoped ffmpeg-full PATH. The operator requested source consolidation
and safe worktree cleanup, not a deployment or a waiver of Linux/live/DVR gates.
Original worktree source and scratch evidence were archived privately under
`/Users/mko/git/proxy-repo-archives/20261026T154331Z` before cleanup. The commit
containing this record includes the fixes; Git history retains the source while
the archive retains non-Git evidence.
