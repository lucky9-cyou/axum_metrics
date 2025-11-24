// main.rs

use axum::{
    Json, Router,
    extract::WebSocketUpgrade,
    response::sse::{Event, Sse},
    routing::{get, post},
};
use futures::stream::{self, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::TcpListener;

use observability_lib::client::{TrackExternal, track_stream};
use observability_lib::{
    ServiceMap, Trackable, WsTracker, setup_observability, spawn_monitored, tag_service_with_state,
    traffic_layer,
};

use async_openai::{
    Client,
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequestArgs,
        CreateChatCompletionStreamResponse,
    },
};

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if key.trim() != key {
            eprintln!(
                "⚠️  WARNING: OPENAI_API_KEY in .env contains spaces/newlines. This causes header errors!"
            );
        }
    }

    let handle = setup_observability().expect("Init failed");

    // Define your application/service names per route (self-defined).
    // Add as many exact/prefix rules as you need.
    let service_map = ServiceMap::new("gateway")
        .exact("/chat/stream", "llm_service")
        .exact("/chat/ask", "llm_service")
        .exact("/order", "order_service")
        .exact("/prices", "market_data_service")
        .exact("/chat", "ws_chat")
        .prefix("/api/v1/orders", "orders_v1")
        .prefix("/api/v1", "api_v1")
        .exact("/metrics", "metrics");

    let app = Router::new()
        .route("/chat/stream", post(llm_stream_handler))
        .route("/chat/ask", post(llm_unary_handler))
        .route("/order", post(create_order))
        .route("/prices", get(crypto_stream))
        .route("/chat", get(chat_handler))
        .route("/metrics", get(move || std::future::ready(handle.render())))
        // Inner layer: records HTTP metrics (expects ServiceTag to be set)
        .layer(axum::middleware::from_fn(traffic_layer))
        // Outermost layer: set per-route service/application label (fixes E0277)
        .layer(axum::middleware::from_fn_with_state(
            service_map,
            tag_service_with_state,
        ));

    println!("🚀 Server running on http://127.0.0.1:3000");
    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// --- Config Helper ---
fn get_openai_client() -> Client<OpenAIConfig> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .unwrap_or_else(|_| "sk-placeholder".to_string())
        .trim()
        .to_string();
    let base_url = std::env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
        .trim()
        .to_string();

    let config = OpenAIConfig::new()
        .with_api_key(api_key)
        .with_api_base(base_url);

    Client::with_config(config)
}

fn get_model() -> String {
    std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-3.5-turbo".to_string())
}

// ============================================================================
// HANDLERS (unchanged)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunkRecord {
    pub index: usize,
    pub content_delta: Option<String>,
    pub usage_snapshot: Option<serde_json::Value>,
    pub elapsed_ms: u128,
}

#[tracing::instrument(fields(service = "llm_service"), ret)]
async fn llm_stream_handler() -> Json<Value> {
    let client = get_openai_client();
    let model = get_model();

    let body = json!({
        "model": model,
        "messages": [
            { "role": "user", "content": "Write a short poem about Rust." }
        ],
        "stream": true,
        "stream_options": { "include_usage": true }
    });

    tracing::info!("Starting Stream Request...");

    let start_local = std::time::Instant::now();

    let result = client
        .chat()
        .create_stream_byot(body)
        .track_external("gpt_handshake", &model)
        .await
        .track();

    match result {
        Ok(stream) => {
            tracing::info!("Stream Handshake OK. Reading chunks...");

            let mut monitored = track_stream(stream, "gpt_generation", &model);

            let mut acc = String::with_capacity(2048);
            let mut chunks: Vec<StreamChunkRecord> = Vec::with_capacity(128);
            let mut idx = 0usize;
            let mut last_response_meta: Option<Value> = None;

            while let Some(evt) = monitored.next().await {
                match evt {
                    Ok(chunk) => {
                        let chunk: CreateChatCompletionStreamResponse = chunk;
                        let elapsed = start_local.elapsed().as_millis();

                        last_response_meta = Some(
                            serde_json::to_value(&chunk)
                                .unwrap_or_else(|_| json!({"error": "serialize_failed"})),
                        );

                        let mut delta_str: Option<String> = None;
                        for choice in &chunk.choices {
                            if let Some(delta) = &choice.delta.content {
                                acc.push_str(delta);
                                delta_str = Some(delta.clone());
                            }
                        }

                        let usage_snapshot = chunk.usage.as_ref().map(|_| {
                            serde_json::to_value(&chunk.usage)
                                .unwrap_or(json!({"error":"serialize_failed"}))
                        });

                        chunks.push(StreamChunkRecord {
                            index: idx,
                            content_delta: delta_str,
                            usage_snapshot,
                            elapsed_ms: elapsed,
                        });
                        idx += 1;
                    }
                    Err(e) => {
                        tracing::error!("Stream Error: {:?}", e);
                        return Json(
                            json!({ "error": "Stream interrupted", "details": e.to_string() }),
                        );
                    }
                }
            }

            let response_meta =
                last_response_meta.unwrap_or_else(|| json!({"warning": "no_chunks"}));

            tracing::info!("Stream Complete. Content length: {}", acc.len());

            Json(json!({
                "reply": acc,
                "meta": response_meta,
                "chunk_count": idx
            }))
        }
        Err(e) => {
            tracing::error!("Handshake Failed: {:?}", e);
            Json(json!({ "error": "Handshake failed", "details": e.to_string() }))
        }
    }
}

#[tracing::instrument(fields(service = "llm_service"), ret)]
async fn llm_unary_handler() -> Json<Value> {
    let client = get_openai_client();
    let model = get_model();

    let request = CreateChatCompletionRequestArgs::default()
        .model(model.clone())
        .messages([ChatCompletionRequestUserMessageArgs::default()
            .content("Write a haiku about Rust.")
            .build()
            .unwrap()
            .into()])
        .build()
        .unwrap();

    let response = client
        .chat()
        .create(request)
        .track_external("gpt_unary", &model)
        .await
        .track();

    match response {
        Ok(resp) => {
            let content = resp
                .choices
                .first()
                .and_then(|c| c.message.content.clone())
                .unwrap_or_default();
            Json(json!({ "reply": content }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

#[tracing::instrument(fields(service = "order_service"), ret)]
async fn create_order() -> Json<Value> {
    spawn_monitored(async {
        let _ = email_user().await;
    });
    if rand::random::<f32>() < 0.3 {
        let _ = process_payment().await;
        return Json(json!({ "status": "failed" }));
    }
    Json(json!({ "status": "created" }))
}

async fn crypto_stream() -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = stream::repeat_with(|| {
        let price = 100 + (rand::random::<u8>() as u16);
        Event::default().data(format!("Price: ${}", price))
    })
    .map(Ok)
    .then(|event| async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        event
    });
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

async fn chat_handler(ws: WebSocketUpgrade) -> impl axum::response::IntoResponse {
    ws.on_upgrade(|mut socket| async move {
        let _tracker = WsTracker::new("ws_chat");
        while let Some(Ok(msg)) = socket.recv().await {
            if let axum::extract::ws::Message::Text(t) = msg {
                let _ = socket.send(axum::extract::ws::Message::Text(t)).await;
            }
        }
    })
}

#[tracing::instrument]
async fn email_user() -> Result<(), &'static str> {
    tokio::time::sleep(Duration::from_millis(50)).await;
    if rand::random::<f32>() < 0.5 {
        Err("SMTP Fail").track()
    } else {
        Ok(())
    }
}

#[tracing::instrument]
async fn process_payment() -> Result<(), &'static str> {
    Err("Gateway Fail").track()
}
