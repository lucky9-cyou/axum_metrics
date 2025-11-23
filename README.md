# Modern Observability Lib (Rust 2025 / Axum 0.8)

A production-grade, zero-overhead observability library for Axum 0.8 applications. It combines **Metrics**, **Tracing**, and **Error Tracking** into a unified, elegant API.

## 🌟 Features

*   **Auto-Detected Traffic:** Automatically distinguishes between HTTP, SSE, and WebSockets.
*   **Cardinality Protection:** Safe route extraction prevents memory explosions from dynamic paths.
*   **Log Pollution Free:** Tracks business errors in Prometheus without spamming your console/Sentry logs.
*   **Async Context Safety:** Preserves Trace IDs and Service Labels across `tokio::spawn`.
*   **Accurate Streaming Metrics:** Tracks the *actual* duration of long-lived connections (SSE/WS), not just the handshake.

## 📦 Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
axum = { version = "0.8.4", features = ["macros", "ws"] }
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
observability_lib = { path = "./path/to/lib" } # Or git dependency
```

## 🚀 Quick Start

### 1. Initialize (Main)
One line to setup Tracing and Prometheus.

```rust
#[tokio::main]
async fn main() {
    let handle = observability_lib::setup_observability()
        .expect("Init failed");

    let app = Router::new()
        .route("/", get(handler))
        .route("/metrics", get(move || std::future::ready(handle.render())))
        .layer(axum::middleware::from_fn(observability_lib::traffic_layer));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

### 2. Instrument Business Logic
Use standard `tracing` attributes. Service names are inherited by child functions.

```rust
use observability_lib::{spawn_monitored, Trackable};

#[tracing::instrument(fields(service = "payment"), ret)]
async fn handler() {
    // 1. Track Errors silently
    let _ = risky_op().await.track(); 

    // 2. Spawn Async Task (Preserves "payment" service label)
    spawn_monitored(async {
        let _ = send_email().await;
    });
}
```

### 3. Track WebSockets
Use `WsTracker` inside the upgrade handler to track session duration.

```rust
use observability_lib::WsTracker;

ws.on_upgrade(|socket| async move {
    let _tracker = WsTracker::new("chat_service");
    // Gauge active=1 until this block ends
    handle_socket(socket).await;
});
```

## 📊 Exported Metrics

| Metric Name | Type | Description |
| :--- | :--- | :--- |
| `http_requests_total` | Counter | Count of requests (labeled by route, status, type) |
| `http_requests_active` | Gauge | Currently active connections (critical for SSE/WS) |
| `http_request_duration_seconds` | Histogram | Latency of HTTP headers / Standard requests |
| `function_duration_seconds` | Histogram | Latency of internal functions annotated with `#[instrument]` |
| `function_errors_total` | Counter | Count of errors caught via `.track()` |

## ⚠️ Critical Configuration

This library uses a "Secret Channel" (`target: "metrics::signal"`) to send error events to Prometheus without logging them to stdout.

If you add your own `EnvFilter`, ensure you **DO NOT** filter out this target.

**Correct:**
```rust
RUST_LOG="info,metrics::signal=debug"
```
*(Note: The library handles this automatically in `setup_observability`, but be careful if you override it manually).*
