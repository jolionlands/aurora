use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::path::PathBuf;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Metrics store
// ---------------------------------------------------------------------------

/// Global metrics collection.  All fields are thread-safe.
pub struct Metrics {
    /// Total wallpaper swaps since daemon start.
    pub swaps_total: AtomicU64,

    /// Decode latency histogram buckets (ms): [<10, <50, <100, <200, <500, <1000, >=1000]
    pub decode_ms_buckets: [AtomicU64; 7],
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
            decode_ms_buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
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
        let bucket = match ms {
            0..=9 => 0,
            10..=49 => 1,
            50..=99 => 2,
            100..=199 => 3,
            200..=499 => 4,
            500..=999 => 5,
            _ => 6,
        };
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
            1.0
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
        out.push_str(
            "# HELP aurora_cache_hit_ratio Decode cache hit ratio (0..1)\n",
        );
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
        out.push_str(
            "# HELP aurora_decode_ms_bucket Decode latency histogram in milliseconds\n",
        );
        out.push_str("# TYPE aurora_decode_ms_bucket histogram\n");
        let bounds = [10u64, 50, 100, 200, 500, 1000];
        let mut cumulative = 0u64;
        for (i, le) in bounds.iter().enumerate() {
            cumulative += self.decode_ms_buckets[i].load(Ordering::Relaxed);
            out.push_str(&format!(
                "aurora_decode_ms_bucket{{le=\"{le}\"}} {cumulative}\n"
            ));
        }
        // +Inf bucket
        cumulative += self.decode_ms_buckets[6].load(Ordering::Relaxed);
        out.push_str(&format!(
            "aurora_decode_ms_bucket{{le=\"+Inf\"}} {cumulative}\n"
        ));
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
                let path_str = path.to_string_lossy().replace('\\', "/");
                out.push_str(&format!(
                    "aurora_current_photo{{monitor=\"{monitor}\",path=\"{path_str}\"}} 1\n"
                ));
            }
        }

        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Arc::try_unwrap(Self::new()).unwrap_or_else(|_| panic!("arc unwrap failed"))
    }
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

    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("metrics accept error: {e}");
                continue;
            }
        };

        let m = Arc::clone(&metrics);
        tokio::spawn(async move {
            if let Err(e) = handle_request(&mut stream, &m).await {
                tracing::debug!("metrics request from {peer} error: {e}");
            }
        });
    }
}

async fn handle_request(
    stream: &mut tokio::net::TcpStream,
    metrics: &Arc<Metrics>,
) -> Result<()> {
    let (reader, mut writer) = stream.split();
    let mut buf_reader = BufReader::new(reader);

    // Read request line
    let mut request_line = String::new();
    buf_reader.read_line(&mut request_line).await?;

    // Drain remaining headers
    loop {
        let mut line = String::new();
        buf_reader.read_line(&mut line).await?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }

    let request_line = request_line.trim();

    // Route
    if request_line.starts_with("GET /metrics") {
        let body = metrics.render_prometheus();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        writer.write_all(response.as_bytes()).await?;
    } else if request_line.starts_with("GET /healthz") {
        let body = "ok\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        writer.write_all(response.as_bytes()).await?;
    } else {
        let body = "404 Not Found\n";
        let response = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        writer.write_all(response.as_bytes()).await?;
    }

    Ok(())
}
