use axum::{
    body::Body,
    extract::MatchedPath,
    http::{Request, Response},
    middleware::Next,
};
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use pin_project_lite::pin_project;
use reqwest::header::CONTENT_TYPE;
use std::{
    borrow::Cow,
    future::Future,
    pin::Pin,
    sync::OnceLock,
    task::{Context as TaskContext, Poll},
    time::Instant,
};
use tracing::Instrument;
use tracing::{
    Event, Id, Subscriber,
    field::{Field, Visit},
    span::Attributes,
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

static RECORDER_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
const METRIC_TARGET: &str = "metrics::signal";

struct ConnectionGuard {
    type_label: &'static str,
    service_label: String,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        gauge!("http_requests_active",
            "type" => self.type_label,
            "service" => self.service_label.clone()
        )
        .decrement(1.0);
    }
}

pin_project! {
    pub struct MonitoredBody<B> {
        #[pin]
        inner: B,
        _guard: Option<ConnectionGuard>,
    }
}

impl<B> http_body::Body for MonitoredBody<B>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        match this.inner.poll_frame(cx) {
            Poll::Ready(None) => {
                let _ = this._guard.take();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

pub async fn traffic_layer(req: Request<Body>, next: Next) -> Response<Body> {
    let start = Instant::now();

    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| Cow::Owned(m.as_str().to_owned()))
        .unwrap_or(Cow::Borrowed("unmatched_route"));

    let method = req.method().as_str().to_owned();
    let service = "gateway".to_string();

    let response = next.run(req).await;
    let status = response.status().as_u16().to_string();

    let is_ws_handshake = response.status() == 101;
    let is_sse = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.to_ascii_lowercase().starts_with("text/event-stream"))
        .unwrap_or(false);

    let type_lbl = if is_ws_handshake {
        "websocket"
    } else if is_sse {
        "sse"
    } else {
        "http"
    };

    counter!("http_requests_total",
        "route" => route.clone(),
        "method" => method.clone(),
        "status" => status,
        "type" => type_lbl,
        "service" => service.clone()
    )
    .increment(1);

    if !is_ws_handshake {
        histogram!("http_request_duration_seconds",
            "route" => route,
            "method" => method,
            "type" => type_lbl,
            "service" => service.clone()
        )
        .record(start.elapsed().as_secs_f64());
    }

    if is_ws_handshake {
        response
    } else {
        // increment with the correct type, including "sse"
        gauge!("http_requests_active",
            "type" => type_lbl,
            "service" => service.clone()
        )
        .increment(1.0);

        let (parts, body) = response.into_parts();
        let monitored_body = MonitoredBody {
            inner: body,
            _guard: Some(ConnectionGuard {
                // ensure drop uses the same label
                type_label: if is_sse { "sse" } else { "http" },
                service_label: service,
            }),
        };
        Response::from_parts(parts, Body::new(monitored_body))
    }
}

// ============================================================================
// 4. WEBSOCKET TRACKER
// ============================================================================

pub struct WsTracker {
    service: &'static str,
}

impl WsTracker {
    pub fn new(service: &'static str) -> Self {
        gauge!("http_requests_active", "type" => "websocket_session", "service" => service)
            .increment(1.0);
        Self { service }
    }
}

impl Drop for WsTracker {
    fn drop(&mut self) {
        gauge!("http_requests_active", "type" => "websocket_session", "service" => self.service)
            .decrement(1.0);
    }
}

#[derive(Clone)]
struct ServiceLabel(String);

struct MetricProcessorLayer;

impl<S> Layer<S> for MetricProcessorLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        span.extensions_mut().insert(Instant::now());

        let mut visitor = ServiceExtractor::default();
        attrs.record(&mut visitor);

        let svc = visitor.service.or_else(|| {
            span.parent()
                .and_then(|p| p.extensions().get::<ServiceLabel>().map(|s| s.0.clone()))
        });

        if let Some(s) = svc {
            span.extensions_mut().insert(ServiceLabel(s));
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(start) = span.extensions().get::<Instant>().copied() else {
            return;
        };

        let name = span.name();
        if name == "request" {
            return;
        }

        let service = span
            .extensions()
            .get::<ServiceLabel>()
            .map(|s| s.0.clone())
            .unwrap_or_else(|| "unknown".to_string());

        histogram!("function_duration_seconds",
            "function" => name,
            "service" => service
        )
        .record(start.elapsed().as_secs_f64());
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() != METRIC_TARGET {
            return;
        }

        let Some(curr) = ctx.lookup_current() else {
            return;
        };

        let mut svc: Option<String> = None;
        let mut func: Option<String> = None;

        // Collect the scope from root -> current, then iterate in reverse
        // so we effectively walk current -> root.
        let scope: Vec<_> = curr.scope().from_root().collect();
        for s in scope.into_iter().rev() {
            if svc.is_none() {
                if let Some(sl) = s.extensions().get::<ServiceLabel>() {
                    svc = Some(sl.0.clone());
                }
            }
            if func.is_none() && s.name() != "request" {
                func = Some(s.name().to_string());
            }
            if svc.is_some() && func.is_some() {
                break;
            }
        }

        let service = svc.unwrap_or_else(|| "unknown".to_string());
        let function = func.unwrap_or_else(|| curr.name().to_string());

        counter!("function_errors_total",
            "service" => service,
            "function" => function
        )
        .increment(1);
    }
}

#[derive(Default)]
struct ServiceExtractor {
    service: Option<String>,
}
impl Visit for ServiceExtractor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "service" {
            self.service = Some(format!("{:?}", value));
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "service" {
            self.service = Some(value.to_string());
        }
    }
}

pub trait Trackable<T, E> {
    fn track(self) -> Result<T, E>;
}
impl<T, E> Trackable<T, E> for Result<T, E> {
    fn track(self) -> Result<T, E> {
        if self.is_err() {
            tracing::info!(target: METRIC_TARGET, "err");
        }
        self
    }
}

pub fn spawn_monitored<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let span = tracing::Span::current();
    tokio::spawn(future.instrument(span))
}

pub mod client {
    use super::*;
    use async_trait::async_trait;
    use futures::Stream;
    use reqwest::{Client, Request, Response};
    use reqwest_middleware::{ClientBuilder, ClientWithMiddleware, Middleware, Next, Result};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct ClientMetrics {
        target_service: &'static str,
    }

    #[async_trait]
    impl Middleware for ClientMetrics {
        async fn handle(
            &self,
            req: Request,
            // FIX: Use standard http::Extensions (re-exported by Axum)
            extensions: &mut axum::http::Extensions,
            next: Next<'_>,
        ) -> Result<Response> {
            let start = Instant::now();
            let method = req.method().to_string();

            // Execute
            let res = next.run(req, extensions).await;

            let duration = start.elapsed().as_secs_f64();
            let status = match &res {
                Ok(r) => r.status().as_u16().to_string(),
                Err(_) => "error".to_string(),
            };

            // Record "Time to Headers"
            histogram!("external_request_duration_seconds",
                "target" => self.target_service,
                "method" => method,
                "status" => status
            )
            .record(duration);

            res
        }
    }

    /// Create a client that automatically tracks outbound latency
    pub fn create_client(target_service: &'static str) -> ClientWithMiddleware {
        let client = Client::builder().build().unwrap();
        ClientBuilder::new(client)
            .with(ClientMetrics { target_service })
            .build()
    }

    // --- B. The Stream Wrapper ---

    struct ClientStreamGuard {
        start: Instant,
        service: &'static str,
    }

    impl Drop for ClientStreamGuard {
        fn drop(&mut self) {
            // Record total time from start of stream to finish/drop
            let duration = self.start.elapsed().as_secs_f64();
            histogram!("external_stream_duration_seconds", "service" => self.service)
                .record(duration);
        }
    }

    pin_project! {
        pub struct MonitoredStream<S> {
            #[pin]
            inner: S,
            _guard: Option<ClientStreamGuard>,
        }
    }

    impl<S, I, E> Stream for MonitoredStream<S>
    where
        S: Stream<Item = std::result::Result<I, E>>,
    {
        type Item = std::result::Result<I, E>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.project();
            match this.inner.poll_next(cx) {
                Poll::Ready(None) => {
                    // Stream ended naturally. Drop guard to record metric.
                    let _ = this._guard.take();
                    Poll::Ready(None)
                }
                other => other,
            }
        }
    }

    /// Wrap an external stream (like OpenAI response) to track its full duration
    pub fn track_stream<S, I, E>(stream: S, service_name: &'static str) -> MonitoredStream<S>
    where
        S: Stream<Item = std::result::Result<I, E>>,
    {
        MonitoredStream {
            inner: stream,
            _guard: Some(ClientStreamGuard {
                start: Instant::now(),
                service: service_name,
            }),
        }
    }

    pub trait TrackExternal: Sized {
        fn track_external(self, service: &'static str) -> ExternalRequestFuture<Self>;
    }

    impl<F> TrackExternal for F
    where
        F: Future,
    {
        fn track_external(self, service: &'static str) -> ExternalRequestFuture<Self> {
            ExternalRequestFuture {
                inner: self,
                service,
                start: Instant::now(),
            }
        }
    }

    pin_project! {
        /// A Future that times its execution and records 'external_request_duration_seconds'
        pub struct ExternalRequestFuture<F> {
            #[pin]
            inner: F,
            service: &'static str,
            start: Instant,
        }
    }

    impl<F> Future for ExternalRequestFuture<F>
    where
        F: Future,
    {
        type Output = F::Output;

        fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
            let this = self.project();
            match this.inner.poll(cx) {
                Poll::Ready(result) => {
                    let duration = this.start.elapsed().as_secs_f64();

                    // Record Latency
                    histogram!("external_request_duration_seconds",
                        "target" => *this.service, // Using 'target' label for consistency
                        "method" => "POST",        // Assumption for RPC/LLM calls
                        "status" => "completed"    // We don't inspect inner Result here
                    )
                    .record(duration);

                    Poll::Ready(result)
                }
                Poll::Pending => Poll::Pending,
            }
        }
    }
}

pub fn setup_observability() -> Result<&'static PrometheusHandle, String> {
    let handle = RECORDER_HANDLE.get_or_init(|| {
        let builder = PrometheusBuilder::new()
            .set_buckets_for_metric(
                Matcher::Full("http_request_duration_seconds".to_string()),
                &[0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0],
            )
            .expect("failed to set buckets");

        let handle = builder
            .install_recorder()
            .expect("Failed to install recorder");

        describe_counter!("http_requests_total", "Total HTTP requests");
        describe_gauge!("http_requests_active", "Active connections");
        describe_histogram!("http_request_duration_seconds", "HTTP Latency");
        describe_histogram!("function_duration_seconds", "Internal Latency");
        describe_counter!("function_errors_total", "Internal Errors");
        describe_histogram!("external_request_duration_seconds", "Outbound HTTP Latency");
        describe_histogram!(
            "external_stream_duration_seconds",
            "Outbound Stream Duration"
        );
        describe_histogram!(
            "external_stream_ttft_seconds",
            "Time to first token for outbound streams"
        );

        use tracing_subscriber::filter::{LevelFilter, Targets};
        use tracing_subscriber::prelude::*;

        let log_filter = Targets::new()
            .with_target(METRIC_TARGET, LevelFilter::OFF)
            .with_default(LevelFilter::ERROR);

        let _ = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .compact()
                    .with_filter(log_filter),
            )
            .with(MetricProcessorLayer)
            .try_init();

        handle
    });
    Ok(handle)
}
