# TVP recording-safety corrections

## Request and evidence

After publishing the Polsat checkpoint, the user requested checking TVP for the
same bugs and fixing confirmed defects. The user explicitly selected the
recording-safe policy: end an established response when FFmpeg exits rather
than concatenate another process's reset timeline. This replaces the existing
documented transparent token-refresh behavior.

Inspection of `src/server.rs` found unlimited downstream sends/draining, implicit
HEAD dispatch to the streaming handler, optional audio mapping, no per-track
activity supervision, and FFmpeg restarts within established responses. The
existing real HLS integration test deliberately expects three copies of its
six-second input and notes that timestamps restart. The baseline full suite
passes (native Mac, full FFmpeg on scoped PATH, CI=true to prohibit media skips).
There is no recording-artifact checker in this repository, so Polsat's checker
certification defect has no equivalent code to patch here.

## Required correction

- Keep the TVP resolver, quality/default-language selection, stable URLs,
  one-input remux and metrics. No transcoding or channel/config expansion.
- Once HTTP200 media starts, never launch another FFmpeg for that response.
  EOF/errors end with an aborted transfer, not normal completion or a new clock.
- Before success require both video and audio payload from the owned FFmpeg;
  output contains exactly one mandatory video/audio pair, no subtitle/data tracks.
- Use the already-tested Polsat limits: bounded 2MiB bootstrap/queue, 120-second
  overall startup ceiling, 30-second downstream stall and per-track silence.
  Queue backpressure and child cleanup must not wait indefinitely.
- Reset only the timed-out owned HTTP/1 connection, including unread responses;
  release its permit after child cleanup. Do not act on bare reused descriptor
  numbers, unrelated processes, other streams or production containers.
- HEAD validates the numeric route and returns unknown-length video/mp2t
  metadata without resolving TVP, spawning FFmpeg or taking a stream slot.
- Keep metrics truthful about errors/timeouts and retirement of active restart
  behavior. Update existing assertions where the approved behavior changed;
  preserve unrelated tests and do not silently drop their coverage.
- Native source/test changes only. No server work, deployment, restarts, image
  publication, or changes to WireGuard/Jellyfin while recording safety is active.
- The user authorized the Polsat push. Leave TVP changes uncommitted unless a
  subsequent instruction authorizes committing/pushing that repository.

## Acceptance

Real HLS output must decode, contain both selected tracks and exactly one
six-second timeline rather than three replayed passes. Deterministic process/
socket tests must prove EOF abort/no respawn, startup/disconnect cleanup, missing
track rejection, one-track stalls, unread-client reset and permit recovery.
Run the complete Rust suite with media required, fmt and Clippy. Linux image and
actual Jellyfin recording acceptance remain explicitly unverified.
