use anyhow::Context;
use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderValue, Method, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};
use std::{env, net::SocketAddr};
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    http: Client,
    hcaptcha_secret: Option<String>,
    hcaptcha_site_key: Option<String>,
    hcaptcha_skip_verify: bool,
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let body = Json(ErrorResponse {
            error: self.message,
        });
        (self.status, body).into_response()
    }
}

#[derive(Deserialize)]
struct RegisterRequest {
    nickname: String,
    hcaptcha_token: String,
}

#[derive(Serialize)]
struct RegisterResponse {
    user: User,
}

#[derive(Serialize, FromRow)]
struct User {
    id: i64,
    nickname: String,
    created_at: String,
    last_login_at: String,
}

#[derive(Serialize)]
struct HealthResponse {
    ok: bool,
}

#[derive(Debug, Deserialize)]
struct HcaptchaVerifyResponse {
    success: bool,
    #[serde(rename = "error-codes", default)]
    error_codes: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let mut db_url =
        env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://qstream.db".to_string());
    if db_url.starts_with("sqlite://") && !db_url.contains("mode=") {
        if db_url.contains('?') {
            db_url.push_str("&mode=rwc");
        } else {
            db_url.push_str("?mode=rwc");
        }
    }
    let app_addr = env::var("APP_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
    let frontend_origin =
        env::var("FRONTEND_ORIGIN").unwrap_or_else(|_| "http://localhost:5173".to_string());

    let hcaptcha_secret = env::var("HCAPTCHA_SECRET").ok();
    let hcaptcha_site_key = env::var("HCAPTCHA_SITE_KEY").ok();
    let hcaptcha_skip_verify = env::var("HCAPTCHA_SKIP_VERIFY")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);

    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .with_context(|| format!("failed to connect to database: {db_url}"))?;

    init_db(&db).await?;

    let state = AppState {
        db,
        http: Client::new(),
        hcaptcha_secret,
        hcaptcha_site_key,
        hcaptcha_skip_verify,
    };

    let cors = CorsLayer::new()
        .allow_origin(
            HeaderValue::from_str(&frontend_origin)
                .with_context(|| format!("invalid FRONTEND_ORIGIN: {frontend_origin}"))?,
        )
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE]);

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/register", post(register))
        .with_state(state)
        .layer(cors);

    let addr: SocketAddr = app_addr
        .parse()
        .with_context(|| format!("invalid APP_ADDR: {app_addr}"))?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind: {addr}"))?;

    tracing::info!("server listening on {addr}");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;

    Ok(())
}

async fn init_db(db: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            nickname TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            last_login_at TEXT NOT NULL DEFAULT (datetime('now')),
            last_hcaptcha_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        "#,
    )
    .execute(db)
    .await?;

    Ok(())
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { ok: true })
}

async fn register(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, AppError> {
    let nickname = payload.nickname.trim();
    if !(2..=32).contains(&nickname.chars().count()) {
        return Err(AppError::bad_request(
            "nickname must contain 2..32 characters",
        ));
    }

    if payload.hcaptcha_token.trim().is_empty() {
        return Err(AppError::bad_request("hcaptcha token is required"));
    }

    verify_hcaptcha(
        &state,
        payload.hcaptcha_token.trim(),
        Some(addr.ip().to_string()),
    )
    .await?;

    sqlx::query(
        r#"
        INSERT INTO users (nickname, last_login_at, last_hcaptcha_at)
        VALUES (?1, datetime('now'), datetime('now'))
        ON CONFLICT(nickname) DO UPDATE
        SET last_login_at = datetime('now'),
            last_hcaptcha_at = datetime('now');
        "#,
    )
    .bind(nickname)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to save user: {err}")))?;

    let user = sqlx::query_as::<_, User>(
        r#"
        SELECT id, nickname, created_at, last_login_at
        FROM users
        WHERE nickname = ?1;
        "#,
    )
    .bind(nickname)
    .fetch_one(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to fetch user: {err}")))?;

    Ok(Json(RegisterResponse { user }))
}

async fn verify_hcaptcha(
    state: &AppState,
    token: &str,
    remote_ip: Option<String>,
) -> Result<(), AppError> {
    if state.hcaptcha_skip_verify {
        return Ok(());
    }

    let secret = state
        .hcaptcha_secret
        .as_deref()
        .ok_or_else(|| AppError::internal("HCAPTCHA_SECRET is not configured"))?;

    let mut form: Vec<(String, String)> = vec![
        ("secret".to_string(), secret.to_string()),
        ("response".to_string(), token.to_string()),
    ];

    if let Some(ip) = remote_ip {
        form.push(("remoteip".to_string(), ip));
    }
    if let Some(site_key) = state.hcaptcha_site_key.as_deref() {
        form.push(("sitekey".to_string(), site_key.to_string()));
    }

    let verify = state
        .http
        .post("https://api.hcaptcha.com/siteverify")
        .form(&form)
        .send()
        .await
        .map_err(|err| AppError::internal(format!("hcaptcha request failed: {err}")))?;

    if !verify.status().is_success() {
        return Err(AppError::internal(format!(
            "hcaptcha returned unexpected status: {}",
            verify.status()
        )));
    }

    let body: HcaptchaVerifyResponse = verify
        .json()
        .await
        .map_err(|err| AppError::internal(format!("invalid hcaptcha response: {err}")))?;

    if !body.success {
        let details = if body.error_codes.is_empty() {
            "unknown hcaptcha error".to_string()
        } else {
            body.error_codes.join(", ")
        };
        return Err(AppError::unauthorized(format!(
            "hcaptcha validation failed: {details}"
        )));
    }

    Ok(())
}
