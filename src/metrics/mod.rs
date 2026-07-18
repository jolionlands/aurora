use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::timeout;

const MAX_CONNECTIONS: usize = 32;
const MAX_REQUEST_HEAD_BYTES: usize = 8 * 1024;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(2);
const DECODE_MS_BUCKET_BOUNDS: [u64; 7] = [10, 50, 100, 250, 500, 1000, 2000];

fn decode_ms_bucket_index(ms: u64) -> usize {
    DECODE_MS_BUCKET_BOUNDS.partition_point(|bound| *bound < ms)
}

// ---------------------------------------------------------------------------
// Metrics store
// ---------------------------------------------------------------------------

/// Global metrics collection.  All fields are thread-safe.
pub struct Metrics {
    /// Total wallpaper swaps since daemon start.
    pub swaps_total: AtomicU64,

    /// Non-cumulative decode latency buckets, one per finite bound plus +Inf.
    pub decode_ms_buckets: [AtomicU64; DECODE_MS_BUCKET_BOUNDS.len() + 1],
    pub decode_ms_sum: AtomicU64,
    pub decode_ms_count: AtomicU64,

    /// Cache hit/miss counters for computing ratio.
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,

    /// Current wallpaper per monitor (monitor name → path).
    pub current_photo: Mutex<HashMap<String, PathBuf>>,

    /// Total number of photos in the index.
    pub index_size: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            swaps_total: AtomicU64::new(0),
            decode_ms_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            decode_ms_sum: AtomicU64::new(0),
            decode_ms_count: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            current_photo: Mutex::new(HashMap::new()),
            index_size: AtomicU64::new(0),
        })
    }

    pub fn record_swap(&self) {
        self.swaps_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_decode_ms(&self, ms: u64) {
        let bucket = decode_ms_bucket_index(ms);
        self.decode_ms_buckets[bucket].fetch_add(1, Ordering::Relaxed);
        self.decode_ms_sum.fetch_add(ms, Ordering::Relaxed);
        self.decode_ms_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_current_photo(&self, monitor: &str, path: PathBuf) {
        self.current_photo.lock().insert(monitor.to_string(), path);
    }

    pub fn set_index_size(&self, n: u64) {
        self.index_size.store(n, Ordering::Relaxed);
    }

    /// Compute cache hit ratio in [0.0, 1.0].
    pub fn cache_hit_ratio(&self) -> f64 {
        let hits = self.cache_hits.load(Ordering::Relaxed);
        let misses = self.cache_misses.load(Ordering::Relaxed);
        let total = hits + misses;
        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }

    // -----------------------------------------------------------------------
    // Prometheus serialisation
    // -----------------------------------------------------------------------

    pub fn render_prometheus(&self) -> String {
        let mut out = String::with_capacity(1024);

        // swaps_total
        let swaps = self.swaps_total.load(Ordering::Relaxed);
        out.push_str("# HELP aurora_swaps_total Total number of wallpaper swaps since start\n");
        out.push_str("# TYPE aurora_swaps_total counter\n");
        out.push_str(&format!("aurora_swaps_total {swaps}\n"));

        // cache_hit_ratio
        let ratio = self.cache_hit_ratio();
        out.push_str("# HELP aurora_cache_hit_ratio Decode cache hit ratio (0..1)\n");
        out.push_str("# TYPE aurora_cache_hit_ratio gauge\n");
        out.push_str(&format!("aurora_cache_hit_ratio {ratio:.4}\n"));

        // index_size
        let idx = self.index_size.load(Ordering::Relaxed);
        out.push_str("# HELP aurora_index_size Number of photos in the index\n");
        out.push_str("# TYPE aurora_index_size gauge\n");
        out.push_str(&format!("aurora_index_size {idx}\n"));

        // decode_ms histogram
        let count = self.decode_ms_count.load(Ordering::Relaxed);
        let sum = self.decode_ms_sum.load(Ordering::Relaxed);
        out.push_str("# HELP aurora_decode_ms Decode latency histogram in milliseconds\n");
        out.push_str("# TYPE aurora_decode_ms histogram\n");
        let mut cumulative = 0u64;
        for (i, le) in DECODE_MS_BUCKET_BOUNDS.iter().enumerate() {
            cumulative += self.decode_ms_buckets[i].load(Ordering::Relaxed);
            out.push_str(&format!(
                "aurora_decode_ms_bucket{{le=\"{le}\"}} {}\n",
                cumulative.min(count)
            ));
        }
        out.push_str(&format!("aurora_decode_ms_bucket{{le=\"+Inf\"}} {count}\n"));
        out.push_str(&format!("aurora_decode_ms_sum {sum}\n"));
        out.push_str(&format!("aurora_decode_ms_count {count}\n"));

        // current_photo labels
        let photos = self.current_photo.lock();
        if !photos.is_empty() {
            out.push_str(
                "# HELP aurora_current_photo Current wallpaper path per monitor (info metric)\n",
            );
            out.push_str("# TYPE aurora_current_photo gauge\n");
            for (monitor, path) in photos.iter() {
                let monitor = escape_label_value(monitor);
                let path = escape_label_value(&path.to_string_lossy());
                out.push_str(&format!(
                    "aurora_current_photo{{monitor=\"{monitor}\",path=\"{path}\"}} 1\n"
                ));
            }
        }

        out
    }
}

fn escape_label_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

// ---------------------------------------------------------------------------
// HTTP server
// ---------------------------------------------------------------------------

/// Serve Prometheus `/metrics` and `/healthz` on the given port.
/// Never returns (runs until task is cancelled).
pub async fn serve_metrics(port: u16, metrics: Arc<Metrics>) -> Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("metrics server listening on http://{addr}/metrics");
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    loop {
        let permit = Arc::clone(&slots).acquire_owned().await?;
        let (mut stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("metrics accept error: {e}");
                continue;
            }
        };

        let m = Arc::clone(&metrics);
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle_request(&mut stream, &m).await {
                tracing::debug!("metrics request from {peer} error: {e}");
            }
        });
    }
}

async fn handle_request(stream: &mut tokio::net::TcpStream, metrics: &Arc<Metrics>) -> Result<()> {
    let (reader, mut writer) = stream.split();
    let request_line = timeout(REQUEST_READ_TIMEOUT, read_request_head(reader))
        .await
        .context("metrics request timed out")??;

    match get_request_path(&request_line) {
        Some("/metrics") => {
            let body = metrics.render_prometheus();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            writer.write_all(response.as_bytes()).await?;
        }
        Some("/healthz") => {
            writer
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n")
                .await?;
        }
        _ => {
            writer
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 14\r\nConnection: close\r\n\r\n404 Not Found\n")
                .await?;
        }
    }

    Ok(())
}

fn get_request_path(request_line: &str) -> Option<&str> {
    let mut parts = request_line.split_whitespace();
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("GET"), Some(path), Some("HTTP/1.0" | "HTTP/1.1"), None) => Some(path),
        _ => None,
    }
}

async fn read_request_head<R: AsyncRead + Unpin>(reader: R) -> Result<String> {
    let mut reader = BufReader::new(reader).take((MAX_REQUEST_HEAD_BYTES + 1) as u64);
    let mut request_line = Vec::new();
    let mut total = reader.read_until(b'\n', &mut request_line).await?;
    if total == 0 {
        bail!("empty HTTP request");
    }
    if total > MAX_REQUEST_HEAD_BYTES {
        bail!("HTTP request headers exceed {MAX_REQUEST_HEAD_BYTES} bytes");
    }

    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line).await?;
        if read == 0 {
            bail!("incomplete HTTP request headers");
        }
        total += read;
        if total > MAX_REQUEST_HEAD_BYTES {
            bail!("HTTP request headers exceed {MAX_REQUEST_HEAD_BYTES} bytes");
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }

    Ok(std::str::from_utf8(&request_line)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn cache_hit_ratio_starts_at_zero_and_tracks_observations() {
        let metrics = Metrics::new();
        assert_eq!(metrics.cache_hit_ratio(), 0.0);

        metrics.record_cache_miss();
        metrics.record_cache_hit();
        assert_eq!(metrics.cache_hit_ratio(), 0.5);
    }

    #[test]
    fn decode_histogram_boundaries_and_exposition_follow_prometheus_semantics() {
        let metrics = Metrics::new();
        for (index, bound) in DECODE_MS_BUCKET_BOUNDS.iter().copied().enumerate() {
            assert_eq!(decode_ms_bucket_index(bound), index);
            metrics.record_decode_ms(bound);
        }
        metrics.record_decode_ms(2001);

        let buckets = std::array::from_fn::<_, 8, _>(|index| {
            metrics.decode_ms_buckets[index].load(Ordering::Relaxed)
        });
        assert_eq!(buckets, [1; 8]);

        let rendered = metrics.render_prometheus();
        assert!(
            rendered.contains("# HELP aurora_decode_ms Decode latency histogram in milliseconds\n")
        );
        assert!(rendered.contains("# TYPE aurora_decode_ms histogram\n"));
        assert!(!rendered.contains("# TYPE aurora_decode_ms_bucket histogram"));
        for (index, bound) in DECODE_MS_BUCKET_BOUNDS.iter().enumerate() {
            assert!(rendered.lines().any(|line| {
                line == format!("aurora_decode_ms_bucket{{le=\"{bound}\"}} {}", index + 1)
            }));
        }
        assert!(rendered
            .lines()
            .any(|line| line == "aurora_decode_ms_bucket{le=\"+Inf\"} 8"));
        assert!(rendered
            .lines()
            .any(|line| line == "aurora_decode_ms_count 8"));
        assert!(rendered
            .lines()
            .any(|line| line == "aurora_decode_ms_sum 5911"));
    }

    #[test]
    fn prometheus_label_values_are_escaped() {
        let metrics = Metrics::new();
        metrics.set_current_photo(
            "display\\one\"two\nthree",
            PathBuf::from("C:\\pics\\one\"two\nthree.jpg"),
        );

        assert_eq!(
            escape_label_value("display\\one\"two\nthree"),
            r#"display\\one\"two\nthree"#
        );
        assert!(metrics.render_prometheus().contains(
            r#"aurora_current_photo{monitor="display\\one\"two\nthree",path="C:\\pics\\one\"two\nthree.jpg"} 1"#
        ));
    }

    #[test]
    fn metrics_routes_require_an_exact_get_path() {
        assert_eq!(get_request_path("GET /metrics HTTP/1.1"), Some("/metrics"));
        assert_eq!(get_request_path("GET /healthz HTTP/1.0"), Some("/healthz"));
        assert_ne!(
            get_request_path("GET /metrics-anything HTTP/1.1"),
            Some("/metrics")
        );
        assert_ne!(
            get_request_path("GET /metrics?query HTTP/1.1"),
            Some("/metrics")
        );
        assert_eq!(get_request_path("POST /metrics HTTP/1.1"), None);
    }

    #[tokio::test]
    async fn oversized_request_headers_are_rejected() {
        let request = format!(
            "GET /metrics HTTP/1.1\r\nX-Fill: {}\r\n\r\n",
            "x".repeat(MAX_REQUEST_HEAD_BYTES)
        );
        let error = read_request_head(Cursor::new(request.into_bytes()))
            .await
            .expect_err("oversized headers should fail");

        assert!(error.to_string().contains("exceed"));
    }
}
