//! Bounded, process-local Prometheus counters, gauges and latency histograms.
//! No collection task or history is needed: lifetimes update the counters,
//! and a scrape adds the elapsed time of connections that are still active.

use crate::tvp::ResolveError;
use std::collections::BTreeMap;
use std::fmt::{Display, Write};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::OwnedSemaphorePermit;

const MAX_CHANNELS: usize = 128;
const BUCKETS: [f64; 10] = [0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 40.0, 80.0];
const REQUEST_OUTCOMES: [&str; 7] = [
    "started",
    "drm",
    "resolve_error",
    "capacity",
    "playlist_error",
    "ffmpeg_error",
    "cancelled",
];
const END_REASONS: [&str; 8] = [
    "client_disconnect",
    "shutdown",
    "quick_failures",
    "restart_error",
    "cancelled",
    "downstream_timeout",
    "upstream_exit",
    "media_timeout",
];
const PHASES: [&str; 2] = ["initial", "retry"];
const RESOLVE_OUTCOMES: [&str; 13] = [
    "ok",
    "drm",
    "no_source",
    "no_variant",
    "json",
    "upstream",
    "url",
    "too_large",
    "http_403",
    "http_error",
    "timeout",
    "connect",
    "cancelled",
];

/// A registry belongs to one running server, rather than a process-global
/// singleton, so tests and separately constructed servers remain independent.
pub struct Metrics {
    started: Instant,
    start_time: f64,
    channels: Mutex<BTreeMap<u64, Arc<ChannelMetrics>>>,
    totals: Arc<Totals>,
    other: Arc<ChannelMetrics>,
}

impl Default for Metrics {
    fn default() -> Self {
        let totals = Arc::new(Totals::default());
        Self {
            started: Instant::now(),
            start_time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            channels: Mutex::new(BTreeMap::new()),
            totals: totals.clone(),
            other: Arc::new(ChannelMetrics::new(totals)),
        }
    }
}

impl Metrics {
    pub(crate) fn request(&self) -> RequestGuard {
        self.totals.requests_in_flight.fetch_add(1, Relaxed);
        RequestGuard {
            metrics: self.totals.clone(),
            outcome: RequestOutcome::Cancelled,
        }
    }

    pub(crate) fn resolve(&self, phase: ResolvePhase) -> ResolveGuard {
        self.totals.resolves_in_flight[phase as usize].fetch_add(1, Relaxed);
        ResolveGuard {
            metrics: self.totals.clone(),
            phase,
            started: Instant::now(),
            outcome: RESOLVE_OUTCOMES.len() - 1,
        }
    }

    pub(crate) fn channel(&self, id: u64) -> Arc<ChannelMetrics> {
        let mut channels = self.channels.lock().expect("metrics registry poisoned");
        if let Some(metrics) = channels.get(&id) {
            return metrics.clone();
        }
        if channels.len() >= MAX_CHANNELS {
            return self.other.clone();
        }
        let metrics = Arc::new(ChannelMetrics::new(self.totals.clone()));
        channels.insert(id, metrics.clone());
        metrics
    }

    /// Prometheus text 0.0.4. `limit` is the configured capacity (0 means
    /// unlimited); `in_use` comes from the server's semaphore, including starts.
    /// Encoding does not hold the registry lock or perform any external I/O.
    pub fn render(&self, limit: usize, in_use: usize) -> String {
        let mut channels: Vec<_> = self
            .channels
            .lock()
            .expect("metrics registry poisoned")
            .iter()
            .map(|(id, metrics)| (id.to_string(), metrics.clone()))
            .collect();
        channels.push(("other".into(), self.other.clone()));
        let totals = &self.totals;
        let mut out = String::new();
        for (name, help, value) in [
            (
                "tvp_process_start_time_seconds",
                "Server start time in Unix seconds.",
                self.start_time,
            ),
            (
                "tvp_process_uptime_seconds",
                "Server uptime in seconds.",
                self.started.elapsed().as_secs_f64(),
            ),
            (
                "tvp_stream_limit",
                "Configured stream capacity; zero means unlimited.",
                limit as f64,
            ),
            (
                "tvp_stream_slots_in_use",
                "Occupied stream slots, including ffmpeg startup.",
                in_use as f64,
            ),
        ] {
            header(&mut out, name, "gauge", help);
            sample(&mut out, name, "", value);
        }
        header(
            &mut out,
            "tvp_build_info",
            "gauge",
            "Proxy package version.",
        );
        sample(
            &mut out,
            "tvp_build_info",
            &format!("version=\"{}\"", env!("CARGO_PKG_VERSION")),
            1,
        );

        header(
            &mut out,
            "tvp_stream_requests_total",
            "counter",
            "Channel requests completed by startup outcome.",
        );
        for (i, outcome) in REQUEST_OUTCOMES.iter().enumerate() {
            sample(
                &mut out,
                "tvp_stream_requests_total",
                &format!("outcome=\"{outcome}\""),
                totals.requests[i].load(Relaxed),
            );
        }
        header(
            &mut out,
            "tvp_stream_requests_in_flight",
            "gauge",
            "Requests still resolving or starting ffmpeg.",
        );
        sample(
            &mut out,
            "tvp_stream_requests_in_flight",
            "",
            totals.requests_in_flight.load(Relaxed),
        );
        header(
            &mut out,
            "tvp_streams_active",
            "gauge",
            "Admitted stream connections, including bootstrap and output draining.",
        );
        for (id, c) in &channels {
            let active = c.activity.lock().expect("metrics activity poisoned").active;
            sample(
                &mut out,
                "tvp_streams_active",
                &format!("channel=\"{id}\""),
                active,
            );
        }
        header(
            &mut out,
            "tvp_stream_seconds_total",
            "counter",
            "Cumulative connection seconds, including ongoing streams and bootstrap.",
        );
        for (id, c) in &channels {
            let seconds = c
                .activity
                .lock()
                .expect("metrics activity poisoned")
                .seconds(Instant::now());
            sample(
                &mut out,
                "tvp_stream_seconds_total",
                &format!("channel=\"{id}\""),
                seconds,
            );
        }
        atomic_family(
            &mut out,
            &channels,
            "tvp_stream_bytes_total",
            "counter",
            "MPEG-TS payload bytes handed to HTTP; not acknowledged client delivery.",
            |c| &c.bytes,
        );
        header(
            &mut out,
            "tvp_stream_ends_total",
            "counter",
            "Stream connections ended by reason.",
        );
        for (i, reason) in END_REASONS.iter().enumerate() {
            sample(
                &mut out,
                "tvp_stream_ends_total",
                &format!("reason=\"{reason}\""),
                totals.ends[i].load(Relaxed),
            );
        }
        atomic_family(
            &mut out,
            &channels,
            "tvp_ffmpeg_processes_active",
            "gauge",
            "ffmpeg children currently owned by the proxy.",
            |c| &c.processes,
        );
        header(
            &mut out,
            "tvp_ffmpeg_restarts_total",
            "counter",
            "Successfully started replacement ffmpeg children.",
        );
        sample(
            &mut out,
            "tvp_ffmpeg_restarts_total",
            "",
            totals.restarts.load(Relaxed),
        );
        header(
            &mut out,
            "tvp_resolve_total",
            "counter",
            "Channel resolution attempts completed by phase and outcome.",
        );
        for (phase, name) in PHASES.iter().enumerate() {
            for (i, outcome) in RESOLVE_OUTCOMES.iter().enumerate() {
                sample(
                    &mut out,
                    "tvp_resolve_total",
                    &format!("phase=\"{name}\",outcome=\"{outcome}\""),
                    totals.resolves[phase][i].load(Relaxed),
                );
            }
        }
        header(
            &mut out,
            "tvp_resolves_in_flight",
            "gauge",
            "Channel resolutions currently pending.",
        );
        for (phase, name) in PHASES.iter().enumerate() {
            sample(
                &mut out,
                "tvp_resolves_in_flight",
                &format!("phase=\"{name}\""),
                totals.resolves_in_flight[phase].load(Relaxed),
            );
        }
        header(
            &mut out,
            "tvp_resolve_duration_seconds",
            "histogram",
            "Resolution latency, including failed/cancelled attempts; excludes retry sleep.",
        );
        for (phase, name) in PHASES.iter().enumerate() {
            let histogram = totals.resolve_duration[phase]
                .lock()
                .expect("metrics histogram poisoned")
                .clone();
            histogram.write(
                &mut out,
                "tvp_resolve_duration_seconds",
                &format!("phase=\"{name}\""),
            );
        }
        header(
            &mut out,
            "tvp_stream_startup_seconds",
            "histogram",
            "Request arrival to first nonempty HTTP body chunk; observed once if output appears.",
        );
        let histogram = totals
            .startup
            .lock()
            .expect("metrics histogram poisoned")
            .clone();
        histogram.write(&mut out, "tvp_stream_startup_seconds", "");
        out
    }
}

#[derive(Default)]
struct Totals {
    requests: [AtomicU64; REQUEST_OUTCOMES.len()],
    requests_in_flight: AtomicU64,
    ends: [AtomicU64; END_REASONS.len()],
    restarts: AtomicU64,
    resolves: [[AtomicU64; RESOLVE_OUTCOMES.len()]; PHASES.len()],
    resolves_in_flight: [AtomicU64; PHASES.len()],
    resolve_duration: [Mutex<Histogram>; PHASES.len()],
    startup: Mutex<Histogram>,
}

pub(crate) struct ChannelMetrics {
    totals: Arc<Totals>,
    activity: Mutex<Activity>,
    bytes: AtomicU64,
    processes: AtomicU64,
}

impl ChannelMetrics {
    fn new(totals: Arc<Totals>) -> Self {
        Self {
            totals,
            activity: Mutex::default(),
            bytes: AtomicU64::new(0),
            processes: AtomicU64::new(0),
        }
    }

    pub(crate) fn stream(self: &Arc<Self>, permit: OwnedSemaphorePermit) -> StreamGuard {
        self.activity
            .lock()
            .expect("metrics activity poisoned")
            .start(Instant::now());
        StreamGuard {
            metrics: self.clone(),
            reason: EndReason::Cancelled,
            _permit: permit,
        }
    }

    pub(crate) fn process(self: &Arc<Self>) -> ProcessGuard {
        self.processes.fetch_add(1, Relaxed);
        ProcessGuard(self.clone())
    }

    pub(crate) fn output(&self, bytes: usize, first: Option<Duration>) {
        self.bytes.fetch_add(bytes as u64, Relaxed);
        if let Some(duration) = first {
            self.totals
                .startup
                .lock()
                .expect("metrics histogram poisoned")
                .observe(duration);
        }
    }
}

// Discriminants index the fixed label arrays above. User-provided strings are
// never used as labels; numeric IDs have a process-lifetime cardinality cap.
#[derive(Clone, Copy)]
pub(crate) enum RequestOutcome {
    Started,
    Drm,
    ResolveError,
    Capacity,
    PlaylistError,
    FfmpegError,
    Cancelled,
}

pub(crate) struct RequestGuard {
    metrics: Arc<Totals>,
    outcome: RequestOutcome,
}

impl RequestGuard {
    pub(crate) fn finish(mut self, outcome: RequestOutcome) {
        self.outcome = outcome;
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.metrics.requests[self.outcome as usize].fetch_add(1, Relaxed);
        self.metrics.requests_in_flight.fetch_sub(1, Relaxed);
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ResolvePhase {
    Initial,
}

pub(crate) struct ResolveGuard {
    metrics: Arc<Totals>,
    phase: ResolvePhase,
    started: Instant,
    outcome: usize,
}

impl ResolveGuard {
    pub(crate) fn finish<T>(mut self, result: &Result<T, ResolveError>) {
        self.outcome = match result {
            Ok(_) => 0,
            Err(ResolveError::Drm) => 1,
            Err(ResolveError::NoSource) => 2,
            Err(ResolveError::NoVariant) => 3,
            Err(ResolveError::Json(_)) => 4,
            Err(ResolveError::Upstream(e)) => {
                if e.is_timeout() {
                    10
                } else if e.is_connect() {
                    11
                } else if e.status().is_some_and(|status| status.as_u16() == 403) {
                    8
                } else if e.status().is_some() {
                    9
                } else {
                    5
                }
            }
            Err(ResolveError::Url(_)) => 6,
            Err(ResolveError::TooLarge) => 7,
        };
    }
}

impl Drop for ResolveGuard {
    fn drop(&mut self) {
        let phase = self.phase as usize;
        self.metrics.resolve_duration[phase]
            .lock()
            .expect("metrics histogram poisoned")
            .observe(self.started.elapsed());
        self.metrics.resolves[phase][self.outcome].fetch_add(1, Relaxed);
        self.metrics.resolves_in_flight[phase].fetch_sub(1, Relaxed);
    }
}

#[derive(Clone, Copy)]
pub(crate) enum EndReason {
    ClientDisconnect,
    Shutdown,
    // Preserve zero-valued legacy quick_failures/restart_error series.
    Cancelled = 4,
    DownstreamTimeout,
    UpstreamExit,
    MediaTimeout,
}

impl EndReason {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::ClientDisconnect => "client disconnected",
            Self::Shutdown => "server shutting down",
            Self::Cancelled => "stream task cancelled",
            Self::DownstreamTimeout => "client stopped accepting output",
            Self::UpstreamExit => "upstream media process ended",
            Self::MediaTimeout => "required media track stopped",
        }
    }
}

pub(crate) struct StreamGuard {
    metrics: Arc<ChannelMetrics>,
    reason: EndReason,
    // The slot is released after Drop has finished recording the stream.
    _permit: OwnedSemaphorePermit,
}

impl StreamGuard {
    pub(crate) fn finish(mut self, reason: EndReason) {
        self.reason = reason;
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.metrics
            .activity
            .lock()
            .expect("metrics activity poisoned")
            .end(Instant::now());
        self.metrics.totals.ends[self.reason as usize].fetch_add(1, Relaxed);
    }
}

pub(crate) struct ProcessGuard(Arc<ChannelMetrics>);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.0.processes.fetch_sub(1, Relaxed);
    }
}

struct Activity {
    active: u64,
    elapsed: f64,
    updated: Instant,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            active: 0,
            elapsed: 0.0,
            updated: Instant::now(),
        }
    }
}

impl Activity {
    fn seconds(&self, now: Instant) -> f64 {
        self.elapsed
            + now.saturating_duration_since(self.updated).as_secs_f64() * self.active as f64
    }

    fn start(&mut self, now: Instant) {
        self.elapsed = self.seconds(now);
        self.updated = now;
        self.active += 1;
    }

    fn end(&mut self, now: Instant) {
        self.elapsed = self.seconds(now);
        self.updated = now;
        self.active -= 1;
    }
}

#[derive(Clone, Default)]
struct Histogram {
    buckets: [u64; BUCKETS.len()],
    count: u64,
    sum: f64,
}

impl Histogram {
    fn observe(&mut self, duration: Duration) {
        let seconds = duration.as_secs_f64();
        self.count += 1;
        self.sum += seconds;
        for (i, bound) in BUCKETS.iter().enumerate() {
            if seconds <= *bound {
                self.buckets[i] += 1;
            }
        }
    }

    fn write(&self, out: &mut String, name: &str, labels: &str) {
        let prefix = if labels.is_empty() {
            String::new()
        } else {
            format!("{labels},")
        };
        for (bound, count) in BUCKETS.iter().zip(self.buckets) {
            sample(
                out,
                &format!("{name}_bucket"),
                &format!("{prefix}le=\"{bound}\""),
                count,
            );
        }
        sample(
            out,
            &format!("{name}_bucket"),
            &format!("{prefix}le=\"+Inf\""),
            self.count,
        );
        sample(out, &format!("{name}_sum"), labels, self.sum);
        sample(out, &format!("{name}_count"), labels, self.count);
    }
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}").expect("writing to String");
}

fn sample(out: &mut String, name: &str, labels: &str, value: impl Display) {
    if labels.is_empty() {
        writeln!(out, "{name} {value}").expect("writing to String");
    } else {
        writeln!(out, "{name}{{{labels}}} {value}").expect("writing to String");
    }
}

fn atomic_family(
    out: &mut String,
    channels: &[(String, Arc<ChannelMetrics>)],
    name: &str,
    kind: &str,
    help: &str,
    read: fn(&ChannelMetrics) -> &AtomicU64,
) {
    header(out, name, kind, help);
    for (id, channel) in channels {
        sample(
            out,
            name,
            &format!("channel=\"{id}\""),
            read(channel).load(Relaxed),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_metrics_distinguish_timeouts_from_connection_errors() {
        let metrics = Metrics::default();
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path("/slow"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
            .mount(&mock)
            .await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let error = client
            .get(format!("{}/slow", mock.uri()))
            .send()
            .await
            .unwrap_err();
        metrics
            .resolve(ResolvePhase::Initial)
            .finish(&Err::<(), _>(ResolveError::Upstream(error)));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let error = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap_err();
        metrics
            .resolve(ResolvePhase::Initial)
            .finish(&Err::<(), _>(ResolveError::Upstream(error)));

        let text = metrics.render(0, 0);
        assert!(
            text.contains("tvp_resolve_total{phase=\"initial\",outcome=\"timeout\"} 1\n"),
            "{text}"
        );
        assert!(
            text.contains("tvp_resolve_total{phase=\"initial\",outcome=\"connect\"} 1\n"),
            "{text}"
        );
        assert!(text.contains("tvp_resolves_in_flight{phase=\"initial\"} 0\n"));
    }

    #[test]
    fn channel_overflow_retains_all_output_with_bounded_series() {
        let metrics = Metrics::default();
        for id in 1..=140 {
            metrics.channel(id).output(1, None);
        }
        let text = metrics.render(0, 0);
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("tvp_stream_bytes_total{"))
                .count(),
            129
        );
        for id in 1..=128 {
            assert!(text.contains(&format!("tvp_stream_bytes_total{{channel=\"{id}\"}} 1\n")));
        }
        assert!(text.contains("tvp_stream_bytes_total{channel=\"other\"} 12\n"));
        assert!(!text.contains("channel=\"129\""));
        metrics.channel(1).output(3, None);
        assert!(
            metrics
                .render(0, 0)
                .contains("tvp_stream_bytes_total{channel=\"1\"} 4\n")
        );
    }

    #[test]
    fn stream_seconds_include_overlapping_and_ongoing_connections() {
        let start = Instant::now();
        let mut activity = Activity::default();
        activity.start(start);
        activity.start(start + Duration::from_secs(2));
        assert_eq!(activity.seconds(start + Duration::from_secs(3)), 4.0);
        activity.end(start + Duration::from_secs(5));
        assert_eq!(activity.seconds(start + Duration::from_secs(6)), 9.0);
        activity.end(start + Duration::from_secs(7));
        assert_eq!(activity.seconds(start + Duration::from_secs(20)), 10.0);
        assert_eq!(activity.active, 0);
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_include_boundaries() {
        let mut histogram = Histogram::default();
        for seconds in [0.0, 0.5, 100.0] {
            histogram.observe(Duration::from_secs_f64(seconds));
        }
        let mut text = String::new();
        histogram.write(&mut text, "latency_seconds", "channel=\"1\"");
        for sample in [
            "latency_seconds_bucket{channel=\"1\",le=\"0.1\"} 1\n",
            "latency_seconds_bucket{channel=\"1\",le=\"0.25\"} 1\n",
            "latency_seconds_bucket{channel=\"1\",le=\"0.5\"} 2\n",
            "latency_seconds_bucket{channel=\"1\",le=\"80\"} 2\n",
            "latency_seconds_bucket{channel=\"1\",le=\"+Inf\"} 3\n",
            "latency_seconds_sum{channel=\"1\"} 100.5\n",
            "latency_seconds_count{channel=\"1\"} 3\n",
        ] {
            assert!(text.contains(sample), "missing {sample} in {text}");
        }
    }

    #[test]
    fn dropped_guards_record_cancellation_and_release_capacity() {
        let metrics = Metrics::default();
        let channel = metrics.channel(1);
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let request = metrics.request();
        let resolve = metrics.resolve(ResolvePhase::Initial);
        let stream = channel.stream(slots.clone().try_acquire_owned().unwrap());
        let process = channel.process();
        drop((request, resolve, stream, process));

        assert_eq!(slots.available_permits(), 1);
        let text = metrics.render(1, 0);
        for sample in [
            "tvp_stream_requests_total{outcome=\"cancelled\"} 1\n",
            "tvp_stream_requests_in_flight 0\n",
            "tvp_stream_ends_total{reason=\"cancelled\"} 1\n",
            "tvp_streams_active{channel=\"1\"} 0\n",
            "tvp_ffmpeg_processes_active{channel=\"1\"} 0\n",
            "tvp_resolve_total{phase=\"initial\",outcome=\"cancelled\"} 1\n",
            "tvp_resolves_in_flight{phase=\"initial\"} 0\n",
            "tvp_resolve_duration_seconds_count{phase=\"initial\"} 1\n",
        ] {
            assert!(text.contains(sample), "missing {sample} in {text}");
        }
    }
}
