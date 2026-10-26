# TVP Recording Safety Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the Polsat-discovered lifecycle/recording failures confirmed in TVP.

**Architecture:** Retain TVP's resolver and single selected HLS master. Replace
established-response restart with bounded startup and one owned streaming child;
use per-connection HTTP/1 abort control and fixed-PID A/V activity supervision.

**Tech Stack:** Rust, Axum0.8, Tokio, FFmpeg/ffprobe, existing wiremock tests.

**Spec:** [Recording-safety corrections](../specs/2026-10-25-recording-safety.md).

**Execution:** User approved Native execution. All three native tasks and one
independent review/fix pass are finished;77 tests, fmt, Clippy and diff checks
pass. The operator subsequently authorized commit/push/main consolidation and
evidence-preserving worktree cleanup on2026-10-26. See the
[acceptance checkpoint](../../acceptance/2026-10-25-recording-safety.md) for
deferred telemetry and pending Linux/live/DVR gates. No server actions performed.

## Global Constraints

- No process replacement after HTTP200; fatal EOF aborts transport.
- Both tracks mandatory; one complete HLS input and codec-copy MPEG-TS.
- Bootstrap/queued media bounded to2MiB; startup120s; downstream/per-track30s.
- Use owned per-connection control, never unrelated sockets/processes.
- Preserve resolver/quality selection, unrelated tests and bounded metrics.
- No production/server operations; no TVP commits/pushes without authorization.

## Review Focus

1. A blocked HTTP writer must not retain its body after releasing capacity:
   Task1 raw unread-socket reset/repeated-capacity test.
2. New FFmpeg clocks must not be spliced into one recording:
   Task2 real one-pass HLS packet/decode timeline and no-respawn test.
3. Audio-only bytes must not conceal stalled video (or vice versa):
   Task3 startup and per-track activity regressions.
4. Cancellation during startup/full queues must reap children and clean private
   masters without touching other streams: Tasks1–3 ownership regressions.
5. Metrics/HEAD must not silently start work or misreport removed recovery:
   Task1 HEAD no-side-effects; Task2 metrics assertions and README review.

---

## Files and shared interfaces

- Modify `src/server.rs`: session/HTTP lifecycle, FFmpeg argv and supervision.
- Create `src/connection.rs`: owned HTTP connection abort; register in `src/lib.rs`.
- Create `src/transport.rs`: bounded fixed-PID activity parsing; register in lib.
- Modify `src/main.rs`: state construction for bounded startup/stream timeouts.
- Modify `src/metrics.rs`: truthful upstream/timeout end reasons; no active restarts.
- Modify `Cargo.toml`/lock only for required socket/runtime features; prefer
  existing dependencies and std owned-descriptor support.
- Modify `tests/integration.rs`; add process-fixture support if required to emit
  valid TS packets without disguising test text as production MPEG-TS.
- Modify `README.md`: new recovery semantics and actual validation limitations.

### Task 1: Bounded stalled responses and metadata-only HEAD

**Interfaces:** `serve(TcpListener,AppState)` keeps its public signature;
connection info carries an owned socket-abort handle to each admitted session.
Use a30-second downstream deadline (state duration adjustable by existing test
construction), not another indefinite send/drain wait.

- [ ] Write `head_does_not_resolve_or_spawn` (no Content-Length, no API requests,
  no child/slot use) and `stalled_readers_are_reset_without_draining` (raw sockets,
  constrained receive window, child gone, peer reset, slot reusable repeatedly).
- [ ] Run each against current code; expect HEAD side effects and stall timeout.
- [ ] Add explicit HEAD, owned descriptor control, deadline around backpressure
  and terminal draining; reset before releasing timed-out connection ownership.
  Explicitly restrict serving to HTTP/1 so one socket maps to one active stream.
- [ ] Run targeted tests and whole suite. Preserve healthy-stream and shutdown
  behavior and record outcomes; no commit step without authorization.

### Task 2: No new timeline on an established response

**Interfaces:** Task1 connection control remains available in session cleanup.
Remove established-response restart; preserve appropriate initial error statuses.
Retire unused retry state and make end-reason counters describe upstream exits.

- [ ] Write `post_start_exit_never_replays_input`: record generated six-second
  HLS once; assert150 video packets, full decode, no backward DTS and no new child.
  Add a controlled process-exit/no-respawn test and expect current code to replay.
- [ ] Replace the established restart loop with one owned-child forward/cleanup
  path. Ensure even successful FFmpeg EOF aborts the live HTTP response.
- [ ] Update prior retry-policy tests to the explicitly approved no-splice
  contract, retaining checks for API errors, cancellation, metrics, separate
  private masters, admission and cross-client independence.
- [ ] Run targeted tests and whole suite, update README token-expiry claims and
  metrics documentation. No image build/deploy or TVP commit/push.

### Task 3: Both-track startup and per-track liveness

**Interfaces:** Fixed output PIDs256/257 consumed by bounded `Tracker::feed`;
startup retains at most2MiB before handing chunks to the bounded live queue.

- [ ] Add audio-only/video-only startup failures and one-track-stall regressions,
  plus large-keyframe-before-audio success and cancellation-before-first-byte.
  Run before changes; expect current success/stall bugs.
- [ ] Require mandatory video/audio maps, disable subtitle/data, assign fixed
  PIDs and inspect payload activity. Bound initial A/V bootstrap to120s/2MiB,
  then abort on30s silence of either track. Reserve queue space before allocating
  more media. Reap child and release private master/permit on every failure.
- [ ] Adapt process test fixtures to valid transport packets where the new
  contract requires them; preserve metrics bytes/first-output coverage.
- [ ] Run `CI=true PATH="/opt/homebrew/opt/ffmpeg-full/bin:$PATH" cargo test --locked`,
  `cargo fmt --all --check` and `cargo clippy --all-targets --locked -- -D warnings`.
  Expected all tests/checks pass, no media skips. Get one independent local review
  of the whole change; fix confirmed Important findings test-first.

## Plan review

All requested failure classes map to tasks; durations match the spec, shared
connection/transport ownership is explicit, and external deployment is excluded.
Operator approved this plan and Native execution. TVP baseline full suite was
green; reset-timeline assertions changed only for the approved recording-safe
policy. The acceptance checkpoint records the final verified native scope.
