# tvp-vod-proxy-rs

[Versioned multi-architecture releases and deployment boundary](docs/releasing.md)

[![CI](https://github.com/mkowalski/tvp-vod-proxy-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/mkowalski/tvp-vod-proxy-rs/actions/workflows/ci.yml)

Watch and record the free live channels of Polish public TV (TVP) in
[Jellyfin](https://jellyfin.org) or any IPTV client from outside Poland.

TVP streams its channels for free on [vod.tvp.pl](https://vod.tvp.pl), but only
to Polish IP addresses, and the stream URLs are short-lived signed tokens. This
proxy resolves those URLs through a WireGuard tunnel to a server in Poland and
serves each channel at a **stable URL as one continuous MPEG-TS stream**
(one video variant + the default Polish audio), which IPTV tuners and DVRs
handle well.

Only the proxy's traffic goes through Poland. The tunnel lives inside the
Docker Compose stack's network namespace; the host and every other container
keep their normal route.

## How it works

```mermaid
flowchart LR
    TV["TV / browser"] --> JF["Jellyfin"]
    JF -->|"/tvp/ID.ts"| PX["tvp-vod-proxy"]
    PX -->|"WireGuard"| WGS["WireGuard server<br>in Poland"]
    WGS --> TVP["vod.tvp.pl<br>API + CDN"]
```

Only the proxy container uses the tunnel. The host and other containers keep
their normal route.

```mermaid
sequenceDiagram
    participant J as Jellyfin
    participant P as tvp-vod-proxy
    participant A as TVP API (via Poland)
    participant C as TVP CDN (via Poland)
    J->>P: GET /tvp/399700.ts
    P->>A: request stream URL
    A-->>P: signed HLS URL
    P->>C: fetch playlist and segments
    C-->>P: HLS video + audio
    P-->>J: one MPEG-TS stream
    Note over P,C: fatal upstream exit aborts this response; no reset timeline is appended
```

Per request for `/tvp/<id>.ts`:

1. The proxy asks TVP's API for the channel's current HLS URL (works only from
   a Polish IP, hence the tunnel).
2. It reads the master playlist and selects the highest video variant (or the
   highest under `MAX_BITRATE`, if set; if every variant is above it, the
   lowest one), plus the default audio rendition.
   DRM-protected channels are rejected with HTTP 415.
3. It writes a one-variant master playlist and runs `ffmpeg -c copy` on it,
   streaming MPEG-TS to the client. Reading video and audio as **one** HLS
   input keeps them on a single timeline (separate inputs drift out of A/V
   sync). Video and audio are mandatory; HTTP200 waits for payload from both.
   If ffmpeg can't start, the request fails with500; unusable output fails502.
   Overall startup is bounded to120seconds (504) and bootstrap media to2MiB.
4. If ffmpeg stops (including token expiry or a clean upstream EOF), the proxy
   aborts that response. It does **not** start another ffmpeg and concatenate a
   reset clock or replayed media. A new HTTP request resolves a fresh URL.
   This replaces the previous transparent token-refresh behavior: Jellyfin
   recordings can stop on fatal upstream failure; automatic resumption is not
   promised. ffmpeg's upstream read timeout remains15seconds. After startup,
   30seconds without either required track ends the session. Queued media is
   bounded to2MiB; a client blocking output for30seconds has its owned HTTP/1
   connection reset and child reaped. Disconnect/shutdown cancels startup and
   streaming without affecting other clients.

`HEAD /tvp/<id>.ts` returns metadata for a syntactically valid numeric route
without resolving the channel, taking a slot or starting ffmpeg. It does not
claim the channel is currently playable or advertise a zero-length GET.

No transcoding happens in the proxy; CPU use is negligible.

## Why MPEG-TS and not just the HLS URL?

TVP's streams are HLS masters with several video variants and a separate audio
rendition, in fMP4 segments. Jellyfin's DVR records Live TV with a fixed ffmpeg
command without `-map`, which on such sources can drop the video or pick the
wrong audio ([jellyfin#18234](https://github.com/jellyfin/jellyfin/issues/18234)).
A single pre-muxed MPEG-TS stream with exactly one video and one audio track
avoids that, and the URL never changes, so it can go into a static M3U.

## Requirements

- Docker with Compose v2.
- A WireGuard server **in Poland** you control (home router, VPS, …) and a peer
  configured for this stack.

## Setup

```sh
git clone https://github.com/mkowalski/tvp-vod-proxy-rs.git
cd tvp-vod-proxy-rs

# 1. WireGuard client config
mkdir -p wireguard/wg_confs
( umask 077; wg genkey | tee wireguard/privatekey | wg pubkey > wireguard/publickey )
cp examples/wg0.conf.example wireguard/wg_confs/wg0.conf
chmod 600 wireguard/wg_confs/wg0.conf
#   - put the private key into wg0.conf
#   - add wireguard/publickey as a peer on your server in Poland
#   - fill in Address, the server's PublicKey and Endpoint

# 2. Start (pulls ghcr.io/mkowalski/tvp-vod-proxy-rs)
docker compose up -d
docker compose ps          # tvp-pl-wg must become "healthy" (exit country = PL)

# 3. Test
curl -s -o /dev/null -w '%{http_code}\n' --max-time 10 http://localhost:38099/tvp/399700.ts
```

The sample compose publishes the proxy on all of the Docker host's interfaces,
so Jellyfin on another machine or in another container can reach it.
Restricting it to the host's LAN address in `docker-compose.yml` (e.g.
`"192.0.2.10:38099:8080"`) is recommended. Never publish the proxy on a public
interface.

Images are published for `linux/amd64` and `linux/arm64` as
`ghcr.io/mkowalski/tvp-vod-proxy-rs:<version>` and `:latest`.

**Pin a version rather than `:latest`.** Restarting the proxy cuts every
stream going through it, including DVR recordings in progress, and Jellyfin
does not reconnect. The sample compose pins a version and opts out of
Watchtower; update by changing the tag when nothing is recording.

**Never let anything recreate `wg` on its own.** The proxy runs inside the
`wg` container's network namespace. If `wg` is recreated by something other
than `docker compose up` (Watchtower, `docker compose up --no-deps wg`), the
proxy is left behind in the old, dead namespace: it keeps running and its
loopback healthcheck keeps passing, but port 38099 now leads to the new
namespace, where nothing listens, and clients get "connection refused". The
sample compose therefore pins `wg` and opts it out of Watchtower as well.
Update both with `docker compose pull && docker compose up -d` when nothing
is recording; it recreates `wg` and the proxy together.

### Generate the channel list

```sh
docker exec tvp-pl-proxy tvp-vod-proxy make-m3u 192.0.2.10:38099 > tvp.m3u
```

Replace `192.0.2.10` with the address your IPTV client uses to reach the
Docker host. Paid channels and any channel that fails to resolve (DRM, no HLS
source, upstream error) are skipped and logged to stderr. Channel IDs come from
`https://vod.tvp.pl/api/products/lives` (e.g. TVP Kultura = `399700`).

### Jellyfin

1. Put `tvp.m3u` where Jellyfin can read it.
2. Dashboard → Live TV → **Add tuner device** → M3U Tuner → path to `tvp.m3u`.
3. Map channels to your XMLTV guide and run **Refresh Guide**.

**Samsung (Tizen) TVs and other native players:** Direct Play of an endless
live MPEG-TS can hang on the loading screen. In the TV app, enable
*Settings → Playback → Force transcoding of remote media sources such as Live
TV* so Jellyfin serves HLS instead. With hardware transcoding (Intel QSV,
NVENC, …) this is cheap.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `MAX_BITRATE` | `0` (no limit) | Highest average variant bitrate to select, bit/s. `0` → always the top variant (1080p50, ~6.8 Mbit/s today); `4000000` → 576p. If no variant fits, the lowest is used. |
| `MAX_STREAMS` | `10` | Maximum concurrent streams; further requests get HTTP 503. `0` → no limit. |
| `BIND` | `0.0.0.0` | Listen address. Keep the default in Docker and restrict the published port in `docker-compose.yml` instead. |
| `PORT` | `8080` | Listen port inside the container. |
| `FFMPEG` | `ffmpeg` | ffmpeg binary. |
| `TVP_API` | `https://vod.tvp.pl` | API base URL (a path prefix is kept). |
| `RUST_LOG` | `info` | Log filter, e.g. `debug`. |

`GET /healthz` returns `ok` while the proxy is running; the image's
`HEALTHCHECK` uses it. The sample compose file additionally checks that TVP is
reachable through the tunnel.

Live HLS arrives in real time, so the selected variant's average bitrate must
fit in the sustained throughput from Poland. If playback stutters, set
`MAX_BITRATE` below your tunnel's throughput.

## Metrics

`GET /metrics` exposes Prometheus text metrics on the proxy's existing port.
With the sample Compose port mapping:

```sh
curl http://localhost:38099/metrics
```

Collection is entirely within the proxy. Scraping reads counters and gauges;
it does not contact TVP or start a stream. Counters and histograms accumulate
since startup and reset when the proxy restarts. `tvp_process_start_time_seconds`
identifies a new run.

### Telegraf

Add the input from [examples/telegraf.conf](examples/telegraf.conf) to your
Telegraf configuration and replace the example address with the proxy's Docker
host address as reachable **from Telegraf**:

```toml
[[inputs.prometheus]]
  urls = ["http://192.0.2.10:38099/metrics"]
  interval = "10s"
  metric_version = 2
  name_override = "tvp_proxy"
```

Use your existing InfluxDB output configuration. `metric_version = 2` stores
metric names as fields and labels as tags in the `tvp_proxy` measurement.
For availability, alert on missing `tvp_process_uptime_seconds` samples: an
unreachable proxy produces no new samples. See the
[Telegraf Prometheus input documentation](https://docs.influxdata.com/telegraf/v1/input-plugins/prometheus/).

### Metric reference

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `tvp_stream_requests_total` | Counter | `outcome` | Valid channel requests classified at startup: `started`, `drm`, `resolve_error`, `capacity`, `playlist_error`, `ffmpeg_error`, `cancelled`. |
| `tvp_stream_requests_in_flight` | Gauge | — | Requests still resolving or starting ffmpeg. |
| `tvp_streams_active` | Gauge | `channel` | Admitted connections including A/V bootstrap and bounded output draining. |
| `tvp_stream_seconds_total` | Counter | `channel` | Summed connection duration, including ongoing streams and bootstrap. Two simultaneous one-hour streams contribute 7,200 seconds. |
| `tvp_stream_bytes_total` | Counter | `channel` | MPEG-TS payload bytes handed to the HTTP layer. |
| `tvp_stream_ends_total` | Counter | `reason` | Ended connections: `client_disconnect`, `shutdown`, `upstream_exit`, `media_timeout`, `downstream_timeout`, `cancelled`. Legacy `quick_failures`/`restart_error` remain zero. |
| `tvp_ffmpeg_processes_active` | Gauge | `channel` | ffmpeg children currently owned by the proxy. |
| `tvp_ffmpeg_restarts_total` | Counter | — | Legacy compatibility series, always zero: established streams are not restarted. |
| `tvp_resolve_total` | Counter | `phase`, `outcome` | Resolution attempts including failures/cancellation. `initial` is active; legacy `retry` remains zero. |
| `tvp_resolves_in_flight` | Gauge | `phase` | Resolutions currently pending. |
| `tvp_resolve_duration_seconds` | Histogram | `phase` | Resolution latency, including failures/cancellation and excluding retry sleep. |
| `tvp_stream_startup_seconds` | Histogram | — | Time from request arrival to the first nonempty HTTP body chunk; observed once, only if output appears. |
| `tvp_stream_limit` | Gauge | — | Effective stream capacity; `0` means unlimited. |
| `tvp_stream_slots_in_use` | Gauge | — | Occupied capacity, including ffmpeg startup before a connection becomes active. |
| `tvp_process_start_time_seconds` | Gauge | — | Server initialization time as Unix seconds. |
| `tvp_process_uptime_seconds` | Gauge | — | Seconds since server initialization. |
| `tvp_build_info` | Gauge | `version` | Package version, with a value of `1`. |

Resolution outcomes are `ok`, `drm`, `no_source`, `no_variant`, `json`,
`http_403`, `http_error` (other failing HTTP statuses), `timeout`, `connect`,
`upstream` (other transport errors), `url`, `too_large`, and `cancelled`.
An initial resolution happens before checking capacity, so a request rejected
with 503 still contributes a resolution attempt.

Histograms export cumulative `_bucket`, `_sum` and `_count` series. Bucket
upper bounds, in seconds, are `0.1`, `0.25`, `0.5`, `1`, `2.5`, `5`, `10`,
`20`, `40`, `80`, and `+Inf`; bucket series add the `le` label. Use bucket
increments over the desired time window to calculate latency percentiles.

Fixed outcome counters and histograms are emitted at zero from startup, so
short connections between scrapes are counted. Per-channel series appear when
a resolved channel is admitted and remain present, including zero active
connections. There are at most 128 distinct channel IDs plus `channel="other"`,
which accumulates additional IDs. Requests that fail resolution or find capacity
full do not allocate channel series. Labels contain IDs and fixed categories;
client addresses, user agents and upstream URLs are not exported.

For charts, use reset-aware counter increases/rates: byte rate × 8 gives output
bit/s, and an increase in streaming seconds ÷ 3,600 gives connection-hours.
The first sample of a new per-channel series establishes its baseline.
Connections include player probes, viewing and recordings; they are not a count
of people. HTTP200 means both required MPEG-TS payload PIDs were observed, not
that codecs decoded or A/V synchronization is correct. Output bytes do not prove
client receipt or playback or measure WireGuard traffic. Startup timeouts count
as cancelled pending requests; missing-track bootstrap failures as `ffmpeg_error`.

## Networking notes

- `AllowedIPs = 0.0.0.0/0` applies only inside the stack's namespace.
- The `PostUp` line in `wg0.conf` routes RFC 1918 ranges back via the Docker
  bridge gateway, so replies to LAN clients and LAN DNS don't enter the tunnel.
  It must match the `gateway` in `docker-compose.yml`.
- The `wg` healthcheck fails if the exit country isn't `PL`; `proxy` only
  starts once it is healthy.
- `proxy` shares `wg`'s network namespace (`network_mode: service:wg`), so
  `wg` must never be recreated without the proxy (see above). `depends_on`
  sets `restart: true` so Compose restarts the proxy whenever it recreates
  `wg`. Note that current Compose (v5) recreates the proxy on every
  `docker compose up -d` in this directory, even with no changes: its config
  hash includes the `wg` container ID.

## Development

```sh
cargo test --locked               # unit + integration tests (streaming ones need ffmpeg/ffprobe on PATH)
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --all --check
cargo run -- serve                # needs a Polish IP to reach TVP
```

The integration tests generate a small TVP-like HLS stream (two fMP4 video
variants and a separate audio rendition) with ffmpeg, serve it from a mock
TVP API/CDN, and check the proxy's output: one video + one audio track,
variant selection, DRM → 415, API errors → 502, and that ffmpeg is stopped
(and not restarted) when the client disconnects, even while ffmpeg is stalled.
Real media regressions require exactly one six-second pass, monotonic DTS on
both tracks and a full strict decode, rather than accepting replayed passes.
Synthetic transport-packet subprocesses exercise no-respawn on exit, bootstrap
deadlines/missing tracks, one-track silence, private playlists, capacity,
disconnect/shutdown and unread-client resets without draining their sockets.
They test lifecycle/activity only, not codec validity.
No test talks to the real TVP. Without ffmpeg/ffprobe the streaming tests are
skipped, unless `CI` is set, in which case they fail.

CI runs formatting, clippy, tests, `cargo-deny` and a Docker build on pull
requests and pushes to `main`. Pushing a `v*` tag publishes a multi-arch image to GHCR.

## Limitations

- A few channels use FairPlay DRM; they can't be proxied and are skipped.
- Paid channels (TVP HD, TVP Seriale) require a subscription and are skipped.
- One ffmpeg process and one upstream connection per active viewer/recording;
  all of it goes through your Polish uplink (~7 Mbit/s per 1080p channel).
- TVP may change its API at any time.
- Native regression checks do not certify a Linux image or actual Jellyfin DVR
  artifacts. This source change has not been deployed; no live server, WG, TVP
  or Jellyfin restart was performed while recording safety is active.

See the [recording-safety checkpoint](docs/acceptance/2026-10-25-recording-safety.md)
for verified regressions, review results and pending deployment/DVR gates.

## Legal notice

This section explains how the project relates to Polish law. It is a summary
of the statutes linked below, not legal advice. You are responsible for how you
use the software.

### What the software does

- It plays **free-to-air** live channels that Telewizja Polska S.A. makes
  available to everyone on vod.tvp.pl without an account or payment.
- It connects to TVP **from a Polish internet connection that you operate**
  (your own WireGuard server in Poland). It does not use third-party mirrors or
  unofficial restreams; every stream comes directly from TVP's servers.
- It is for **private viewing and recording** by you and your household. The
  proxy serves your own media server; it is not a public service and must not be
  exposed to the internet.

### What it does not do

- **No DRM circumvention.** Channels protected by encryption (e.g. FairPlay)
  are detected and refused with HTTP 415. The software never decrypts, removes
  or alters any protection.
- **No paid content.** Channels that require a TVP subscription are skipped.
- **No redistribution.** It does not rebroadcast (*reemitowanie*) or make any
  programme available to the public.
- **No modification** of the programme: video and audio are copied bit-for-bit
  (`ffmpeg -c copy`) into a different container.

### Relevant provisions

**Personal use** — [Art. 23 Copyright Act][art23] (*ustawa o prawie autorskim i
prawach pokrewnych*): a work that has already been disseminated may be used free
of charge, without the author's permission, for one's own personal use,
including by people in a personal relationship (family, friends). TVP's free
live channels are disseminated works.
[Art. 100][art100] applies the same permitted use to related rights, which
covers the broadcaster's own right in its broadcasts ([Art. 97][art97]).
Permitted use must not interfere with the normal exploitation of the work or
harm the rightholder's legitimate interests ([Art. 35][art35]); private viewing
of a free channel, from TVP's own servers, does not deprive TVP of any
revenue.

**Technical protection measures** — [Art. 6 ust. 1 pkt 11][art6] defines
*effective* technical protection as control over use of a work through an
access code or protection mechanism, **in particular encryption, scrambling or
other transformation of the work, or a copy-control mechanism**. Liability under
[Art. 79 ust. 6][art79] and [Art. 118¹][art1181] concerns removing or
circumventing such measures. This software does not break encryption,
scrambling or copy control, and refuses encrypted channels.

**Access to information** — [Art. 267 Criminal Code][art267] penalises obtaining
*information not intended for the person* by breaking or bypassing a special
safeguard. The free channels are intended for the public, and the software
reaches them through an ordinary connection that genuinely originates in
Poland; it does not break into any system or bypass authentication.

**Rebroadcasting** — rebroadcasting a programme to the public
(*reemitowanie*, [Art. 6 ust. 1 pkt 5][art6]) is the broadcaster's exclusive
right ([Art. 97 pkt 4][art97]). This software only delivers a stream to the
person who requested it, on their own network; it is not a rebroadcast.

### Your responsibilities

- Use it only for private viewing within your household.
- Keep the proxy on your private network; do not publish it, share its URLs or
  run it as a service for others.
- Do not sell, publicly show or upload recordings.
- If you live outside Poland, local law also applies to you.

TVP and vod.tvp.pl are trademarks of Telewizja Polska S.A. This project is not
affiliated with or endorsed by TVP.

[art6]: https://lexlege.pl/ustawa-o-prawie-autorskim-i-prawach-pokrewnych/art-6/
[art23]: https://lexlege.pl/ustawa-o-prawie-autorskim-i-prawach-pokrewnych/art-23/
[art35]: https://lexlege.pl/ustawa-o-prawie-autorskim-i-prawach-pokrewnych/art-35/
[art79]: https://lexlege.pl/ustawa-o-prawie-autorskim-i-prawach-pokrewnych/art-79/
[art97]: https://lexlege.pl/ustawa-o-prawie-autorskim-i-prawach-pokrewnych/art-97/
[art100]: https://lexlege.pl/ustawa-o-prawie-autorskim-i-prawach-pokrewnych/art-100/
[art1181]: https://lexlege.pl/ustawa-o-prawie-autorskim-i-prawach-pokrewnych/art-118-1/
[art267]: https://lexlege.pl/kk/art-267/

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

The Docker image also contains [FFmpeg](https://ffmpeg.org) from Alpine Linux,
licensed under GPL-2.0-or-later. It runs as a separate program and is not
linked into the proxy. Its source is available from
[Alpine's aports](https://gitlab.alpinelinux.org/alpine/aports/-/tree/master/community/ffmpeg).
