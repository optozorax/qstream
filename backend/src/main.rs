use anyhow::Context;
use axum::{
    extract::{ConnectInfo, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Redirect,
    },
    routing::{get, post, put},
    Json, Router,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};
use std::{
    collections::HashMap,
    convert::Infallible,
    env,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    http: Client,
    google_client_id: Option<String>,
    google_client_secret: Option<String>,
    google_redirect_uri: Option<String>,
    public_base_url: String,
    session_events: SessionEventBus,
    sse_connections: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

#[derive(Clone, Default)]
struct SessionEventBus {
    channels: Arc<RwLock<HashMap<i64, broadcast::Sender<SessionEvent>>>>,
}

impl SessionEventBus {
    async fn subscribe(&self, session_id: i64) -> broadcast::Receiver<SessionEvent> {
        self.get_or_create_channel(session_id).await.subscribe()
    }

    async fn publish(&self, session_id: i64, event: SessionEvent) {
        let sender = self.get_or_create_channel(session_id).await;
        let _ = sender.send(event);
    }

    async fn get_or_create_channel(&self, session_id: i64) -> broadcast::Sender<SessionEvent> {
        let mut channels = self.channels.write().await;
        channels
            .entry(session_id)
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }

    async fn cleanup(&self) {
        let mut channels = self.channels.write().await;
        let before = channels.len();
        channels.retain(|_, sender| sender.receiver_count() > 0);
        let removed = before - channels.len();
        if removed > 0 {
            tracing::debug!(
                removed,
                remaining = channels.len(),
                "cleaned up idle SSE channels"
            );
        }
    }
}

#[derive(Clone, Serialize)]
struct SessionEvent {
    kind: &'static str,
    question_id: Option<i64>,
}

impl SessionEvent {
    fn question_created(question_id: i64) -> Self {
        Self {
            kind: "question_created",
            question_id: Some(question_id),
        }
    }

    fn question_changed(question_id: i64) -> Self {
        Self {
            kind: "question_changed",
            question_id: Some(question_id),
        }
    }

    fn question_deleted(question_id: i64) -> Self {
        Self {
            kind: "question_deleted",
            question_id: Some(question_id),
        }
    }

    fn resync() -> Self {
        Self {
            kind: "resync",
            question_id: None,
        }
    }
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

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn too_many_requests(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
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

#[derive(Deserialize, Default)]
struct GoogleOAuthStartQuery {
    return_to: Option<String>,
}

#[derive(Deserialize, Default)]
struct GoogleOAuthCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct CreateSessionRequest {
    name: String,
    description: Option<String>,
    stream_link: Option<String>,
}

#[derive(Deserialize)]
struct CreateQuestionRequest {
    text: String,
}

#[derive(Deserialize)]
struct VoteRequest {
    value: i64,
}

#[derive(Deserialize)]
struct ModerateQuestionRequest {
    action: String,
}

#[derive(Serialize, FromRow)]
struct User {
    id: i64,
    nickname: String,
    created_at: i64,
    last_login_at: i64,
}

#[derive(Serialize, FromRow)]
struct StreamSession {
    id: i64,
    owner_user_id: i64,
    public_code: String,
    created_at: i64,
    is_active: i64,
    name: Option<String>,
    description: Option<String>,
    stream_link: Option<String>,
}

#[derive(Deserialize)]
struct UpdateSessionRequest {
    name: String,
    description: Option<String>,
    stream_link: Option<String>,
}

#[derive(Serialize, FromRow)]
struct QuestionView {
    id: i64,
    session_id: i64,
    author_user_id: i64,
    author_nickname: String,
    body: String,
    is_answering: i64,
    is_answered: i64,
    is_rejected: i64,
    is_deleted: i64,
    created_at: i64,
    score: i64,
    votes_count: i64,
}

#[derive(FromRow)]
struct AuthUser {
    user_id: i64,
}

#[derive(FromRow)]
struct QuestionAdminMeta {
    session_id: i64,
    owner_user_id: i64,
    author_user_id: i64,
}

#[derive(Serialize)]
struct CreateSessionResponse {
    session: StreamSession,
    public_url: String,
}

#[derive(Serialize)]
struct ListUserSessionsResponse {
    sessions: Vec<StreamSession>,
}

#[derive(Serialize)]
struct ListQuestionsResponse {
    session: StreamSession,
    sort: String,
    questions: Vec<QuestionView>,
}

#[derive(Serialize)]
struct VoteResponse {
    question_id: i64,
    score: i64,
    user_vote: i64,
}

#[derive(Serialize)]
struct ModerateQuestionResponse {
    question_id: i64,
    deleted: bool,
    banned: bool,
    question: Option<QuestionView>,
}

#[derive(Serialize)]
struct HealthResponse {
    ok: bool,
}

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct GoogleUserInfoResponse {
    sub: String,
    name: Option<String>,
}

#[derive(Deserialize, Default)]
struct ListQuestionsQuery {
    sort: Option<String>,
}

#[derive(FromRow)]
struct QuestionVoteMeta {
    session_id: i64,
    owner_user_id: i64,
    session_is_active: i64,
    is_answering: i64,
    is_answered: i64,
    is_rejected: i64,
    is_deleted: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuestionSort {
    Top,
    New,
    Answered,
}

impl QuestionSort {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "top" | "popular" => Some(Self::Top),
            "new" => Some(Self::New),
            "answered" => Some(Self::Answered),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::New => "new",
            Self::Answered => "answered",
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let mut db_url =
        env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://qstream.db?mode=rwc".to_string());
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
    let public_base_url =
        env::var("PUBLIC_BASE_URL").unwrap_or_else(|_| "http://localhost:5173".to_string());

    let google_client_id = env::var("GOOGLE_CLIENT_ID").ok();
    let google_client_secret = env::var("GOOGLE_CLIENT_SECRET").ok();
    let google_redirect_uri = env::var("GOOGLE_REDIRECT_URI").ok();
    let reset_db_on_boot = env::var("RESET_DB_ON_BOOT")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);

    tracing::info!(
        db = %db_url,
        addr = %app_addr,
        frontend_origin = %frontend_origin,
        google_oauth_configured = google_client_id.is_some() && google_client_secret.is_some() && google_redirect_uri.is_some(),
        reset_db = reset_db_on_boot,
        "starting qstream backend"
    );

    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&db_url)
        .await
        .with_context(|| format!("failed to connect to database: {db_url}"))?;

    tracing::info!("connected to database");
    init_db(&db, reset_db_on_boot).await?;
    tracing::info!(reset = reset_db_on_boot, "database schema ready");

    let session_events = SessionEventBus::default();

    // Spawn cleanup task for session event channels
    {
        let bus = session_events.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                bus.cleanup().await;
            }
        });
    }

    let state = AppState {
        db,
        http: Client::new(),
        google_client_id,
        google_client_secret,
        google_redirect_uri,
        public_base_url,
        session_events,
        sse_connections: Arc::new(Mutex::new(HashMap::new())),
    };

    let cors = if frontend_origin == "*" {
        CorsLayer::new().allow_origin(Any)
    } else {
        CorsLayer::new().allow_origin(
            HeaderValue::from_str(&frontend_origin)
                .with_context(|| format!("invalid FRONTEND_ORIGIN: {frontend_origin}"))?,
        )
    }
    .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE, Method::OPTIONS])
    .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/me", get(get_me))
        .route("/api/google_oauth2/start", get(google_oauth_start))
        .route("/api/google_oauth2", get(google_oauth_callback))
        .route("/api/sessions", get(list_user_sessions).post(create_session))
        .route("/api/sessions/:code", put(update_session).delete(delete_session))
        .route("/api/sessions/:code/stop", post(stop_session))
        .route("/api/sessions/:code/events", get(session_events_handler))
        .route(
            "/api/sessions/:code/questions",
            get(list_questions).post(create_question),
        )
        .route("/api/questions/:id/vote", post(vote_question))
        .route("/api/questions/:id/moderate", post(moderate_question))
        .with_state(state)
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = app_addr
        .parse()
        .with_context(|| format!("invalid APP_ADDR: {app_addr}"))?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind: {addr}"))?;

    tracing::info!("server listening on {addr}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        tokio::signal::ctrl_c().await.ok();
    })
    .await?;

    Ok(())
}

async fn init_db(db: &SqlitePool, reset_db_on_boot: bool) -> anyhow::Result<()> {
    if reset_db_on_boot {
        sqlx::raw_sql(include_str!("schema_reset.sql"))
            .execute(db)
            .await?;
    }

    sqlx::raw_sql(include_str!("schema.sql"))
        .execute(db)
        .await?;

    ensure_google_oauth_schema(db).await?;
    ensure_session_metadata_schema(db).await?;
    ensure_multi_session_schema(db).await?;

    Ok(())
}

async fn ensure_google_oauth_schema(db: &SqlitePool) -> anyhow::Result<()> {
    let has_google_sub: Option<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_table_info('users') WHERE name = 'google_sub' LIMIT 1;",
    )
    .fetch_optional(db)
    .await?;

    if has_google_sub.is_none() {
        sqlx::query("ALTER TABLE users ADD COLUMN google_sub TEXT;")
            .execute(db)
            .await?;
    }

    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_users_google_sub ON users(google_sub);")
        .execute(db)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS oauth_login_states (
            state TEXT PRIMARY KEY,
            return_to TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            expires_at INTEGER NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_oauth_login_states_expires_at ON oauth_login_states(expires_at);",
    )
    .execute(db)
    .await?;

    Ok(())
}

async fn ensure_session_metadata_schema(db: &SqlitePool) -> anyhow::Result<()> {
    for (col, col_type) in &[("name", "TEXT"), ("description", "TEXT"), ("stream_link", "TEXT")] {
        let has_col: Option<String> = sqlx::query_scalar(&format!(
            "SELECT name FROM pragma_table_info('stream_sessions') WHERE name = '{col}' LIMIT 1;"
        ))
        .fetch_optional(db)
        .await?;

        if has_col.is_none() {
            sqlx::query(&format!(
                "ALTER TABLE stream_sessions ADD COLUMN {col} {col_type};"
            ))
            .execute(db)
            .await?;
        }
    }
    Ok(())
}

async fn ensure_multi_session_schema(db: &SqlitePool) -> anyhow::Result<()> {
    // Check whether owner_user_id still has a UNIQUE constraint (index with a single column).
    let indexes: Vec<(String, i64)> = sqlx::query_as(
        r#"SELECT name, "unique" FROM pragma_index_list('stream_sessions') WHERE "unique" = 1;"#,
    )
    .fetch_all(db)
    .await?;

    let mut has_unique_owner = false;
    for (idx_name, _) in &indexes {
        let cols: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT name FROM pragma_index_info('{idx_name}');"
        ))
        .fetch_all(db)
        .await?;
        if cols == ["owner_user_id"] {
            has_unique_owner = true;
            break;
        }
    }

    if !has_unique_owner {
        return Ok(());
    }

    // Recreate the table without the UNIQUE constraint on owner_user_id.
    // name/description/stream_link are guaranteed to exist by the preceding migration.
    let mut conn = db.acquire().await?;
    sqlx::query("PRAGMA foreign_keys = OFF;")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        r#"
        CREATE TABLE stream_sessions_new (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            owner_user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            public_code TEXT NOT NULL UNIQUE,
            created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
            is_active   INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
            name        TEXT,
            description TEXT,
            stream_link TEXT
        );
        "#,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO stream_sessions_new
            SELECT id, owner_user_id, public_code, created_at, is_active, name, description, stream_link
            FROM stream_sessions;
        "#,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query("DROP TABLE stream_sessions;")
        .execute(&mut *conn)
        .await?;
    sqlx::query("ALTER TABLE stream_sessions_new RENAME TO stream_sessions;")
        .execute(&mut *conn)
        .await?;
    sqlx::query("PRAGMA foreign_keys = ON;")
        .execute(&mut *conn)
        .await?;

    tracing::info!("migrated stream_sessions: removed unique constraint on owner_user_id");
    Ok(())
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { ok: true })
}

async fn get_me(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<User>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    let user = sqlx::query_as::<_, User>(
        r#"
        SELECT
            id,
            nickname,
            CAST(created_at AS INTEGER) AS created_at,
            CAST(last_login_at AS INTEGER) AS last_login_at
        FROM users
        WHERE id = ?1
        LIMIT 1;
        "#,
    )
    .bind(auth.user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to fetch auth user: {err}")))?
    .ok_or_else(|| AppError::unauthorized("invalid or expired token"))?;

    Ok(Json(user))
}

async fn google_oauth_start(
    State(state): State<AppState>,
    Query(query): Query<GoogleOAuthStartQuery>,
) -> Result<Redirect, AppError> {
    let client_id = state
        .google_client_id
        .as_deref()
        .ok_or_else(|| AppError::internal("GOOGLE_CLIENT_ID is not configured"))?;
    let redirect_uri = state
        .google_redirect_uri
        .as_deref()
        .ok_or_else(|| AppError::internal("GOOGLE_REDIRECT_URI is not configured"))?;

    let return_to = sanitize_return_to(query.return_to.as_deref());
    let state_token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());

    sqlx::query("DELETE FROM oauth_login_states WHERE expires_at <= unixepoch();")
        .execute(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to cleanup oauth states: {err}")))?;

    sqlx::query(
        r#"
        INSERT INTO oauth_login_states (state, return_to, created_at, expires_at)
        VALUES (?1, ?2, unixepoch(), unixepoch() + 600);
        "#,
    )
    .bind(&state_token)
    .bind(&return_to)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to create oauth state: {err}")))?;

    let mut auth_url = reqwest::Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
        .map_err(|err| AppError::internal(format!("failed to build oauth url: {err}")))?;
    auth_url
        .query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid profile")
        .append_pair("state", &state_token)
        .append_pair("access_type", "online")
        .append_pair("prompt", "select_account");

    Ok(Redirect::to(auth_url.as_ref()))
}

async fn google_oauth_callback(
    State(state): State<AppState>,
    Query(query): Query<GoogleOAuthCallbackQuery>,
) -> Result<Redirect, AppError> {
    let client_id = state
        .google_client_id
        .as_deref()
        .ok_or_else(|| AppError::internal("GOOGLE_CLIENT_ID is not configured"))?;
    let client_secret = state
        .google_client_secret
        .as_deref()
        .ok_or_else(|| AppError::internal("GOOGLE_CLIENT_SECRET is not configured"))?;
    let redirect_uri = state
        .google_redirect_uri
        .as_deref()
        .ok_or_else(|| AppError::internal("GOOGLE_REDIRECT_URI is not configured"))?;

    let Some(state_token) = query.state.as_deref() else {
        return Ok(Redirect::to("/#auth_error=missing_state"));
    };

    let return_to = match consume_oauth_state(&state.db, state_token).await {
        Ok(return_to) => return_to,
        Err(_) => return Ok(Redirect::to("/#auth_error=invalid_state")),
    };

    if let Some(error) = query.error.as_deref() {
        let code = if error.is_empty() {
            "oauth_denied"
        } else {
            "oauth_provider_error"
        };
        tracing::warn!(google_error = %error, "google oauth callback returned error");
        return Ok(redirect_with_fragment(&return_to, code, None));
    }

    let Some(code) = query.code.as_deref() else {
        return Ok(redirect_with_fragment(
            &return_to,
            "missing_authorization_code",
            None,
        ));
    };

    let token_response = state
        .http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code", code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await
        .map_err(|err| AppError::internal(format!("google token exchange failed: {err}")))?;

    if !token_response.status().is_success() {
        tracing::warn!(status = %token_response.status(), "google token exchange returned non-success");
        return Ok(redirect_with_fragment(
            &return_to,
            "oauth_token_exchange_failed",
            None,
        ));
    }

    let token_payload: GoogleTokenResponse = token_response
        .json()
        .await
        .map_err(|err| AppError::internal(format!("invalid google token response: {err}")))?;

    let userinfo_response = state
        .http
        .get("https://openidconnect.googleapis.com/v1/userinfo")
        .bearer_auth(&token_payload.access_token)
        .send()
        .await
        .map_err(|err| AppError::internal(format!("google userinfo request failed: {err}")))?;

    if !userinfo_response.status().is_success() {
        tracing::warn!(status = %userinfo_response.status(), "google userinfo returned non-success");
        return Ok(redirect_with_fragment(
            &return_to,
            "oauth_userinfo_failed",
            None,
        ));
    }

    let profile: GoogleUserInfoResponse = userinfo_response
        .json()
        .await
        .map_err(|err| AppError::internal(format!("invalid google userinfo response: {err}")))?;

    let nickname = normalize_google_name(profile.name.as_deref());

    let user = sqlx::query_as::<_, User>(
        r#"
        INSERT INTO users (nickname, google_sub, last_login_at)
        VALUES (?1, ?2, unixepoch())
        ON CONFLICT(google_sub) DO UPDATE SET
            nickname = excluded.nickname,
            last_login_at = unixepoch()
        RETURNING
            id,
            nickname,
            CAST(created_at AS INTEGER) AS created_at,
            CAST(last_login_at AS INTEGER) AS last_login_at;
        "#,
    )
    .bind(&nickname)
    .bind(&profile.sub)
    .fetch_one(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to upsert oauth user: {err}")))?;

    let auth_token = create_auth_session(&state.db, user.id).await?;

    tracing::info!(user_id = user.id, nickname = %user.nickname, "google oauth login succeeded");
    Ok(redirect_with_fragment(
        &return_to,
        "auth_token",
        Some(&auth_token),
    ))
}

async fn list_user_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ListUserSessionsResponse>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    let sessions = sqlx::query_as::<_, StreamSession>(
        r#"
        SELECT
            id,
            owner_user_id,
            public_code,
            CAST(created_at AS INTEGER) AS created_at,
            is_active,
            name,
            description,
            stream_link
        FROM stream_sessions
        WHERE owner_user_id = ?1
        ORDER BY CAST(created_at AS INTEGER) DESC;
        "#,
    )
    .bind(auth.user_id)
    .fetch_all(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to list sessions: {err}")))?;

    Ok(Json(ListUserSessionsResponse { sessions }))
}

async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateSessionRequest>,
) -> Result<Json<CreateSessionResponse>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    let name = payload.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad_request("name cannot be empty"));
    }
    if name.chars().count() > 100 {
        return Err(AppError::bad_request("name cannot exceed 100 characters"));
    }

    let description = payload.description.and_then(|d| {
        let t = d.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    });
    if let Some(ref d) = description {
        if d.chars().count() > 500 {
            return Err(AppError::bad_request("description cannot exceed 500 characters"));
        }
    }

    let stream_link = payload.stream_link.and_then(|l| {
        let t = l.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    });
    if let Some(ref l) = stream_link {
        if !l.starts_with("http://") && !l.starts_with("https://") {
            return Err(AppError::bad_request(
                "stream_link must start with http:// or https://",
            ));
        }
        if l.len() > 500 {
            return Err(AppError::bad_request("stream_link cannot exceed 500 characters"));
        }
    }

    let mut inserted_id: Option<i64> = None;
    for _ in 0..8 {
        let public_code = generate_session_code();
        let res = sqlx::query(
            r#"
            INSERT INTO stream_sessions (owner_user_id, public_code, created_at, is_active, name, description, stream_link)
            VALUES (?1, ?2, unixepoch(), 1, ?3, ?4, ?5);
            "#,
        )
        .bind(auth.user_id)
        .bind(&public_code)
        .bind(&name)
        .bind(&description)
        .bind(&stream_link)
        .execute(&state.db)
        .await;

        match res {
            Ok(result) => {
                inserted_id = Some(result.last_insert_rowid());
                break;
            }
            Err(err) if err.to_string().contains("stream_sessions.public_code") => continue,
            Err(err) => {
                return Err(AppError::internal(format!(
                    "failed to create stream session: {err}"
                )));
            }
        }
    }

    let row_id =
        inserted_id.ok_or_else(|| AppError::internal("failed to generate unique session code"))?;

    let session = sqlx::query_as::<_, StreamSession>(
        r#"
        SELECT
            id,
            owner_user_id,
            public_code,
            CAST(created_at AS INTEGER) AS created_at,
            is_active,
            name,
            description,
            stream_link
        FROM stream_sessions
        WHERE id = ?1;
        "#,
    )
    .bind(row_id)
    .fetch_one(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to fetch new session: {err}")))?;

    tracing::info!(user_id = auth.user_id, code = %session.public_code, %name, "session created");
    Ok(Json(CreateSessionResponse {
        public_url: build_public_session_url(&state.public_base_url, &session.public_code),
        session,
    }))
}

async fn list_questions(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Query(query): Query<ListQuestionsQuery>,
) -> Result<Json<ListQuestionsResponse>, AppError> {
    let session = find_session_by_code(&state.db, &code).await?;

    let raw_sort = query.sort.as_deref().unwrap_or("top");
    let sort = QuestionSort::parse(raw_sort)
        .ok_or_else(|| AppError::bad_request("sort must be 'top', 'new', or 'answered'"))?;

    let questions = list_questions_for_session(&state.db, session.id, sort).await?;

    Ok(Json(ListQuestionsResponse {
        session,
        sort: sort.as_str().to_string(),
        questions,
    }))
}

async fn session_events_handler(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(code): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let session = find_session_by_code(&state.db, &code).await?;

    // Per-IP SSE connection limit
    let ip = addr.ip();
    {
        let mut conns = state.sse_connections.lock().await;
        let count = conns.entry(ip).or_insert(0);
        if *count >= 3 {
            tracing::warn!(ip = %ip, code = %code, "SSE connection limit reached");
            return Err(AppError::too_many_requests(
                "too many SSE connections from this IP",
            ));
        }
        *count += 1;
        tracing::info!(ip = %ip, code = %code, connections = *count, "SSE client connected");
    }

    let receiver = state.session_events.subscribe(session.id).await;

    // Clone for the drop guard
    let sse_connections = state.sse_connections.clone();

    let stream = BroadcastStream::new(receiver).filter_map(|message| {
        let event = match message {
            Ok(event) => event,
            Err(err) => {
                tracing::warn!("session event stream lagged: {err}");
                SessionEvent::resync()
            }
        };

        match serde_json::to_string(&event) {
            Ok(payload) => Some(Ok(Event::default().data(payload))),
            Err(err) => {
                tracing::warn!("failed to serialize session event: {err}");
                None
            }
        }
    });

    // Wrap stream to decrement connection count on drop
    let stream = SseDropGuard {
        inner: Box::pin(stream),
        sse_connections,
        ip,
        decremented: false,
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    ))
}

/// A stream wrapper that decrements the SSE connection count when dropped.
struct SseDropGuard<S> {
    inner: std::pin::Pin<Box<S>>,
    sse_connections: Arc<Mutex<HashMap<IpAddr, usize>>>,
    ip: IpAddr,
    decremented: bool,
}

impl<S: Stream<Item = Result<Event, Infallible>>> Stream for SseDropGuard<S> {
    type Item = Result<Event, Infallible>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl<S> Drop for SseDropGuard<S> {
    fn drop(&mut self) {
        if !self.decremented {
            self.decremented = true;
            let conns = self.sse_connections.clone();
            let ip = self.ip;
            tokio::spawn(async move {
                let mut conns = conns.lock().await;
                if let Some(count) = conns.get_mut(&ip) {
                    *count = count.saturating_sub(1);
                    let remaining = *count;
                    if remaining == 0 {
                        conns.remove(&ip);
                    }
                    tracing::info!(ip = %ip, connections = remaining, "SSE client disconnected");
                }
            });
        }
    }
}

async fn create_question(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(code): Path<String>,
    Json(payload): Json<CreateQuestionRequest>,
) -> Result<Json<QuestionView>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;
    let session = find_session_by_code(&state.db, &code).await?;

    if session.owner_user_id == auth.user_id {
        return Err(AppError::forbidden("session owner cannot create questions"));
    }

    if session.is_active == 0 {
        return Err(AppError::forbidden("session is stopped"));
    }

    if is_user_banned(&state.db, session.id, auth.user_id).await? {
        return Err(AppError::forbidden("you are banned in this session"));
    }

    let text = payload.text.trim();
    let text_len = text.chars().count();
    if !(1..=300).contains(&text_len) {
        return Err(AppError::bad_request(
            "question text must contain 1..300 characters",
        ));
    }

    let last_question_at: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT CAST(created_at AS INTEGER)
        FROM questions
        WHERE session_id = ?1 AND author_user_id = ?2
        ORDER BY CAST(created_at AS INTEGER) DESC
        LIMIT 1;
        "#,
    )
    .bind(session.id)
    .bind(auth.user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to check question rate limit: {err}")))?;

    if let Some(last_ts) = last_question_at {
        let now = now_unix();
        if now - last_ts < 60 {
            tracing::warn!(user_id = auth.user_id, session = %code, "question rate limit hit");
            return Err(AppError::too_many_requests(
                "you can post only one question per minute",
            ));
        }
    }

    let insert = sqlx::query(
        r#"
        INSERT INTO questions (session_id, author_user_id, body, created_at)
        VALUES (?1, ?2, ?3, unixepoch());
        "#,
    )
    .bind(session.id)
    .bind(auth.user_id)
    .bind(text)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to create question: {err}")))?;

    let question_id = insert.last_insert_rowid();
    let question = fetch_question_by_id(&state.db, question_id).await?;
    tracing::info!(
        question_id,
        user_id = auth.user_id,
        session = %code,
        chars = text_len,
        "question created"
    );
    state
        .session_events
        .publish(session.id, SessionEvent::question_created(question_id))
        .await;

    Ok(Json(question))
}

async fn vote_question(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(question_id): Path<i64>,
    Json(payload): Json<VoteRequest>,
) -> Result<Json<VoteResponse>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    if payload.value != -1 && payload.value != 1 {
        return Err(AppError::bad_request("vote value must be -1 or 1"));
    }

    let question_meta = sqlx::query_as::<_, QuestionVoteMeta>(
        r#"
        SELECT q.session_id, s.owner_user_id, s.is_active AS session_is_active,
               q.is_answering, q.is_answered, q.is_rejected, q.is_deleted
        FROM questions q
        JOIN stream_sessions s ON s.id = q.session_id
        WHERE q.id = ?1;
        "#,
    )
    .bind(question_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to find question: {err}")))?
    .ok_or_else(|| AppError::not_found("question not found"))?;

    if question_meta.owner_user_id == auth.user_id {
        return Err(AppError::forbidden(
            "session owner cannot vote for questions",
        ));
    }

    if question_meta.session_is_active == 0 {
        return Err(AppError::forbidden("session is stopped"));
    }

    if question_meta.is_answered == 1 || question_meta.is_answering == 1 {
        return Err(AppError::bad_request(
            "cannot vote for answered or in-progress questions",
        ));
    }

    if question_meta.is_rejected == 1 {
        return Err(AppError::bad_request("cannot vote for rejected questions"));
    }

    if question_meta.is_deleted == 1 {
        return Err(AppError::bad_request("cannot vote for deleted questions"));
    }

    if is_user_banned(&state.db, question_meta.session_id, auth.user_id).await? {
        return Err(AppError::forbidden("you are banned in this session"));
    }

    // Vote rate limiting: max 200 votes per minute
    let vote_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM votes
        WHERE user_id = ?1 AND updated_at > unixepoch() - 60;
        "#,
    )
    .bind(auth.user_id)
    .fetch_one(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to check vote rate limit: {err}")))?;

    if vote_count >= 200 {
        tracing::warn!(
            user_id = auth.user_id,
            "vote rate limit hit ({vote_count}/min)"
        );
        return Err(AppError::too_many_requests(
            "too many votes, please slow down",
        ));
    }

    sqlx::query(
        r#"
        INSERT INTO votes (question_id, user_id, value, created_at, updated_at)
        VALUES (?1, ?2, ?3, unixepoch(), unixepoch())
        ON CONFLICT(question_id, user_id) DO UPDATE
        SET value = excluded.value,
            updated_at = unixepoch();
        "#,
    )
    .bind(question_id)
    .bind(auth.user_id)
    .bind(payload.value)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to save vote: {err}")))?;

    let score: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(value), 0)
        FROM votes
        WHERE question_id = ?1;
        "#,
    )
    .bind(question_id)
    .fetch_one(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to recalculate score: {err}")))?;

    tracing::info!(
        question_id,
        user_id = auth.user_id,
        value = payload.value,
        score,
        "vote recorded"
    );
    state
        .session_events
        .publish(
            question_meta.session_id,
            SessionEvent::question_changed(question_id),
        )
        .await;

    Ok(Json(VoteResponse {
        question_id,
        score,
        user_vote: payload.value,
    }))
}

async fn moderate_question(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(question_id): Path<i64>,
    Json(payload): Json<ModerateQuestionRequest>,
) -> Result<Json<ModerateQuestionResponse>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    let meta = sqlx::query_as::<_, QuestionAdminMeta>(
        r#"
        SELECT s.id AS session_id, s.owner_user_id, q.author_user_id
        FROM questions q
        JOIN stream_sessions s ON s.id = q.session_id
        WHERE q.id = ?1
        LIMIT 1;
        "#,
    )
    .bind(question_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to load question admin metadata: {err}")))?
    .ok_or_else(|| AppError::not_found("question not found"))?;

    if meta.owner_user_id != auth.user_id {
        return Err(AppError::forbidden(
            "only session owner can moderate questions",
        ));
    }

    match payload.action.as_str() {
        "answer" => {
            let update = sqlx::query(
                r#"
                UPDATE questions
                SET is_answering = 1
                WHERE id = ?1 AND is_answered = 0;
                "#,
            )
            .bind(question_id)
            .execute(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!("failed to mark question as in-progress: {err}"))
            })?;

            if update.rows_affected() == 0 {
                return Err(AppError::bad_request(
                    "cannot mark already answered question as in-progress",
                ));
            }

            let question = fetch_question_by_id(&state.db, question_id).await?;
            tracing::info!(
                question_id,
                user_id = auth.user_id,
                "question marked answering"
            );
            state
                .session_events
                .publish(meta.session_id, SessionEvent::question_changed(question_id))
                .await;

            Ok(Json(ModerateQuestionResponse {
                question_id,
                deleted: false,
                banned: false,
                question: Some(question),
            }))
        }
        "finish_answering" => {
            let update = sqlx::query(
                r#"
                UPDATE questions
                SET is_answered = 1,
                    is_answering = 0
                WHERE id = ?1 AND is_answering = 1 AND is_answered = 0;
                "#,
            )
            .bind(question_id)
            .execute(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!("failed to finish answering question: {err}"))
            })?;

            if update.rows_affected() == 0 {
                return Err(AppError::bad_request(
                    "question is not in progress of answering",
                ));
            }

            let question = fetch_question_by_id(&state.db, question_id).await?;
            tracing::info!(question_id, user_id = auth.user_id, "question answered");
            state
                .session_events
                .publish(meta.session_id, SessionEvent::question_changed(question_id))
                .await;

            Ok(Json(ModerateQuestionResponse {
                question_id,
                deleted: false,
                banned: false,
                question: Some(question),
            }))
        }
        "reject" => {
            let update = sqlx::query(
                r#"
                UPDATE questions
                SET is_rejected = 1
                WHERE id = ?1 AND is_answered = 0 AND is_rejected = 0;
                "#,
            )
            .bind(question_id)
            .execute(&state.db)
            .await
            .map_err(|err| AppError::internal(format!("failed to reject question: {err}")))?;

            if update.rows_affected() == 0 {
                return Err(AppError::bad_request(
                    "cannot reject: question is already answered or rejected",
                ));
            }

            let question = fetch_question_by_id(&state.db, question_id).await?;
            tracing::info!(question_id, user_id = auth.user_id, "question rejected");
            state
                .session_events
                .publish(meta.session_id, SessionEvent::question_changed(question_id))
                .await;

            Ok(Json(ModerateQuestionResponse {
                question_id,
                deleted: false,
                banned: false,
                question: Some(question),
            }))
        }
        "delete" => {
            // Soft-delete: set is_deleted = 1 instead of removing from DB
            sqlx::query(
                r#"
                UPDATE questions
                SET is_deleted = 1
                WHERE id = ?1;
                "#,
            )
            .bind(question_id)
            .execute(&state.db)
            .await
            .map_err(|err| AppError::internal(format!("failed to delete question: {err}")))?;
            tracing::info!(question_id, user_id = auth.user_id, "question soft-deleted");
            state
                .session_events
                .publish(meta.session_id, SessionEvent::question_deleted(question_id))
                .await;

            Ok(Json(ModerateQuestionResponse {
                question_id,
                deleted: true,
                banned: false,
                question: None,
            }))
        }
        "ban" => {
            if meta.author_user_id == auth.user_id {
                return Err(AppError::bad_request("cannot ban yourself"));
            }

            sqlx::query(
                r#"
                INSERT INTO bans (session_id, user_id, created_at)
                VALUES (?1, ?2, unixepoch())
                ON CONFLICT(session_id, user_id) DO NOTHING;
                "#,
            )
            .bind(meta.session_id)
            .bind(meta.author_user_id)
            .execute(&state.db)
            .await
            .map_err(|err| AppError::internal(format!("failed to ban user: {err}")))?;

            let question = fetch_question_by_id(&state.db, question_id).await?;
            tracing::info!(
                question_id,
                user_id = auth.user_id,
                banned_user = meta.author_user_id,
                "user banned"
            );

            Ok(Json(ModerateQuestionResponse {
                question_id,
                deleted: false,
                banned: true,
                question: Some(question),
            }))
        }
        _ => Err(AppError::bad_request(
            "action must be one of: answer, finish_answering, reject, delete, ban",
        )),
    }
}

async fn stop_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Result<Json<StreamSession>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;
    let session = find_session_by_code(&state.db, &code).await?;

    if session.owner_user_id != auth.user_id {
        return Err(AppError::forbidden("only session owner can stop session"));
    }

    if session.is_active == 0 {
        return Err(AppError::bad_request("session is already stopped"));
    }

    sqlx::query("UPDATE stream_sessions SET is_active = 0 WHERE id = ?1;")
        .bind(session.id)
        .execute(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to stop session: {err}")))?;

    tracing::info!(session_id = session.id, code = %code, "session stopped");
    state
        .session_events
        .publish(session.id, SessionEvent::resync())
        .await;

    let updated = find_session_by_code(&state.db, &code).await?;
    Ok(Json(updated))
}

async fn delete_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Result<StatusCode, AppError> {
    let auth = require_auth_user(&state, &headers).await?;
    let session = find_session_by_code(&state.db, &code).await?;

    if session.owner_user_id != auth.user_id {
        return Err(AppError::forbidden("only session owner can delete session"));
    }

    if session.is_active != 0 {
        return Err(AppError::bad_request("stop the session before deleting it"));
    }

    sqlx::query("DELETE FROM stream_sessions WHERE id = ?1;")
        .bind(session.id)
        .execute(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to delete session: {err}")))?;

    tracing::info!(session_id = session.id, code = %code, "session deleted");
    Ok(StatusCode::NO_CONTENT)
}

async fn update_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(code): Path<String>,
    Json(payload): Json<UpdateSessionRequest>,
) -> Result<Json<StreamSession>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;
    let session = find_session_by_code(&state.db, &code).await?;

    if session.owner_user_id != auth.user_id {
        return Err(AppError::forbidden("only session owner can update session info"));
    }

    let name = payload.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad_request("name cannot be empty"));
    }
    if name.chars().count() > 100 {
        return Err(AppError::bad_request("name cannot exceed 100 characters"));
    }

    let description = payload.description.map(|d| {
        let t = d.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    }).flatten();

    if let Some(ref d) = description {
        if d.chars().count() > 500 {
            return Err(AppError::bad_request("description cannot exceed 500 characters"));
        }
    }

    let stream_link = payload.stream_link.map(|l| {
        let t = l.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    }).flatten();

    if let Some(ref l) = stream_link {
        if !l.starts_with("http://") && !l.starts_with("https://") {
            return Err(AppError::bad_request("stream_link must start with http:// or https://"));
        }
        if l.len() > 500 {
            return Err(AppError::bad_request("stream_link cannot exceed 500 characters"));
        }
    }

    sqlx::query(
        r#"
        UPDATE stream_sessions
        SET name = ?1, description = ?2, stream_link = ?3
        WHERE id = ?4;
        "#,
    )
    .bind(&name)
    .bind(&description)
    .bind(&stream_link)
    .bind(session.id)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to update session: {err}")))?;

    tracing::info!(session_id = session.id, %name, "session metadata updated");
    let updated = find_session_by_code(&state.db, &code).await?;
    Ok(Json(updated))
}

async fn require_auth_user(state: &AppState, headers: &HeaderMap) -> Result<AuthUser, AppError> {
    let token = extract_bearer_token(headers)
        .ok_or_else(|| AppError::unauthorized("missing bearer token"))?;
    let token_hash = hash_token(&token);

    let auth_user = sqlx::query_as::<_, AuthUser>(
        r#"
        SELECT s.user_id
        FROM auth_sessions s
        WHERE s.token_hash = ?1
          AND (s.expires_at IS NULL OR s.expires_at > unixepoch())
        ORDER BY s.id DESC
        LIMIT 1;
        "#,
    )
    .bind(&token_hash)
    .fetch_optional(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to validate token: {err}")))?
    .ok_or_else(|| AppError::unauthorized("invalid or expired token"))?;

    sqlx::query(
        r#"
        UPDATE auth_sessions
        SET last_seen_at = unixepoch()
        WHERE token_hash = ?1;
        "#,
    )
    .bind(token_hash)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to refresh auth session: {err}")))?;

    Ok(auth_user)
}

async fn find_session_by_code(db: &SqlitePool, code: &str) -> Result<StreamSession, AppError> {
    sqlx::query_as::<_, StreamSession>(
        r#"
        SELECT
            id,
            owner_user_id,
            public_code,
            CAST(created_at AS INTEGER) AS created_at,
            is_active,
            name,
            description,
            stream_link
        FROM stream_sessions
        WHERE public_code = ?1
        LIMIT 1;
        "#,
    )
    .bind(code)
    .fetch_optional(db)
    .await
    .map_err(|err| AppError::internal(format!("failed to find session: {err}")))?
    .ok_or_else(|| AppError::not_found("session not found"))
}

async fn list_questions_for_session(
    db: &SqlitePool,
    session_id: i64,
    sort: QuestionSort,
) -> Result<Vec<QuestionView>, AppError> {
    let (filter, order_by) = match sort {
        QuestionSort::Answered => (
            "WHERE q.session_id = ?1 AND (q.is_answered = 1 OR q.is_rejected = 1) AND q.is_deleted = 0",
            "ORDER BY CAST(q.created_at AS INTEGER) DESC",
        ),
        QuestionSort::New => (
            "WHERE q.session_id = ?1 AND q.is_answered = 0 AND q.is_rejected = 0 AND q.is_deleted = 0",
            "ORDER BY q.is_answering DESC, CAST(q.created_at AS INTEGER) DESC",
        ),
        QuestionSort::Top => (
            "WHERE q.session_id = ?1 AND q.is_answered = 0 AND q.is_rejected = 0 AND q.is_deleted = 0",
            "ORDER BY q.is_answering DESC, score DESC, CAST(q.created_at AS INTEGER) DESC",
        ),
    };

    let sql = format!(
        r#"
        SELECT
            q.id,
            q.session_id,
            q.author_user_id,
            u.nickname AS author_nickname,
            q.body,
            q.is_answering,
            q.is_answered,
            q.is_rejected,
            q.is_deleted,
            CAST(q.created_at AS INTEGER) AS created_at,
            COALESCE(SUM(v.value), 0) AS score,
            COALESCE(COUNT(v.user_id), 0) AS votes_count
        FROM questions q
        JOIN users u ON u.id = q.author_user_id
        LEFT JOIN votes v ON v.question_id = q.id
        {filter}
        GROUP BY q.id, q.session_id, q.author_user_id, u.nickname, q.body, q.is_answering, q.is_answered, q.is_rejected, q.is_deleted, q.created_at
        {order_by};
        "#
    );

    sqlx::query_as::<_, QuestionView>(&sql)
        .bind(session_id)
        .fetch_all(db)
        .await
        .map_err(|err| AppError::internal(format!("failed to list questions: {err}")))
}

async fn fetch_question_by_id(db: &SqlitePool, question_id: i64) -> Result<QuestionView, AppError> {
    sqlx::query_as::<_, QuestionView>(
        r#"
        SELECT
            q.id,
            q.session_id,
            q.author_user_id,
            u.nickname AS author_nickname,
            q.body,
            q.is_answering,
            q.is_answered,
            q.is_rejected,
            q.is_deleted,
            CAST(q.created_at AS INTEGER) AS created_at,
            COALESCE(SUM(v.value), 0) AS score,
            COALESCE(COUNT(v.user_id), 0) AS votes_count
        FROM questions q
        JOIN users u ON u.id = q.author_user_id
        LEFT JOIN votes v ON v.question_id = q.id
        WHERE q.id = ?1
        GROUP BY q.id, q.session_id, q.author_user_id, u.nickname, q.body, q.is_answering, q.is_answered, q.is_rejected, q.is_deleted, q.created_at
        LIMIT 1;
        "#,
    )
    .bind(question_id)
    .fetch_optional(db)
    .await
    .map_err(|err| AppError::internal(format!("failed to fetch question: {err}")))?
    .ok_or_else(|| AppError::not_found("question not found"))
}

async fn is_user_banned(db: &SqlitePool, session_id: i64, user_id: i64) -> Result<bool, AppError> {
    let banned: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT 1
        FROM bans
        WHERE session_id = ?1 AND user_id = ?2
        LIMIT 1;
        "#,
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(db)
    .await
    .map_err(|err| AppError::internal(format!("failed to check ban: {err}")))?;

    Ok(banned.is_some())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let auth_header = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = auth_header
        .strip_prefix("Bearer ")
        .or_else(|| auth_header.strip_prefix("bearer "))?;

    if token.is_empty() {
        return None;
    }

    Some(token.to_string())
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn generate_session_code() -> String {
    Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(10)
        .collect()
}

fn build_public_session_url(base: &str, code: &str) -> String {
    format!("{}/s/{code}", base.trim_end_matches('/'))
}

async fn create_auth_session(db: &SqlitePool, user_id: i64) -> Result<String, AppError> {
    let auth_token = format!("{}{}", Uuid::new_v4(), Uuid::new_v4().simple());
    let token_hash = hash_token(&auth_token);

    sqlx::query(
        r#"
        INSERT INTO auth_sessions (user_id, token_hash, created_at, last_seen_at)
        VALUES (?1, ?2, unixepoch(), unixepoch());
        "#,
    )
    .bind(user_id)
    .bind(token_hash)
    .execute(db)
    .await
    .map_err(|err| AppError::internal(format!("failed to create auth session: {err}")))?;

    Ok(auth_token)
}

async fn consume_oauth_state(db: &SqlitePool, state_token: &str) -> Result<String, AppError> {
    let mut tx = db
        .begin()
        .await
        .map_err(|err| AppError::internal(format!("failed to start oauth state tx: {err}")))?;

    let return_to: Option<String> = sqlx::query_scalar(
        r#"
        SELECT return_to
        FROM oauth_login_states
        WHERE state = ?1 AND expires_at > unixepoch()
        LIMIT 1;
        "#,
    )
    .bind(state_token)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|err| AppError::internal(format!("failed to read oauth state: {err}")))?;

    sqlx::query("DELETE FROM oauth_login_states WHERE state = ?1;")
        .bind(state_token)
        .execute(&mut *tx)
        .await
        .map_err(|err| AppError::internal(format!("failed to consume oauth state: {err}")))?;

    tx.commit()
        .await
        .map_err(|err| AppError::internal(format!("failed to commit oauth state tx: {err}")))?;

    return_to.ok_or_else(|| AppError::unauthorized("invalid or expired oauth state"))
}

fn sanitize_return_to(raw: Option<&str>) -> String {
    let candidate = raw.unwrap_or("/");
    if candidate.len() > 2048 {
        return "/".to_string();
    }
    if !candidate.starts_with('/') || candidate.starts_with("//") {
        return "/".to_string();
    }

    let without_fragment = candidate.split('#').next().unwrap_or("/");
    if without_fragment.is_empty() {
        "/".to_string()
    } else {
        without_fragment.to_string()
    }
}

fn normalize_google_name(raw: Option<&str>) -> String {
    let trimmed = raw.unwrap_or("").trim();
    if trimmed.is_empty() {
        return "google_user".to_string();
    }

    let mut normalized = String::new();
    for ch in trimmed.chars().take(64) {
        if ch.is_control() {
            continue;
        }
        normalized.push(ch);
    }

    let final_name = normalized.trim();
    if final_name.is_empty() {
        "google_user".to_string()
    } else {
        final_name.to_string()
    }
}

fn redirect_with_fragment(return_to: &str, key: &str, value: Option<&str>) -> Redirect {
    let path = sanitize_return_to(Some(return_to));
    let target = if let Some(value) = value {
        format!("{path}#{key}={value}")
    } else {
        format!("{path}#auth_error={key}")
    };
    Redirect::to(&target)
}
