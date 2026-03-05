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
    downvote_threshold: Option<i64>,
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
    action: ModerateAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ModerateAction {
    Answer,
    FinishAnswering,
    Reject,
    Reopen,
    Restore,
    Delete,
    Ban,
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
    stopped_at: Option<i64>,
    downvote_threshold: i64,
}

#[derive(Deserialize)]
struct UpdateSessionRequest {
    name: String,
    description: Option<String>,
    stream_link: Option<String>,
    downvote_threshold: Option<i64>,
}

#[derive(Serialize, FromRow)]
struct QuestionView {
    id: i64,
    session_id: i64,
    author_user_id: i64,
    author_nickname: String,
    author_is_banned: i64,
    body: String,
    is_answering: i64,
    is_answered: i64,
    is_rejected: i64,
    is_deleted: i64,
    created_at: i64,
    score: i64,
    votes_count: i64,
    user_vote: i64,
    answering_started_at: Option<i64>,
    answered_at: Option<i64>,
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
    session_is_active: i64,
    session_name: Option<String>,
    question_body: String,
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
    sort: QuestionSort,
    questions: Vec<QuestionView>,
    question_cooldown_remaining: i64,
    viewer_is_banned: bool,
}

#[derive(Serialize, FromRow)]
struct BannedUser {
    user_id: i64,
    nickname: String,
    banned_at: i64,
    question_body: Option<String>,
    session_name: Option<String>,
}

#[derive(Serialize)]
struct ListBansResponse {
    bans: Vec<BannedUser>,
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
    sort: Option<QuestionSort>,
}

#[derive(FromRow)]
struct QuestionVoteMetaRow {
    session_id: i64,
    owner_user_id: i64,
    session_is_active: i64,
    status: QuestionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
enum QuestionStatus {
    New,
    Answering,
    Answered,
    Rejected,
    Deleted,
}

impl QuestionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Answering => "answering",
            Self::Answered => "answered",
            Self::Rejected => "rejected",
            Self::Deleted => "deleted",
        }
    }
}

struct QuestionVoteMeta {
    session_id: i64,
    owner_user_id: i64,
    session_is_active: i64,
    status: QuestionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuestionSort {
    #[serde(alias = "popular")]
    Top,
    New,
    Answered,
    Downvoted,
    Deleted,
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
    .allow_methods([
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::OPTIONS,
    ])
    .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/me", get(get_me))
        .route("/api/google_oauth2/start", get(google_oauth_start))
        .route("/api/google_oauth2", get(google_oauth_callback))
        .route(
            "/api/sessions",
            get(list_user_sessions).post(create_session),
        )
        .route(
            "/api/sessions/:code",
            put(update_session).delete(delete_session),
        )
        .route("/api/sessions/:code/stop", post(stop_session))
        .route("/api/bans", get(list_bans))
        .route("/api/bans/:user_id", axum::routing::delete(unban_user))
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
            created_at,
            last_login_at
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
            created_at,
            last_login_at;
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
            created_at,
            is_active,
            name,
            description,
            stream_link,
            stopped_at,
            downvote_threshold
        FROM stream_sessions
        WHERE owner_user_id = ?1
        ORDER BY created_at DESC;
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
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    });
    if let Some(ref d) = description {
        if d.chars().count() > 500 {
            return Err(AppError::bad_request(
                "description cannot exceed 500 characters",
            ));
        }
    }

    let stream_link = payload.stream_link.and_then(|l| {
        let t = l.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    });
    if let Some(ref l) = stream_link {
        if !l.starts_with("http://") && !l.starts_with("https://") {
            return Err(AppError::bad_request(
                "stream_link must start with http:// or https://",
            ));
        }
        if l.len() > 500 {
            return Err(AppError::bad_request(
                "stream_link cannot exceed 500 characters",
            ));
        }
    }

    let downvote_threshold = payload
        .downvote_threshold
        .map(|t| t.clamp(1, 1000))
        .unwrap_or(5);

    let mut inserted_id: Option<i64> = None;
    for _ in 0..8 {
        let public_code = generate_session_code();
        let res = sqlx::query(
            r#"
            INSERT INTO stream_sessions (owner_user_id, public_code, created_at, is_active, name, description, stream_link, downvote_threshold)
            VALUES (?1, ?2, unixepoch(), 1, ?3, ?4, ?5, ?6);
            "#,
        )
        .bind(auth.user_id)
        .bind(&public_code)
        .bind(&name)
        .bind(&description)
        .bind(&stream_link)
        .bind(downvote_threshold)
        .execute(&state.db)
        .await;

        match res {
            Ok(result) => {
                inserted_id = Some(result.last_insert_rowid());
                break;
            }
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => continue,
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
            created_at,
            is_active,
            name,
            description,
            stream_link,
            stopped_at,
            downvote_threshold
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
    headers: HeaderMap,
    Path(code): Path<String>,
    Query(query): Query<ListQuestionsQuery>,
) -> Result<Json<ListQuestionsResponse>, AppError> {
    let session = find_session_by_code(&state.db, &code).await?;
    let viewer_user_id = optional_auth_user_id(&state, &headers).await;

    let sort = query.sort.unwrap_or(QuestionSort::Top);
    let viewer_is_owner = viewer_user_id == Some(session.owner_user_id);
    if sort == QuestionSort::Deleted && !viewer_is_owner {
        return Err(AppError::forbidden(
            "only session owner can view deleted questions",
        ));
    }

    let questions = list_questions_for_session(
        &state.db,
        session.id,
        sort,
        viewer_user_id,
        session.downvote_threshold,
    )
    .await?;

    let (question_cooldown_remaining, viewer_is_banned) = if let Some(uid) = viewer_user_id {
        let row: (Option<i64>, i64) = sqlx::query_as(
            r#"
            SELECT
                (SELECT created_at FROM questions
                 WHERE session_id = ?1 AND author_user_id = ?2
                 ORDER BY created_at DESC LIMIT 1),
                EXISTS(SELECT 1 FROM bans WHERE owner_user_id = ?3 AND user_id = ?2)
            "#,
        )
        .bind(session.id)
        .bind(uid)
        .bind(session.owner_user_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or((None, 0));

        let cooldown = row.0.map(|t| (t + 60 - now_unix()).max(0)).unwrap_or(0);
        (cooldown, row.1 != 0)
    } else {
        (0, false)
    };

    Ok(Json(ListQuestionsResponse {
        session,
        sort,
        questions,
        question_cooldown_remaining,
        viewer_is_banned,
    }))
}

async fn session_events_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(code): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let session = find_session_by_code(&state.db, &code).await?;

    // Per-IP SSE connection limit
    let ip = resolve_request_ip(&headers, addr);
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

    let live_stream = BroadcastStream::new(receiver).filter_map(|message| {
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

    // Emit one event immediately so clients can transition to "live" without
    // waiting for the first keepalive tick or a new question event.
    let initial_stream = tokio_stream::iter([Ok(Event::default().data(
        serde_json::to_string(&SessionEvent::resync())
            .unwrap_or_else(|_| r#"{"kind":"resync","question_id":null}"#.to_string()),
    ))]);
    let stream = initial_stream.chain(live_stream);

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

fn resolve_request_ip(headers: &HeaderMap, addr: SocketAddr) -> IpAddr {
    if !addr.ip().is_loopback() {
        return addr.ip();
    }

    if let Some(value) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(ip) = value
            .split(',')
            .map(|part| part.trim())
            .filter(|part| !part.is_empty())
            .next_back()
            .and_then(|part| part.parse::<IpAddr>().ok())
        {
            return ip;
        }
    }

    addr.ip()
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

    if is_user_banned(&state.db, session.owner_user_id, auth.user_id).await? {
        return Err(AppError::forbidden("you are banned in this session"));
    }

    let text = payload.text.trim();
    let text_len = text.chars().count();
    if !(1..=300).contains(&text_len) {
        return Err(AppError::bad_request(
            "question text must contain 1..300 characters",
        ));
    }
    if count_line_breaks(text) > 5 {
        return Err(AppError::bad_request(
            "question can contain at most 5 line breaks",
        ));
    }

    let insert = sqlx::query(
        r#"
        INSERT INTO questions (session_id, author_user_id, body, created_at)
        SELECT ?1, ?2, ?3, unixepoch()
        WHERE NOT EXISTS (
            SELECT 1
            FROM questions
            WHERE session_id = ?1
              AND author_user_id = ?2
              AND created_at > unixepoch() - 60
        );
        "#,
    )
    .bind(session.id)
    .bind(auth.user_id)
    .bind(text)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to create question: {err}")))?;

    if insert.rows_affected() == 0 {
        tracing::warn!(user_id = auth.user_id, session = %code, "question rate limit hit");
        return Err(AppError::too_many_requests(
            "you can post only one question per minute",
        ));
    }

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

    if payload.value != -1 && payload.value != 0 && payload.value != 1 {
        return Err(AppError::bad_request("vote value must be -1, 0, or 1"));
    }

    let question_meta_row = sqlx::query_as::<_, QuestionVoteMetaRow>(
        r#"
        SELECT q.session_id, s.owner_user_id, s.is_active AS session_is_active,
               q.status
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

    let question_meta = QuestionVoteMeta {
        session_id: question_meta_row.session_id,
        owner_user_id: question_meta_row.owner_user_id,
        session_is_active: question_meta_row.session_is_active,
        status: question_meta_row.status,
    };

    if question_meta.owner_user_id == auth.user_id {
        return Err(AppError::forbidden(
            "session owner cannot vote for questions",
        ));
    }

    if question_meta.session_is_active == 0 {
        return Err(AppError::forbidden("session is stopped"));
    }

    match question_meta.status {
        QuestionStatus::New => {}
        QuestionStatus::Answering | QuestionStatus::Answered => {
            return Err(AppError::bad_request(
                "cannot vote for answered or in-progress questions",
            ));
        }
        QuestionStatus::Rejected => {
            return Err(AppError::bad_request("cannot vote for rejected questions"));
        }
        QuestionStatus::Deleted => {
            return Err(AppError::bad_request("cannot vote for deleted questions"));
        }
    }

    if is_user_banned(&state.db, question_meta.owner_user_id, auth.user_id).await? {
        return Err(AppError::forbidden("you are banned in this session"));
    }

    let mut tx =
        state.db.begin().await.map_err(|err| {
            AppError::internal(format!("failed to start vote transaction: {err}"))
        })?;

    // Vote action rate limiting: max 200 actions per minute.
    sqlx::query(
        r#"
        INSERT INTO vote_actions (user_id, question_id, value, created_at)
        VALUES (?1, ?2, ?3, unixepoch());
        "#,
    )
    .bind(auth.user_id)
    .bind(question_id)
    .bind(payload.value)
    .execute(&mut *tx)
    .await
    .map_err(|err| AppError::internal(format!("failed to record vote action: {err}")))?;

    let vote_action_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM vote_actions
        WHERE user_id = ?1 AND created_at > unixepoch() - 60;
        "#,
    )
    .bind(auth.user_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| AppError::internal(format!("failed to check vote action rate limit: {err}")))?;

    if vote_action_count > 200 {
        tx.rollback().await.ok();
        tracing::warn!(
            user_id = auth.user_id,
            "vote action rate limit hit ({vote_action_count}/min)"
        );
        return Err(AppError::too_many_requests(
            "too many votes, please slow down",
        ));
    }

    sqlx::query("DELETE FROM vote_actions WHERE created_at <= unixepoch() - 86400;")
        .execute(&mut *tx)
        .await
        .map_err(|err| AppError::internal(format!("failed to cleanup vote actions: {err}")))?;

    let previous_vote: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT value
        FROM votes
        WHERE question_id = ?1 AND user_id = ?2
        LIMIT 1;
        "#,
    )
    .bind(question_id)
    .bind(auth.user_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|err| AppError::internal(format!("failed to read previous vote: {err}")))?;

    let (delta, user_vote) = match (previous_vote, payload.value) {
        (None, 0) => (0_i64, 0_i64),
        (None, new_value) => {
            sqlx::query(
                r#"
                INSERT INTO votes (question_id, user_id, value, created_at, updated_at)
                VALUES (?1, ?2, ?3, unixepoch(), unixepoch());
                "#,
            )
            .bind(question_id)
            .bind(auth.user_id)
            .bind(new_value)
            .execute(&mut *tx)
            .await
            .map_err(|err| AppError::internal(format!("failed to save vote: {err}")))?;
            (new_value, new_value)
        }
        (Some(old_value), 0) => {
            sqlx::query("DELETE FROM votes WHERE question_id = ?1 AND user_id = ?2;")
                .bind(question_id)
                .bind(auth.user_id)
                .execute(&mut *tx)
                .await
                .map_err(|err| AppError::internal(format!("failed to remove vote: {err}")))?;
            (-old_value, 0)
        }
        (Some(old_value), new_value) => {
            sqlx::query(
                r#"
                UPDATE votes
                SET value = ?3, updated_at = unixepoch()
                WHERE question_id = ?1 AND user_id = ?2;
                "#,
            )
            .bind(question_id)
            .bind(auth.user_id)
            .bind(new_value)
            .execute(&mut *tx)
            .await
            .map_err(|err| AppError::internal(format!("failed to update vote: {err}")))?;
            (new_value - old_value, new_value)
        }
    };

    if delta != 0 {
        sqlx::query(
            r#"
            UPDATE questions
            SET score = score + ?1
            WHERE id = ?2;
            "#,
        )
        .bind(delta)
        .bind(question_id)
        .execute(&mut *tx)
        .await
        .map_err(|err| AppError::internal(format!("failed to update question score: {err}")))?;
    }

    let score: i64 = sqlx::query_scalar(
        r#"
        SELECT score
        FROM questions
        WHERE id = ?1
        LIMIT 1;
        "#,
    )
    .bind(question_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| AppError::internal(format!("failed to fetch question score: {err}")))?;

    tx.commit()
        .await
        .map_err(|err| AppError::internal(format!("failed to commit vote transaction: {err}")))?;

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
        user_vote,
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
        SELECT s.id AS session_id,
               s.owner_user_id,
               q.author_user_id,
               s.is_active AS session_is_active,
               s.name AS session_name,
               q.body AS question_body
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

    let action = payload.action;
    if meta.session_is_active == 0
        && matches!(
            action,
            ModerateAction::Answer
                | ModerateAction::FinishAnswering
                | ModerateAction::Reject
                | ModerateAction::Reopen
        )
    {
        return Err(AppError::forbidden(
            "session is stopped: only delete and ban are allowed",
        ));
    }

    match action {
        ModerateAction::Answer => {
            let update = sqlx::query(
                r#"
                UPDATE questions
                SET status = ?2,
                    answering_started_at = unixepoch(),
                    answered_at = NULL
                WHERE id = ?1 AND status IN (?3, ?4);
                "#,
            )
            .bind(question_id)
            .bind(QuestionStatus::Answering.as_str())
            .bind(QuestionStatus::New.as_str())
            .bind(QuestionStatus::Rejected.as_str())
            .execute(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!("failed to mark question as in-progress: {err}"))
            })?;

            if update.rows_affected() == 0 {
                return Err(AppError::bad_request(
                    "cannot mark this question as in-progress",
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
        ModerateAction::FinishAnswering => {
            let update = sqlx::query(
                r#"
                UPDATE questions
                SET status = ?2,
                    answered_at = unixepoch()
                WHERE id = ?1 AND status = ?3;
                "#,
            )
            .bind(question_id)
            .bind(QuestionStatus::Answered.as_str())
            .bind(QuestionStatus::Answering.as_str())
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
        ModerateAction::Reject => {
            let update = sqlx::query(
                r#"
                UPDATE questions
                SET status = ?2,
                    answering_started_at = NULL,
                    answered_at = NULL
                WHERE id = ?1 AND status IN (?3, ?4);
                "#,
            )
            .bind(question_id)
            .bind(QuestionStatus::Rejected.as_str())
            .bind(QuestionStatus::New.as_str())
            .bind(QuestionStatus::Answering.as_str())
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
        ModerateAction::Delete => {
            // Soft-delete: mark question as deleted instead of removing from DB.
            sqlx::query(
                r#"
                UPDATE questions
                SET status = ?2
                WHERE id = ?1 AND status != ?2;
                "#,
            )
            .bind(question_id)
            .bind(QuestionStatus::Deleted.as_str())
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
        ModerateAction::Ban => {
            if meta.author_user_id == auth.user_id {
                return Err(AppError::bad_request("cannot ban yourself"));
            }
            if is_user_banned(&state.db, meta.owner_user_id, meta.author_user_id).await? {
                return Err(AppError::bad_request("user is already banned"));
            }

            sqlx::query(
                r#"
                INSERT INTO bans (owner_user_id, user_id, message, session_name, created_at)
                VALUES (?1, ?2, ?3, ?4, unixepoch())
                ON CONFLICT(owner_user_id, user_id) DO UPDATE
                SET message = excluded.message,
                    session_name = excluded.session_name,
                    created_at = unixepoch();
                "#,
            )
            .bind(auth.user_id)
            .bind(meta.author_user_id)
            .bind(&meta.question_body)
            .bind(&meta.session_name)
            .execute(&state.db)
            .await
            .map_err(|err| AppError::internal(format!("failed to ban user: {err}")))?;

            sqlx::query(
                r#"
                UPDATE questions
                SET status = ?3
                WHERE session_id = ?1 AND author_user_id = ?2 AND status != ?3;
                "#,
            )
            .bind(meta.session_id)
            .bind(meta.author_user_id)
            .bind(QuestionStatus::Deleted.as_str())
            .execute(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!("failed to delete banned user questions: {err}"))
            })?;

            tracing::info!(
                question_id,
                user_id = auth.user_id,
                banned_user = meta.author_user_id,
                "user banned and questions deleted"
            );
            state
                .session_events
                .publish(meta.session_id, SessionEvent::resync())
                .await;

            Ok(Json(ModerateQuestionResponse {
                question_id,
                deleted: false,
                banned: true,
                question: None,
            }))
        }
        ModerateAction::Restore => {
            let update = sqlx::query(
                r#"
                UPDATE questions
                SET status = ?2,
                    answering_started_at = NULL,
                    answered_at = NULL
                WHERE id = ?1 AND status = ?3;
                "#,
            )
            .bind(question_id)
            .bind(QuestionStatus::New.as_str())
            .bind(QuestionStatus::Deleted.as_str())
            .execute(&state.db)
            .await
            .map_err(|err| AppError::internal(format!("failed to restore question: {err}")))?;

            if update.rows_affected() == 0 {
                return Err(AppError::bad_request("question is not deleted"));
            }

            let question = fetch_question_by_id(&state.db, question_id).await?;
            tracing::info!(question_id, user_id = auth.user_id, "question restored");
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
        ModerateAction::Reopen => {
            let update = sqlx::query(
                r#"
                UPDATE questions
                SET status = ?2,
                    answering_started_at = NULL,
                    answered_at = NULL
                WHERE id = ?1 AND status IN (?3, ?4);
                "#,
            )
            .bind(question_id)
            .bind(QuestionStatus::New.as_str())
            .bind(QuestionStatus::Answering.as_str())
            .bind(QuestionStatus::Answered.as_str())
            .execute(&state.db)
            .await
            .map_err(|err| AppError::internal(format!("failed to reopen question: {err}")))?;

            if update.rows_affected() == 0 {
                return Err(AppError::bad_request(
                    "question is not in answering or answered state",
                ));
            }

            let question = fetch_question_by_id(&state.db, question_id).await?;
            tracing::info!(question_id, user_id = auth.user_id, "question reopened");
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

    sqlx::query(
        "UPDATE stream_sessions SET is_active = 0, stopped_at = unixepoch() WHERE id = ?1;",
    )
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
        return Err(AppError::forbidden(
            "only session owner can update session info",
        ));
    }

    let name = payload.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad_request("name cannot be empty"));
    }
    if name.chars().count() > 100 {
        return Err(AppError::bad_request("name cannot exceed 100 characters"));
    }

    let description = payload.description.and_then(|d| {
        let t = d.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    });

    if let Some(ref d) = description {
        if d.chars().count() > 500 {
            return Err(AppError::bad_request(
                "description cannot exceed 500 characters",
            ));
        }
    }

    let stream_link = payload.stream_link.and_then(|l| {
        let t = l.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    });

    if let Some(ref l) = stream_link {
        if !l.starts_with("http://") && !l.starts_with("https://") {
            return Err(AppError::bad_request(
                "stream_link must start with http:// or https://",
            ));
        }
        if l.len() > 500 {
            return Err(AppError::bad_request(
                "stream_link cannot exceed 500 characters",
            ));
        }
    }

    let downvote_threshold = payload
        .downvote_threshold
        .map(|t| t.clamp(1, 1000))
        .unwrap_or(session.downvote_threshold);

    sqlx::query(
        r#"
        UPDATE stream_sessions
        SET name = ?1, description = ?2, stream_link = ?3, downvote_threshold = ?4
        WHERE id = ?5;
        "#,
    )
    .bind(&name)
    .bind(&description)
    .bind(&stream_link)
    .bind(downvote_threshold)
    .bind(session.id)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to update session: {err}")))?;

    tracing::info!(session_id = session.id, %name, downvote_threshold, "session metadata updated");
    let updated = find_session_by_code(&state.db, &code).await?;
    Ok(Json(updated))
}

async fn list_bans(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ListBansResponse>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    let bans = sqlx::query_as::<_, BannedUser>(
        r#"
        SELECT b.user_id,
               u.nickname,
               b.created_at AS banned_at,
               b.message AS question_body,
               b.session_name
        FROM bans b
        JOIN users u ON u.id = b.user_id
        WHERE b.owner_user_id = ?1
        ORDER BY b.created_at DESC;
        "#,
    )
    .bind(auth.user_id)
    .fetch_all(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to list bans: {err}")))?;

    Ok(Json(ListBansResponse { bans }))
}

async fn unban_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
) -> Result<StatusCode, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    sqlx::query("DELETE FROM bans WHERE owner_user_id = ?1 AND user_id = ?2;")
        .bind(auth.user_id)
        .bind(user_id)
        .execute(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to unban user: {err}")))?;

    tracing::info!(owner_id = auth.user_id, user_id, "user unbanned");
    Ok(StatusCode::NO_CONTENT)
}

async fn optional_auth_user_id(state: &AppState, headers: &HeaderMap) -> Option<i64> {
    let token = extract_bearer_token(headers)?;
    let token_hash = hash_token(&token);

    sqlx::query_scalar::<_, i64>(
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
    .ok()?
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
            created_at,
            is_active,
            name,
            description,
            stream_link,
            stopped_at,
            downvote_threshold
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
    viewer_user_id: Option<i64>,
    downvote_threshold: i64,
) -> Result<Vec<QuestionView>, AppError> {
    let threshold = downvote_threshold.max(1);
    let status_new = QuestionStatus::New.as_str();
    let status_answering = QuestionStatus::Answering.as_str();
    let status_answered = QuestionStatus::Answered.as_str();
    let status_rejected = QuestionStatus::Rejected.as_str();
    let status_deleted = QuestionStatus::Deleted.as_str();

    let (filter, order_by): (String, String) = match sort {
        QuestionSort::Answered => (
            format!(
                "WHERE q.session_id = ?1 AND q.status IN ('{status_answered}', '{status_rejected}')"
            ),
            "ORDER BY COALESCE(q.answered_at, q.created_at) DESC".to_string(),
        ),
        QuestionSort::New => (
            format!(
                "WHERE q.session_id = ?1 AND q.status IN ('{status_new}', '{status_answering}') \
                 AND (q.score > -{threshold} OR q.status = '{status_answering}')"
            ),
            format!("ORDER BY (q.status = '{status_answering}') DESC, q.created_at DESC"),
        ),
        QuestionSort::Top => (
            format!(
                "WHERE q.session_id = ?1 AND q.status IN ('{status_new}', '{status_answering}') \
                 AND (q.score > -{threshold} OR q.status = '{status_answering}')"
            ),
            format!(
                "ORDER BY (q.status = '{status_answering}') DESC, q.score DESC, q.created_at DESC"
            ),
        ),
        QuestionSort::Downvoted => (
            format!(
                "WHERE q.session_id = ?1 AND q.status = '{status_new}' AND q.score <= -{threshold}"
            ),
            "ORDER BY q.score ASC, q.created_at DESC".to_string(),
        ),
        QuestionSort::Deleted => (
            format!("WHERE q.session_id = ?1 AND q.status = '{status_deleted}'"),
            "ORDER BY q.created_at DESC".to_string(),
        ),
    };

    let sql = format!(
        r#"
        SELECT
            q.id,
            q.session_id,
            q.author_user_id,
            u.nickname AS author_nickname,
            EXISTS(
                SELECT 1
                FROM bans b
                WHERE b.owner_user_id = s.owner_user_id
                  AND b.user_id = q.author_user_id
            ) AS author_is_banned,
            q.body,
            (q.status = '{status_answering}') AS is_answering,
            (q.status = '{status_answered}') AS is_answered,
            (q.status = '{status_rejected}') AS is_rejected,
            (q.status = '{status_deleted}') AS is_deleted,
            q.created_at,
            q.score,
            (SELECT COUNT(*) FROM votes vv WHERE vv.question_id = q.id) AS votes_count,
            COALESCE(uv.value, 0) AS user_vote,
            q.answering_started_at,
            q.answered_at
        FROM questions q
        JOIN stream_sessions s ON s.id = q.session_id
        JOIN users u ON u.id = q.author_user_id
        LEFT JOIN votes uv ON uv.question_id = q.id AND uv.user_id = ?2
        {filter}
        {order_by};
        "#,
        filter = filter,
        order_by = order_by,
        status_answering = status_answering,
        status_answered = status_answered,
        status_rejected = status_rejected,
        status_deleted = status_deleted
    );

    sqlx::query_as::<_, QuestionView>(&sql)
        .bind(session_id)
        .bind(viewer_user_id.unwrap_or(-1))
        .fetch_all(db)
        .await
        .map_err(|err| AppError::internal(format!("failed to list questions: {err}")))
}

async fn fetch_question_by_id(db: &SqlitePool, question_id: i64) -> Result<QuestionView, AppError> {
    let status_answering = QuestionStatus::Answering.as_str();
    let status_answered = QuestionStatus::Answered.as_str();
    let status_rejected = QuestionStatus::Rejected.as_str();
    let status_deleted = QuestionStatus::Deleted.as_str();

    let sql = format!(
        r#"
        SELECT
            q.id,
            q.session_id,
            q.author_user_id,
            u.nickname AS author_nickname,
            EXISTS(
                SELECT 1
                FROM bans b
                WHERE b.owner_user_id = s.owner_user_id
                  AND b.user_id = q.author_user_id
            ) AS author_is_banned,
            q.body,
            (q.status = '{status_answering}') AS is_answering,
            (q.status = '{status_answered}') AS is_answered,
            (q.status = '{status_rejected}') AS is_rejected,
            (q.status = '{status_deleted}') AS is_deleted,
            q.created_at,
            q.score,
            (SELECT COUNT(*) FROM votes vv WHERE vv.question_id = q.id) AS votes_count,
            0 AS user_vote,
            q.answering_started_at,
            q.answered_at
        FROM questions q
        JOIN stream_sessions s ON s.id = q.session_id
        JOIN users u ON u.id = q.author_user_id
        WHERE q.id = ?1
        LIMIT 1;
        "#
    );

    sqlx::query_as::<_, QuestionView>(&sql)
        .bind(question_id)
        .fetch_optional(db)
        .await
        .map_err(|err| AppError::internal(format!("failed to fetch question: {err}")))?
        .ok_or_else(|| AppError::not_found("question not found"))
}

async fn is_user_banned(
    db: &SqlitePool,
    owner_user_id: i64,
    user_id: i64,
) -> Result<bool, AppError> {
    let banned: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT 1
        FROM bans
        WHERE owner_user_id = ?1 AND user_id = ?2
        LIMIT 1;
        "#,
    )
    .bind(owner_user_id)
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

fn count_line_breaks(text: &str) -> usize {
    let mut count = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => {
                count += 1;
            }
            '\r' => {
                count += 1;
                if matches!(chars.peek(), Some('\n')) {
                    let _ = chars.next();
                }
            }
            _ => {}
        }
    }
    count
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
