# TVP versioned releases

Prepared next version: **0.0.5**. See `docs/releases/0.0.5.md` for the intentional
recording-safety behavior change: no established-response respawn/timeline splice.
Source/release publication and production deployment are separate decisions.

## Gates and publication

1. Commit/review the matching Cargo.toml/Cargo.lock version, helpers and notes;
   push/merge to `main` only when authorized. Wait for that exact main-push CI
   run to succeed, including native amd64/arm64 tests and image smoke. A running,
   failed, cancelled, PR, unrelated-SHA or branch-only run does not authorize
   release promotion.
2. After explicit publication approval, push a matching `v0.0.5` tag. The workflow
   resolves annotated tags to the checked-out commit and freezes later jobs to
   that verified SHA. It refuses mismatched versions, missing notes, an existing
   GitHub release or unverified CI.
3. Build platform digests on native Linux amd64/arm64 runners. Pull back each
   exact digest and validate native architecture, unprivileged UID10001, real
   H.264/stereo AAC generation and strict decode, and metadata-only HTTP behavior
   with `TVP_API` set to loopback port9. No live provider request is part of smoke.
4. Only after both jobs pass, promote the tested digests into the versioned GHCR
   manifest at `ghcr.io/mkowalski/tvp-vod-proxy-rs`. Preserve and require BuildKit
   attestations for both runtime manifests. Attach per-platform image/FFmpeg/APK
   evidence to the GitHub release; this is not actual Jellyfin DVR certification.

Stable versions update container `latest` and the latest stable GitHub release.
Any future prerelease is explicitly marked prerelease and never updates either
stable `latest` pointer. No source/package visibility change is part of this work.
CI uses its scoped built-in `GITHUB_TOKEN`, never a retrieved personal token.

Do not force-move tags or overwrite an existing GitHub release. A failed run may
leave unpromoted digests or a partial version manifest; inspect before retrying.
Do not infer actual native image execution or publication success from YAML and
local policy tests alone. Each job has a bounded timeout; no workflow contains a
production deployment/restart command. Deploy by a separately verified image
digest in an operator-approved safe window, preserving WG/Jellyfin and rollback.

Known nonblocking evidence-retention limitation: attached smoke JSON records a
local image configuration digest, but not the registry/source/combined-manifest
identities. Per-platform digest artifacts last1day; after that, identifying the
original published manifests also needs registry state or workflow logs. Fuller
immutable release evidence is deferred; exact-digest execution is still required.

## Local checks

```sh
CI=true PATH="/opt/homebrew/opt/ffmpeg-full/bin:$PATH" cargo test --locked
python3 -m unittest discover -s tests/release -v
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
```

Use full HLS-capable FFmpeg/ffprobe; missing media tooling fails instead of
silently skipping tests. The existing77 Rust regressions remain in force.
Native64-bit ARM execution and the proposed version's CI/publication remain
pending until the reviewed source is pushed and the release is explicitly tagged.
