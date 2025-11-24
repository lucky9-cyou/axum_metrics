use axum::{
    body::Body,
    extract::{MatchedPath, State},
    http::{Request, Response},
    middleware::Next,
};
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use pin_project_lite::pin_project;
use reqwest::header::CONTENT_TYPE;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
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
const SVC_CTX_SPAN: &str = "svc_ctx";

#[derive(Clone, Debug)]
pub struct ServiceTag(pub String);

#[derive(Clone)]
pub struct ServiceMap {
    default: String,
    rules: Arc<Vec<(Rule, String)>>,
}

#[derive(Clone)]
enum Rule {
    Exact(String),
    Prefix(String),
}

impl ServiceMap {
    pub fn new(default: impl Into<String>) -> Self {
        Self {
            default: default.into(),
            rules: Arc::new(Vec::new()),
        }
    }

    pub fn exact(mut self, path: impl Into<String>, service: impl Into<String>) -> Self {
        let mut rules = (*self.rules).clone();
        rules.push((Rule::Exact(path.into()), service.into()));
        self.rules = Arc::new(rules);
        self
    }

    pub fn prefix(mut self, prefix: impl Into<String>, service: impl Into<String>) -> Self {
        let mut rules = (*self.rules).clone();
        rules.push((Rule::Prefix(prefix.into()), service.into()));
        self.rules = Arc::new(rules);
        self
    }

    fn choose(&self, route: &str) -> String {
        for (rule, svc) in self.rules.iter() {
            match rule {
                Rule::Exact(p) if route == p => return svc.clone(),
                Rule::Prefix(pf) if route.starts_with(pf) => return svc.clone(),
                _ => {}
            }
        }
        self.default.clone()
    }
}

pub async fn tag_service_with_state(
    State(service_map): State<ServiceMap>,
    mut req: Request<Body>,
    next: Next,
) -> Response<Body> {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "unmatched_route".to_string());

    let service = service_map.choose(&route);
    req.extensions_mut().insert(ServiceTag(service.clone()));

    // Create a parent span that carries the service label so function spans inherit it.
    let span = tracing::info_span!(SVC_CTX_SPAN, service = service.as_str());
    next.run(req).instrument(span).await
}

struct ConnectionGuard {
    type_label: &'static str,
    service_label: String,
    route: String,
    method: String,
    start: Instant,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        // Active connections gauge
        gauge!(
            "http_requests_active",
            "type" => self.type_label,
            "service" => self.service_label.clone()
        )
        .decrement(1.0);

        // Full duration from request start until body is fully sent / stream is closed
        histogram!(
            "http_request_duration_seconds_ms",
            "route" => self.route.clone(),
            "method" => self.method.clone(),
            "type" => self.type_label,
            "service" => self.service_label.clone()
        )
        .record(self.start.elapsed().as_millis() as f64);
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
                // When the body stream finishes (including SSE), drop the guard
                let _ = this._guard.take();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

pub async fn traffic_layer(req: Request<Body>, next: Next) -> Response<Body> {
    let start = Instant::now();

    // Use owned String to be able to move into the guard
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "unmatched_route".to_string());

    let method = req.method().as_str().to_owned();

    let service = req
        .extensions()
        .get::<ServiceTag>()
        .map(|s| s.0.clone())
        .unwrap_or_else(|| "gateway".to_string());

    let response = next.run(req).await;
    let status = response.status().as_u16().to_string();

    let is_ws_handshake = response.status() == 101;
    let is_sse = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.to_ascii_lowercase().starts_with("text/event-stream"))
        .unwrap_or(false);

    let type_lbl: &'static str = if is_ws_handshake {
        "websocket"
    } else if is_sse {
        "sse"
    } else {
        "http"
    };

    // Count requests (handshake for websocket, initial response for SSE/HTTP)
    counter!(
        "http_requests_total",
        "route" => route.clone(),
        "method" => method.clone(),
        "status" => status,
        "type" => type_lbl,
        "service" => service.clone()
    )
    .increment(1);

    // For websocket upgrade we don't wrap the body, duration is tracked separately by WsTracker
    if is_ws_handshake {
        return response;
    }

    // For HTTP and SSE we track active connections via a guard that lives as long as the body
    gauge!(
        "http_requests_active",
        "type" => type_lbl,
        "service" => service.clone()
    )
    .increment(1.0);

    let (parts, body) = response.into_parts();
    let monitored_body = MonitoredBody {
        inner: body,
        _guard: Some(ConnectionGuard {
            type_label: if is_sse { "sse" } else { "http" },
            service_label: service,
            route,
            method,
            start,
        }),
    };
    Response::from_parts(parts, Body::new(monitored_body))
}

pub struct WsTracker {
    service: &'static str,
    start: Instant,
}

impl WsTracker {
    pub fn new(service: &'static str) -> Self {
        gauge!(
            "http_requests_active",
            "type" => "websocket_session",
            "service" => service
        )
        .increment(1.0);

        Self {
            service,
            start: Instant::now(),
        }
    }
}

impl Drop for WsTracker {
    fn drop(&mut self) {
        gauge!(
            "http_requests_active",
            "type" => "websocket_session",
            "service" => self.service
        )
        .decrement(1.0);

        // Track full websocket session duration
        histogram!(
            "websocket_session_duration_seconds_ms",
            "service" => self.service
        )
        .record(self.start.elapsed().as_millis() as f64);
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
        if name == "request" || name == SVC_CTX_SPAN {
            return;
        }

        let service = span
            .extensions()
            .get::<ServiceLabel>()
            .map(|s| s.0.clone())
            .unwrap_or_else(|| "unknown".to_string());

        histogram!(
            "function_duration_seconds_ms",
            "function" => name,
            "service" => service
        )
        .record(start.elapsed().as_millis() as f64);
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

        let scope: Vec<_> = curr.scope().from_root().collect();
        for s in scope.into_iter().rev() {
            if svc.is_none() {
                if let Some(sl) = s.extensions().get::<ServiceLabel>() {
                    svc = Some(sl.0.clone());
                }
            }
            if func.is_none() && s.name() != "request" && s.name() != SVC_CTX_SPAN {
                func = Some(s.name().to_string());
            }
            if svc.is_some() && func.is_some() {
                break;
            }
        }

        let service = svc.unwrap_or_else(|| "unknown".to_string());
        let function = func.unwrap_or_else(|| curr.name().to_string());

        counter!(
            "function_errors_total",
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
        target_service: String,
        model_name: String,
    }

    #[async_trait]
    impl Middleware for ClientMetrics {
        async fn handle(
            &self,
            req: Request,
            extensions: &mut axum::http::Extensions,
            next: Next<'_>,
        ) -> Result<Response> {
            let start = Instant::now();
            let method = req.method().to_string();

            let res = next.run(req, extensions).await;

            let duration = start.elapsed().as_millis() as f64;
            let status = match &res {
                Ok(r) => r.status().as_u16().to_string(),
                Err(_) => "error".to_string(),
            };

            histogram!(
                "external_request_duration_seconds_ms",
                "target" => self.target_service.clone(),
                "method" => method,
                "status" => status,
                "model" => self.model_name.clone(),
            )
            .record(duration);

            res
        }
    }

    pub fn create_client(target_service: &str, model_name: &str) -> ClientWithMiddleware {
        let client = Client::builder().build().unwrap();
        ClientBuilder::new(client)
            .with(ClientMetrics {
                target_service: target_service.to_string(),
                model_name: model_name.to_string(),
            })
            .build()
    }

    struct ClientStreamGuard {
        start: Instant,
        service: String,
        model_name: String,
    }

    impl Drop for ClientStreamGuard {
        fn drop(&mut self) {
            let duration = self.start.elapsed().as_millis() as f64;
            histogram!(
                "external_stream_duration_seconds_ms",
                "service" => self.service.clone(),
                "model" => self.model_name.clone()
            )
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
                    let _ = this._guard.take();
                    Poll::Ready(None)
                }
                other => other,
            }
        }
    }

    pub fn track_stream<S, I, E>(
        stream: S,
        service_name: &str,
        model_name: &str,
    ) -> MonitoredStream<S>
    where
        S: Stream<Item = std::result::Result<I, E>>,
    {
        MonitoredStream {
            inner: stream,
            _guard: Some(ClientStreamGuard {
                start: Instant::now(),
                service: service_name.to_string(),
                model_name: model_name.to_string(),
            }),
        }
    }

    pub trait TrackExternal: Sized {
        fn track_external(self, service: &str, model_name: &str) -> ExternalRequestFuture<Self>;
    }

    impl<F> TrackExternal for F
    where
        F: Future,
    {
        fn track_external(self, service: &str, model_name: &str) -> ExternalRequestFuture<Self> {
            ExternalRequestFuture {
                inner: self,
                service: service.to_string(),
                start: Instant::now(),
                model_name: model_name.to_string(),
            }
        }
    }

    pin_project! {
        pub struct ExternalRequestFuture<F> {
            #[pin]
            inner: F,
            service: String,
            model_name: String,
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
                    let duration = this.start.elapsed().as_millis() as f64;

                    histogram!(
                        "external_request_duration_seconds_ms",
                        "target" => this.service.clone(),
                        "method" => "POST",
                        "status" => "completed",
                        "model" => this.model_name.clone(),
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
        let handle = PrometheusBuilder::new()
            .set_buckets(
                &((100..=10000)
                    .step_by(100)
                    .map(|x| x as f64)
                    .collect::<Vec<f64>>()),
            )
            .unwrap()
            .install_recorder()
            .expect("Failed to install recorder");

        describe_counter!("http_requests_total", "Total HTTP requests");
        describe_gauge!("http_requests_active", "Active connections");
        describe_histogram!(
            "http_request_duration_seconds_ms",
            "HTTP Latency (full body/stream duration)"
        );
        describe_histogram!("function_duration_seconds_ms", "Internal Latency");
        describe_counter!("function_errors_total", "Internal Errors");
        describe_histogram!(
            "external_request_duration_seconds_ms",
            "Outbound HTTP Latency"
        );
        describe_histogram!(
            "external_stream_duration_seconds_ms",
            "Outbound Stream Duration"
        );
        describe_histogram!(
            "external_stream_ttft_seconds_ms",
            "Time to first token for outbound streams"
        );
        describe_histogram!(
            "websocket_session_duration_seconds_ms",
            "Duration of WebSocket sessions"
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
