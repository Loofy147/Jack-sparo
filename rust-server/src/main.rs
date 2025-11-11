use axum::{
    extract::{Multipart, State},
    routing::{get, post},
    Json, Router, response::IntoResponse
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use redis::AsyncCommands;
use ed25519_dalek::{PublicKey, Signature, Verifier};
use sha2::{Sha256, Digest};
use uuid::Uuid;
use std::{sync::Arc, time::{SystemTime, UNIX_EPOCH}};
use tracing::{info, error};
use anyhow::Result;
use bytes::Bytes;
use hex;

#[derive(Clone)]
struct AppState {
    db: PgPool,
    redis_url: String,
    // map miner_id -> public_key hex can be loaded from DB into cache in prod
}

#[derive(Serialize)]
struct TaskInfo {
    task_id: String,
    performance_threshold: f32,
    validation_data_hash: String,
    // storage info for shared optuna study could be added here
}

#[derive(Deserialize)]
struct SubmissionPayload {
    task_id: String,
    miner_id: i64,
    performance: f32,
    artifact_hash: String,
    hyperparameters: serde_json::Value,
    timestamp: u64,
    nonce: u64,
}

#[derive(Serialize)]
struct ApiResponse {
    status: String,
    reason: Option<String>
}

async fn get_task(_state: State<Arc<AppState>>) -> impl IntoResponse {
    // in real server, load from DB
    let task = TaskInfo {
        task_id: "task-prod-001".to_string(),
        performance_threshold: 0.90,
        validation_data_hash: "deadbeef...".to_string(),
    };
    Json(task)
}

async fn submit(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    // Expect fields:
    // - payload (json)
    // - signature (hex)
    // - artifact (file)
    let mut payload_json: Option<String> = None;
    let mut signature_hex: Option<String> = None;
    let mut artifact_bytes: Option<Bytes> = None;

    while let Some(field) = multipart.next_field().await.unwrap() {
        let name = field.name().map(|s| s.to_string()).unwrap_or_default();
        match name.as_str() {
            "payload" => {
                payload_json = Some(field.text().await.unwrap());
            }
            "signature" => {
                signature_hex = Some(field.text().await.unwrap());
            }
            "artifact" => {
                artifact_bytes = Some(field.bytes().await.unwrap());
            }
            _ => {}
        }
    }

    if payload_json.is_none() || signature_hex.is_none() || artifact_bytes.is_none() {
        return Json(ApiResponse {status: "rejected".into(), reason: Some("missing fields".into())});
    }

    let payload_json = payload_json.unwrap();
    let signature_hex = signature_hex.unwrap();
    let artifact_bytes = artifact_bytes.unwrap();

    // Parse payload
    let payload: SubmissionPayload = match serde_json::from_str(&payload_json) {
        Ok(p) => p,
        Err(e) => {
            error!("bad payload json: {}", e);
            return Json(ApiResponse {status: "rejected".into(), reason: Some("invalid payload json".into())});
        }
    };

    // 1) Verify nonce/replay in Redis
    let redis_client = redis::Client::open(state.redis_url.clone()).unwrap();
    let mut con = match redis_client.get_async_connection().await {
        Ok(c) => c,
        Err(e) => {
            error!("redis conn err: {}", e);
            return Json(ApiResponse {status: "rejected".into(), reason: Some("redis error".into())});
        }
    };
    let sig_key = format!("nonce:{}", signature_hex);
    let set_ok: i32 = match con.set_nx(&sig_key, 1).await {
        Ok(v) => if v {1} else {0},
        Err(e) => { error!("redis setnx fail: {}", e); return Json(ApiResponse {status: "rejected".into(), reason: Some("redis error".into())}); }
    };
    if set_ok == 0 {
        return Json(ApiResponse {status: "rejected".into(), reason: Some("replay".into())});
    }
    let _: () = con.expire(&sig_key, 300).await.unwrap_or(());

    // 2) Verify timestamp freshness
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    if payload.timestamp > now + 60 || now - payload.timestamp > 300 {
        return Json(ApiResponse {status: "rejected".into(), reason: Some("stale timestamp".into())});
    }

    // 3) Verify artifact hash matches
    let mut hasher = Sha256::new();
    hasher.update(&artifact_bytes);
    let computed = hex::encode(hasher.finalize());
    if computed != payload.artifact_hash {
        return Json(ApiResponse {status: "rejected".into(), reason: Some("artifact hash mismatch".into())});
    }

    // 4) Verify signature using stored miner public key
    let pubkey_hex: String = match sqlx::query_as("SELECT public_key FROM miners WHERE miner_id = $1")
        .bind(payload.miner_id)
        .fetch_one(&state.db)
        .await
    {
        Ok((pk,)) => pk,
        Err(e) => {
            error!("miner pk lookup error: {}", e);
            return Json(ApiResponse {
                status: "rejected".into(),
                reason: Some("unknown miner".into()),
            });
        }
    };
    let pubkey_bytes: Vec<u8> = match hex::decode(pubkey_hex) {
        Ok(b) => b,
        Err(_) => return Json(ApiResponse {status:"rejected".into(), reason: Some("invalid pubkey".into())}),
    };
    let vk = match PublicKey::from_bytes(&pubkey_bytes) {
        Ok(k) => k,
        Err(_) => return Json(ApiResponse {status:"rejected".into(), reason: Some("bad pubkey".into())}),
    };
    let sig_bytes: Vec<u8> = match hex::decode(signature_hex) {
        Ok(b) => b,
        Err(_) => return Json(ApiResponse {status:"rejected".into(), reason: Some("bad signature".into())}),
    };
    let sig = match Signature::from_bytes(&sig_bytes) {
        Ok(s) => s,
        Err(_) => return Json(ApiResponse {status:"rejected".into(), reason: Some("signature parse error".into())}),
    };
    if let Err(_) = vk.verify(payload_json.as_bytes(), &sig) {
        return Json(ApiResponse {status:"rejected".into(), reason: Some("bad_signature".into())});
    }

    // 5) Persist ledger row (artifact storage/tracking omitted for brevity)
    let ledger_id = Uuid::new_v4().to_string();
    match sqlx::query("INSERT INTO ledger(id, task_id, miner_id, performance, hyperparameters, artifact_hash, timestamp) VALUES ($1,$2,$3,$4,$5,$6, to_timestamp($7))")
        .bind(ledger_id.clone())
        .bind(payload.task_id)
        .bind(payload.miner_id)
        .bind(payload.performance as f64)
        .bind(payload.hyperparameters)
        .bind(payload.artifact_hash)
        .bind(payload.timestamp as i64)
        .execute(&state.db).await {
        Ok(_) => {
            info!("accepted submission {}", ledger_id);
            // Optionally write artifact to disk / object storage here
            return Json(ApiResponse {status: "accepted".into(), reason: None});
        }
        Err(e) => {
            error!("db insert error: {}", e);
            return Json(ApiResponse {status: "rejected".into(), reason: Some("db error".into())});
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    // read DB + Redis urls from env
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    let redis_url = std::env::var("REDIS_URL").unwrap_or("redis://127.0.0.1/".to_string());
    let db = PgPool::connect(&database_url).await?;

    let state = Arc::new(AppState { db, redis_url });

    let app = Router::new()
        .route("/get_task", get(get_task))
        .route("/submit", post(submit))
        .with_state(state);

    let addr = std::env::var("BIND_ADDR").unwrap_or("0.0.0.0:8080".into());
    info!("listening on {}", addr);
    axum::Server::bind(&addr.parse()?)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
