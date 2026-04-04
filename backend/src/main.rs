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
use sqlx::{sqlite::SqlitePoolOptions, FromRow, Row, SqlitePool};
use std::{
    collections::HashMap,
    convert::Infallible,
    env,
    future::Future,
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
    da_client_id: Option<String>,
    da_client_secret: Option<String>,
    da_redirect_uri: Option<String>,
    public_base_url: String,
    session_events: SessionEventBus,
    sse_connections: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

const SESSION_MAX_AGE_SECS: i64 = 48 * 60 * 60;
const AUTH_SESSION_TOUCH_INTERVAL_SECS: i64 = 10 * 60;
const DA_SYNC_STATE_TOUCH_INTERVAL_SECS: i64 = 15 * 60;
const SQLITE_BUSY_RETRY_ATTEMPTS: usize = 3;
const SQLITE_BUSY_RETRY_DELAY_MS: u64 = 250;

fn active_session_condition_sql(alias: &str) -> String {
    format!("{alias}.is_active = 1 AND {alias}.created_at > unixepoch() - {SESSION_MAX_AGE_SECS}")
}

fn effective_session_is_active_sql(alias: &str) -> String {
    format!(
        "CASE WHEN {} THEN 1 ELSE 0 END",
        active_session_condition_sql(alias)
    )
}

fn effective_session_stopped_at_sql(alias: &str) -> String {
    format!(
        "CASE WHEN {alias}.is_active = 1 AND {alias}.created_at <= unixepoch() - {SESSION_MAX_AGE_SECS} \
         THEN COALESCE({alias}.stopped_at, {alias}.created_at + {SESSION_MAX_AGE_SECS}) \
         ELSE {alias}.stopped_at END"
    )
}

fn effective_session_donations_enabled_sql(alias: &str) -> String {
    format!(
        "CASE WHEN {} THEN {alias}.donations_enabled ELSE 0 END",
        active_session_condition_sql(alias)
    )
}

fn effective_session_donations_enabled_at_sql(alias: &str) -> String {
    format!(
        "CASE WHEN {} THEN {alias}.donations_enabled_at ELSE NULL END",
        active_session_condition_sql(alias)
    )
}

fn effective_session_donations_min_external_id_sql(alias: &str) -> String {
    format!(
        "CASE WHEN {} THEN {alias}.donations_min_external_id ELSE NULL END",
        active_session_condition_sql(alias)
    )
}

fn stream_session_select_sql(alias: &str) -> String {
    format!(
        r#"
            {alias}.id,
            {alias}.owner_user_id,
            {alias}.public_code,
            {alias}.created_at,
            {is_active} AS is_active,
            {alias}.name,
            {alias}.description,
            {alias}.stream_link,
            {stopped_at} AS stopped_at,
            {alias}.downvote_threshold,
            {donations_enabled} AS donations_enabled,
            {donations_enabled_at} AS donations_enabled_at,
            {donations_min_external_id} AS donations_min_external_id
        "#,
        alias = alias,
        is_active = effective_session_is_active_sql(alias),
        stopped_at = effective_session_stopped_at_sql(alias),
        donations_enabled = effective_session_donations_enabled_sql(alias),
        donations_enabled_at = effective_session_donations_enabled_at_sql(alias),
        donations_min_external_id = effective_session_donations_min_external_id_sql(alias),
    )
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
    donation_id: Option<i64>,
}

impl SessionEvent {
    fn question_created(question_id: i64) -> Self {
        Self {
            kind: "question_created",
            question_id: Some(question_id),
            donation_id: None,
        }
    }

    fn question_changed(question_id: i64) -> Self {
        Self {
            kind: "question_changed",
            question_id: Some(question_id),
            donation_id: None,
        }
    }

    fn question_deleted(question_id: i64) -> Self {
        Self {
            kind: "question_deleted",
            question_id: Some(question_id),
            donation_id: None,
        }
    }

    fn donation_created(donation_id: i64) -> Self {
        Self {
            kind: "donation_created",
            question_id: None,
            donation_id: Some(donation_id),
        }
    }

    fn donation_changed(donation_id: i64) -> Self {
        Self {
            kind: "donation_changed",
            question_id: None,
            donation_id: Some(donation_id),
        }
    }

    fn donation_deleted(donation_id: i64) -> Self {
        Self {
            kind: "donation_deleted",
            question_id: None,
            donation_id: Some(donation_id),
        }
    }

    fn resync() -> Self {
        Self {
            kind: "resync",
            question_id: None,
            donation_id: None,
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
struct DonationAlertsOAuthStartQuery {
    return_to: Option<String>,
    auth_token: Option<String>,
}

#[derive(Deserialize, Default)]
struct GoogleOAuthCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct DonationAlertsOAuthCallbackQuery {
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
    donations_enabled: i64,
    donations_enabled_at: Option<i64>,
    donations_min_external_id: Option<i64>,
}

#[derive(Deserialize)]
struct UpdateSessionRequest {
    name: String,
    description: Option<String>,
    stream_link: Option<String>,
    downvote_threshold: Option<i64>,
    donations_enabled: Option<bool>,
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

#[derive(Serialize, FromRow)]
struct FeedItemView {
    id: i64,
    kind: String,
    question_id: Option<i64>,
    donation_id: Option<i64>,
    session_id: i64,
    author_user_id: Option<i64>,
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
    donation_amount_minor: Option<i64>,
    donation_currency: Option<String>,
    donation_usd_cents: Option<i64>,
    donation_external_id: Option<i64>,
}

#[derive(Serialize, FromRow)]
struct DonationView {
    id: i64,
    session_id: i64,
    owner_user_id: i64,
    external_donation_id: i64,
    donor_name: String,
    body: String,
    amount_minor: i64,
    currency: String,
    usd_cents: i64,
    is_answering: i64,
    is_answered: i64,
    is_rejected: i64,
    is_deleted: i64,
    created_at: i64,
    answering_started_at: Option<i64>,
    answered_at: Option<i64>,
}

#[derive(FromRow)]
struct AuthUser {
    user_id: i64,
    last_seen_at: i64,
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

#[derive(FromRow)]
struct DonationAdminMeta {
    session_id: i64,
    owner_user_id: i64,
    session_is_active: i64,
}

#[derive(FromRow)]
struct SessionAnsweringConflicts {
    has_question: i64,
    has_donation: i64,
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
    questions: Vec<FeedItemView>,
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

#[derive(Deserialize)]
struct ModerateDonationRequest {
    action: ModerateDonationAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ModerateDonationAction {
    Answer,
    FinishAnswering,
    Reject,
    Reopen,
    Restore,
    Delete,
}

#[derive(Serialize)]
struct ModerateDonationResponse {
    donation_id: i64,
    deleted: bool,
    donation: Option<DonationView>,
}

#[derive(Serialize, FromRow)]
struct DonationIntegrationStatus {
    connected: i64,
    da_user_id: Option<i64>,
    scope: Option<String>,
    token_expires_at: Option<i64>,
    last_seen_external_id: Option<i64>,
    last_sync_at: Option<i64>,
    last_error: Option<String>,
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

#[derive(Debug, Deserialize)]
struct DonationAlertsTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DonationAlertsUserOauthResponse {
    data: DonationAlertsUserOauthData,
}

#[derive(Debug, Deserialize)]
struct DonationAlertsUserOauthData {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct DonationAlertsDonationsResponse {
    data: Vec<DonationAlertsDonation>,
}

#[derive(Debug, Deserialize)]
struct DonationAlertsDonation {
    id: i64,
    username: Option<String>,
    message: Option<String>,
    amount: f64,
    currency: String,
    created_at: Option<String>,
}

#[derive(Clone, FromRow)]
struct DaIntegrationAuthRow {
    owner_user_id: i64,
    da_user_id: i64,
    access_token: String,
    refresh_token: String,
    token_expires_at: i64,
    scope: String,
    last_seen_external_id: Option<i64>,
    last_sync_at: Option<i64>,
    last_error: Option<String>,
}

#[derive(FromRow)]
struct DonationSyncSessionRow {
    session_id: i64,
    min_external_id: Option<i64>,
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
    let da_client_id = env::var("DA_CLIENT_ID").ok();
    let da_client_secret = env::var("DA_CLIENT_SECRET").ok();
    let da_redirect_uri = env::var("DA_REDIRECT_URI").ok().or_else(|| {
        Some(format!(
            "{}/api/da/oauth/callback",
            public_base_url.trim_end_matches('/')
        ))
    });
    let reset_db_on_boot = env::var("RESET_DB_ON_BOOT")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);

    tracing::info!(
        db = %db_url,
        addr = %app_addr,
        frontend_origin = %frontend_origin,
        google_oauth_configured = google_client_id.is_some() && google_client_secret.is_some() && google_redirect_uri.is_some(),
        da_oauth_configured = da_client_id.is_some() && da_client_secret.is_some() && da_redirect_uri.is_some(),
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
        da_client_id,
        da_client_secret,
        da_redirect_uri,
        public_base_url,
        session_events,
        sse_connections: Arc::new(Mutex::new(HashMap::new())),
    };

    expire_stale_sessions_once(&state).await?;

    {
        let state_for_expiry = state.clone();
        tokio::spawn(async move {
            expire_stale_sessions_loop(state_for_expiry).await;
        });
    }

    {
        let state_for_sync = state.clone();
        tokio::spawn(async move {
            donation_sync_loop(state_for_sync).await;
        });
    }

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
        .route("/api/da/oauth/start", get(da_oauth_start))
        .route("/api/da/oauth/callback", get(da_oauth_callback))
        .route(
            "/api/da/integration",
            get(get_da_integration_status).delete(disconnect_da_integration),
        )
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
        .route("/api/donations/:id/moderate", post(moderate_donation))
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

    run_db_migrations(db).await?;

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

async fn da_oauth_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DonationAlertsOAuthStartQuery>,
) -> Result<Redirect, AppError> {
    let auth = match require_auth_user(&state, &headers).await {
        Ok(auth) => auth,
        Err(err) if err.status == StatusCode::UNAUTHORIZED => {
            if let Some(token) = query.auth_token.as_deref() {
                require_auth_user_by_token(&state, token).await?
            } else {
                return Err(err);
            }
        }
        Err(err) => return Err(err),
    };
    let client_id = state
        .da_client_id
        .as_deref()
        .ok_or_else(|| AppError::internal("DA_CLIENT_ID is not configured"))?;
    let redirect_uri = state
        .da_redirect_uri
        .as_deref()
        .ok_or_else(|| AppError::internal("DA_REDIRECT_URI is not configured"))?;

    let return_to = sanitize_return_to(query.return_to.as_deref());
    let state_token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());

    sqlx::query("DELETE FROM da_oauth_states WHERE expires_at <= unixepoch();")
        .execute(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to cleanup da oauth states: {err}")))?;

    sqlx::query(
        r#"
        INSERT INTO da_oauth_states (state, user_id, return_to, created_at, expires_at)
        VALUES (?1, ?2, ?3, unixepoch(), unixepoch() + 600);
        "#,
    )
    .bind(&state_token)
    .bind(auth.user_id)
    .bind(&return_to)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to create da oauth state: {err}")))?;

    let mut auth_url = reqwest::Url::parse("https://www.donationalerts.com/oauth/authorize")
        .map_err(|err| AppError::internal(format!("failed to build da oauth url: {err}")))?;
    auth_url
        .query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair(
            "scope",
            "oauth-user-show oauth-donation-index oauth-donation-subscribe",
        )
        .append_pair("state", &state_token);

    Ok(Redirect::to(auth_url.as_ref()))
}

async fn da_oauth_callback(
    State(state): State<AppState>,
    Query(query): Query<DonationAlertsOAuthCallbackQuery>,
) -> Result<Redirect, AppError> {
    let client_id = state
        .da_client_id
        .as_deref()
        .ok_or_else(|| AppError::internal("DA_CLIENT_ID is not configured"))?;
    let client_secret = state
        .da_client_secret
        .as_deref()
        .ok_or_else(|| AppError::internal("DA_CLIENT_SECRET is not configured"))?;
    let redirect_uri = state
        .da_redirect_uri
        .as_deref()
        .ok_or_else(|| AppError::internal("DA_REDIRECT_URI is not configured"))?;

    let Some(state_token) = query.state.as_deref() else {
        return Ok(Redirect::to("/#da_oauth_error=missing_state"));
    };

    let (owner_user_id, return_to) = match consume_da_oauth_state(&state.db, state_token).await {
        Ok(payload) => payload,
        Err(_) => return Ok(Redirect::to("/#da_oauth_error=invalid_state")),
    };

    if let Some(error) = query.error.as_deref() {
        let safe = if error.is_empty() {
            "oauth_denied"
        } else {
            "oauth_provider_error"
        };
        tracing::warn!(da_error = %error, "donationalerts oauth callback returned error");
        return Ok(redirect_with_fragment(
            &return_to,
            "da_oauth_error",
            Some(safe),
        ));
    }

    let Some(code) = query.code.as_deref() else {
        return Ok(redirect_with_fragment(
            &return_to,
            "da_oauth_error",
            Some("missing_authorization_code"),
        ));
    };

    let token_response = state
        .http
        .post("https://www.donationalerts.com/oauth/token")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", redirect_uri),
            ("code", code),
        ])
        .send()
        .await
        .map_err(|err| {
            AppError::internal(format!("donationalerts token exchange failed: {err}"))
        })?;

    if !token_response.status().is_success() {
        tracing::warn!(
            status = %token_response.status(),
            "donationalerts token exchange returned non-success"
        );
        return Ok(redirect_with_fragment(
            &return_to,
            "da_oauth_error",
            Some("oauth_token_exchange_failed"),
        ));
    }

    let token_payload: DonationAlertsTokenResponse =
        token_response.json().await.map_err(|err| {
            AppError::internal(format!("invalid donationalerts token response: {err}"))
        })?;

    let userinfo_response = state
        .http
        .get("https://www.donationalerts.com/api/v1/user/oauth")
        .bearer_auth(&token_payload.access_token)
        .send()
        .await
        .map_err(|err| AppError::internal(format!("donationalerts user request failed: {err}")))?;

    if !userinfo_response.status().is_success() {
        tracing::warn!(
            status = %userinfo_response.status(),
            "donationalerts user oauth returned non-success"
        );
        return Ok(redirect_with_fragment(
            &return_to,
            "da_oauth_error",
            Some("oauth_userinfo_failed"),
        ));
    }

    let da_user_payload: DonationAlertsUserOauthResponse =
        userinfo_response.json().await.map_err(|err| {
            AppError::internal(format!("invalid donationalerts user response: {err}"))
        })?;

    let refresh_token = token_payload
        .refresh_token
        .as_deref()
        .ok_or_else(|| AppError::internal("donationalerts token response missing refresh_token"))?;
    let token_expires_at = now_unix() + token_payload.expires_in.unwrap_or(3600).max(60) - 30;
    let scope = token_payload.scope.unwrap_or_else(|| {
        "oauth-user-show oauth-donation-index oauth-donation-subscribe".to_string()
    });

    sqlx::query(
        r#"
        INSERT INTO da_integrations (
            owner_user_id, da_user_id, access_token, refresh_token, token_expires_at, scope,
            last_seen_external_id, last_sync_at, last_error, created_at, updated_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, unixepoch(), unixepoch())
        ON CONFLICT(owner_user_id) DO UPDATE
        SET da_user_id = excluded.da_user_id,
            access_token = excluded.access_token,
            refresh_token = excluded.refresh_token,
            token_expires_at = excluded.token_expires_at,
            scope = excluded.scope,
            last_error = NULL,
            updated_at = unixepoch();
        "#,
    )
    .bind(owner_user_id)
    .bind(da_user_payload.data.id)
    .bind(&token_payload.access_token)
    .bind(refresh_token)
    .bind(token_expires_at)
    .bind(&scope)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to upsert da integration: {err}")))?;

    let return_to_session_code = session_code_from_return_to(&return_to);
    let target_session_id = if let Some(session_code) = return_to_session_code.as_deref() {
        let query = format!(
            r#"
            SELECT id
            FROM stream_sessions s
            WHERE s.owner_user_id = ?1
              AND s.public_code = ?2
              AND {}
            LIMIT 1;
            "#,
            active_session_condition_sql("s")
        );
        sqlx::query_scalar::<_, i64>(&query)
            .bind(owner_user_id)
            .bind(session_code)
            .fetch_optional(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!(
                    "failed to resolve target session for da oauth: {err}"
                ))
            })?
    } else {
        None
    };

    if return_to_session_code.is_some() && target_session_id.is_none() {
        tracing::warn!(
            owner_user_id,
            da_user_id = da_user_payload.data.id,
            return_to_session_code = ?return_to_session_code,
            "da oauth callback referenced a session code, but no active target session was found"
        );
    }

    let fallback_external_id: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(last_seen_external_id, 0)
        FROM da_integrations
        WHERE owner_user_id = ?1
        LIMIT 1;
        "#,
    )
    .bind(owner_user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to read donation baseline fallback: {err}")))?
    .unwrap_or(0)
    .max(0);

    let baseline_external_id =
        match fetch_latest_donation_external_id(&state.http, &token_payload.access_token).await {
            Ok(latest) => latest.max(fallback_external_id),
            Err(err) => {
                tracing::warn!(
                    owner_user_id,
                    "failed to fetch donation baseline after da oauth connect: {err}"
                );
                fallback_external_id
            }
        };

    if let Some(session_id) = target_session_id {
        let query = format!(
            r#"
            UPDATE stream_sessions
            SET donations_enabled = CASE WHEN id = ?3 THEN 1 ELSE 0 END,
                donations_enabled_at = CASE
                    WHEN id = ?3 THEN COALESCE(donations_enabled_at, unixepoch())
                    ELSE NULL
                END,
                donations_min_external_id = CASE
                    WHEN id = ?3 THEN CASE
                        WHEN donations_min_external_id IS NULL THEN ?2
                        WHEN donations_min_external_id < ?2 THEN ?2
                        ELSE donations_min_external_id
                    END
                    ELSE NULL
                END
            WHERE owner_user_id = ?1
              AND {};
            "#,
            active_session_condition_sql("stream_sessions")
        );
        sqlx::query(&query)
            .bind(owner_user_id)
            .bind(baseline_external_id)
            .bind(session_id)
            .execute(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!(
                    "failed to bind da integration to target session: {err}"
                ))
            })?;
    } else {
        let query = format!(
            r#"
            UPDATE stream_sessions
            SET donations_enabled_at = COALESCE(donations_enabled_at, unixepoch()),
                donations_min_external_id = CASE
                    WHEN donations_min_external_id IS NULL THEN ?2
                    WHEN donations_min_external_id < ?2 THEN ?2
                    ELSE donations_min_external_id
                END
            WHERE owner_user_id = ?1
              AND {}
              AND donations_enabled = 1;
            "#,
            active_session_condition_sql("stream_sessions")
        );
        sqlx::query(&query)
            .bind(owner_user_id)
            .bind(baseline_external_id)
            .execute(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!("failed to align donation baseline: {err}"))
            })?;
    }

    let active_sessions_query = format!(
        r#"
        SELECT id
        FROM stream_sessions s
        WHERE s.owner_user_id = ?1
          AND {};
        "#,
        active_session_condition_sql("s")
    );
    let active_session_ids = sqlx::query_scalar::<_, i64>(&active_sessions_query)
        .bind(owner_user_id)
        .fetch_all(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to list active sessions: {err}")))?;
    let active_session_ids_for_log = active_session_ids.clone();
    for session_id in active_session_ids {
        state
            .session_events
            .publish(session_id, SessionEvent::resync())
            .await;
    }

    tracing::info!(
        owner_user_id,
        da_user_id = da_user_payload.data.id,
        return_to_session_code = ?return_to_session_code,
        target_session_id = ?target_session_id,
        baseline_external_id,
        active_session_ids = ?active_session_ids_for_log,
        routing_mode = if target_session_id.is_some() {
            "bind_target_session"
        } else {
            "align_existing_routing"
        },
        "donationalerts oauth connected and donation routing resolved"
    );

    Ok(redirect_with_fragment(&return_to, "da_oauth", Some("ok")))
}

async fn get_da_integration_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DonationIntegrationStatus>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    let status = sqlx::query_as::<_, DonationIntegrationStatus>(
        r#"
        SELECT
            1 AS connected,
            da_user_id,
            scope,
            token_expires_at,
            last_seen_external_id,
            last_sync_at,
            last_error
        FROM da_integrations
        WHERE owner_user_id = ?1
        LIMIT 1;
        "#,
    )
    .bind(auth.user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to load da integration: {err}")))?
    .unwrap_or(DonationIntegrationStatus {
        connected: 0,
        da_user_id: None,
        scope: None,
        token_expires_at: None,
        last_seen_external_id: None,
        last_sync_at: None,
        last_error: None,
    });

    Ok(Json(status))
}

async fn disconnect_da_integration(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    let session_ids = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT id
        FROM stream_sessions
        WHERE owner_user_id = ?1;
        "#,
    )
    .bind(auth.user_id)
    .fetch_all(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to list owner sessions: {err}")))?;

    sqlx::query("DELETE FROM da_integrations WHERE owner_user_id = ?1;")
        .bind(auth.user_id)
        .execute(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to disconnect da integration: {err}")))?;

    sqlx::query("DELETE FROM donations WHERE owner_user_id = ?1;")
        .bind(auth.user_id)
        .execute(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to delete donation history: {err}")))?;

    sqlx::query(
        r#"
        UPDATE stream_sessions
        SET donations_enabled = 0,
            donations_enabled_at = NULL,
            donations_min_external_id = NULL
        WHERE owner_user_id = ?1;
        "#,
    )
    .bind(auth.user_id)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to reset donation session state: {err}")))?;

    for session_id in session_ids {
        state
            .session_events
            .publish(session_id, SessionEvent::resync())
            .await;
    }

    tracing::info!(
        owner_user_id = auth.user_id,
        "donationalerts integration disconnected"
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn list_user_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ListUserSessionsResponse>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    let query = format!(
        r#"
        SELECT
            {}
        FROM stream_sessions s
        WHERE s.owner_user_id = ?1
        ORDER BY s.created_at DESC;
        "#,
        stream_session_select_sql("s")
    );
    let sessions = sqlx::query_as::<_, StreamSession>(&query)
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

    let has_active_donation_session_query = format!(
        r#"
        SELECT id
        FROM stream_sessions s
        WHERE s.owner_user_id = ?1
          AND {}
          AND s.donations_enabled = 1
        LIMIT 1;
        "#,
        active_session_condition_sql("s")
    );
    let has_active_donation_session: Option<i64> =
        sqlx::query_scalar(&has_active_donation_session_query)
            .bind(auth.user_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!("failed to check donation-enabled sessions: {err}"))
            })?;

    let active_session_ids_query = format!(
        r#"
        SELECT s.id
        FROM stream_sessions s
        WHERE s.owner_user_id = ?1
          AND {}
        ORDER BY s.created_at DESC;
        "#,
        active_session_condition_sql("s")
    );
    let active_session_ids = sqlx::query_scalar::<_, i64>(&active_session_ids_query)
        .bind(auth.user_id)
        .fetch_all(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to list active sessions: {err}")))?;

    let donations_enabled_default = if has_active_donation_session.is_some() {
        0
    } else {
        1
    };
    let (donations_enabled_at_default, donations_min_external_id_default) =
        if donations_enabled_default == 1 {
            let baseline = match resolve_donation_baseline_for_owner(&state, auth.user_id).await {
                Ok(value) => value.max(0),
                Err(err) => {
                    tracing::warn!(
                        user_id = auth.user_id,
                        error = ?err,
                        "failed to resolve donation baseline for new session, falling back to 0"
                    );
                    0
                }
            };
            (Some(now_unix()), Some(baseline))
        } else {
            (None, None)
        };

    let mut inserted_id: Option<i64> = None;
    for _ in 0..8 {
        let public_code = generate_session_code();
        let res = sqlx::query(
            r#"
            INSERT INTO stream_sessions (
                owner_user_id,
                public_code,
                created_at,
                is_active,
                name,
                description,
                stream_link,
                downvote_threshold,
                donations_enabled,
                donations_enabled_at,
                donations_min_external_id
            )
            VALUES (?1, ?2, unixepoch(), 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9);
            "#,
        )
        .bind(auth.user_id)
        .bind(&public_code)
        .bind(&name)
        .bind(&description)
        .bind(&stream_link)
        .bind(downvote_threshold)
        .bind(donations_enabled_default)
        .bind(donations_enabled_at_default)
        .bind(donations_min_external_id_default)
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

    let query = format!(
        r#"
        SELECT
            {}
        FROM stream_sessions s
        WHERE s.id = ?1;
        "#,
        stream_session_select_sql("s")
    );
    let session = sqlx::query_as::<_, StreamSession>(&query)
        .bind(row_id)
        .fetch_one(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to fetch new session: {err}")))?;

    tracing::info!(
        owner_user_id = auth.user_id,
        session_id = session.id,
        code = %session.public_code,
        %name,
        active_session_ids = ?active_session_ids,
        active_donation_session_id = ?has_active_donation_session,
        donations_enabled_default,
        donations_min_external_id_default = ?donations_min_external_id_default,
        "session created"
    );
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

    let questions = list_feed_items_for_session(
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
        tracing::debug!(
            ip = %ip,
            session_id = session.id,
            code = %code,
            connections = *count,
            "SSE client connected"
        );
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
        serde_json::to_string(&SessionEvent::resync()).unwrap_or_else(|_| {
            r#"{"kind":"resync","question_id":null,"donation_id":null}"#.to_string()
        }),
    ))]);
    let stream = initial_stream.chain(live_stream);

    // Wrap stream to decrement connection count on drop
    let stream = SseDropGuard {
        inner: Box::pin(stream),
        sse_connections,
        ip,
        session_id: session.id,
        code: code.clone(),
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
    session_id: i64,
    code: String,
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
            let session_id = self.session_id;
            let code = self.code.clone();
            tokio::spawn(async move {
                let mut conns = conns.lock().await;
                if let Some(count) = conns.get_mut(&ip) {
                    *count = count.saturating_sub(1);
                    let remaining = *count;
                    if remaining == 0 {
                        conns.remove(&ip);
                    }
                    tracing::debug!(
                        ip = %ip,
                        session_id,
                        code = %code,
                        connections = remaining,
                        "SSE client disconnected"
                    );
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

    let question_meta_sql = format!(
        r#"
        SELECT q.session_id, s.owner_user_id, {} AS session_is_active,
               q.status
        FROM questions q
        JOIN stream_sessions s ON s.id = q.session_id
        WHERE q.id = ?1;
        "#,
        effective_session_is_active_sql("s")
    );
    let question_meta_row = sqlx::query_as::<_, QuestionVoteMetaRow>(&question_meta_sql)
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

    let question_admin_meta_sql = format!(
        r#"
        SELECT s.id AS session_id,
               s.owner_user_id,
               q.author_user_id,
               {} AS session_is_active,
               s.name AS session_name,
               q.body AS question_body
        FROM questions q
        JOIN stream_sessions s ON s.id = q.session_id
        WHERE q.id = ?1
        LIMIT 1;
        "#,
        effective_session_is_active_sql("s")
    );
    let meta = sqlx::query_as::<_, QuestionAdminMeta>(&question_admin_meta_sql)
        .bind(question_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|err| {
            AppError::internal(format!("failed to load question admin metadata: {err}"))
        })?
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
                WHERE id = ?1
                  AND status IN (?3, ?4)
                  AND NOT EXISTS (
                      SELECT 1
                      FROM questions other
                      WHERE other.session_id = ?5
                        AND other.status = ?2
                        AND other.id != ?1
                  )
                  AND NOT EXISTS (
                      SELECT 1
                      FROM donations other
                      WHERE other.session_id = ?5
                        AND other.status = ?2
                  );
                "#,
            )
            .bind(question_id)
            .bind(QuestionStatus::Answering.as_str())
            .bind(QuestionStatus::New.as_str())
            .bind(QuestionStatus::Rejected.as_str())
            .bind(meta.session_id)
            .execute(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!("failed to mark question as in-progress: {err}"))
            })?;

            if update.rows_affected() == 0 {
                if let Some(message) = current_answering_conflict_message(
                    &state.db,
                    meta.session_id,
                    Some(question_id),
                    None,
                )
                .await?
                {
                    return Err(AppError::bad_request(message));
                }

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

async fn moderate_donation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(donation_id): Path<i64>,
    Json(payload): Json<ModerateDonationRequest>,
) -> Result<Json<ModerateDonationResponse>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    let donation_admin_meta_sql = format!(
        r#"
        SELECT
            s.id AS session_id,
            s.owner_user_id,
            {} AS session_is_active
        FROM donations d
        JOIN stream_sessions s ON s.id = d.session_id
        WHERE d.id = ?1
        LIMIT 1;
        "#,
        effective_session_is_active_sql("s")
    );
    let meta = sqlx::query_as::<_, DonationAdminMeta>(&donation_admin_meta_sql)
        .bind(donation_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to load donation metadata: {err}")))?
        .ok_or_else(|| AppError::not_found("donation not found"))?;

    if meta.owner_user_id != auth.user_id {
        return Err(AppError::forbidden(
            "only session owner can moderate donations",
        ));
    }

    let action = payload.action;
    if meta.session_is_active == 0
        && matches!(
            action,
            ModerateDonationAction::Answer
                | ModerateDonationAction::FinishAnswering
                | ModerateDonationAction::Reject
                | ModerateDonationAction::Reopen
        )
    {
        return Err(AppError::forbidden(
            "session is stopped: only delete is allowed",
        ));
    }

    match action {
        ModerateDonationAction::Answer => {
            let update = sqlx::query(
                r#"
                UPDATE donations
                SET status = ?2,
                    answering_started_at = unixepoch(),
                    answered_at = NULL
                WHERE id = ?1
                  AND status IN (?3, ?4)
                  AND NOT EXISTS (
                      SELECT 1
                      FROM donations other
                      WHERE other.session_id = ?5
                        AND other.status = ?2
                        AND other.id != ?1
                  )
                  AND NOT EXISTS (
                      SELECT 1
                      FROM questions other
                      WHERE other.session_id = ?5
                        AND other.status = ?2
                  );
                "#,
            )
            .bind(donation_id)
            .bind(QuestionStatus::Answering.as_str())
            .bind(QuestionStatus::New.as_str())
            .bind(QuestionStatus::Rejected.as_str())
            .bind(meta.session_id)
            .execute(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!("failed to mark donation answering: {err}"))
            })?;

            if update.rows_affected() == 0 {
                if let Some(message) = current_answering_conflict_message(
                    &state.db,
                    meta.session_id,
                    None,
                    Some(donation_id),
                )
                .await?
                {
                    return Err(AppError::bad_request(message));
                }

                return Err(AppError::bad_request("cannot mark donation as in-progress"));
            }

            let donation = fetch_donation_by_id(&state.db, donation_id).await?;
            state
                .session_events
                .publish(meta.session_id, SessionEvent::donation_changed(donation_id))
                .await;

            Ok(Json(ModerateDonationResponse {
                donation_id,
                deleted: false,
                donation: Some(donation),
            }))
        }
        ModerateDonationAction::FinishAnswering => {
            let update = sqlx::query(
                r#"
                UPDATE donations
                SET status = ?2,
                    answered_at = unixepoch()
                WHERE id = ?1 AND status = ?3;
                "#,
            )
            .bind(donation_id)
            .bind(QuestionStatus::Answered.as_str())
            .bind(QuestionStatus::Answering.as_str())
            .execute(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!("failed to mark donation as answered: {err}"))
            })?;

            if update.rows_affected() == 0 {
                return Err(AppError::bad_request(
                    "donation is not in progress of answering",
                ));
            }

            let donation = fetch_donation_by_id(&state.db, donation_id).await?;
            state
                .session_events
                .publish(meta.session_id, SessionEvent::donation_changed(donation_id))
                .await;

            Ok(Json(ModerateDonationResponse {
                donation_id,
                deleted: false,
                donation: Some(donation),
            }))
        }
        ModerateDonationAction::Reject => {
            let update = sqlx::query(
                r#"
                UPDATE donations
                SET status = ?2,
                    answering_started_at = NULL,
                    answered_at = NULL
                WHERE id = ?1 AND status IN (?3, ?4);
                "#,
            )
            .bind(donation_id)
            .bind(QuestionStatus::Rejected.as_str())
            .bind(QuestionStatus::New.as_str())
            .bind(QuestionStatus::Answering.as_str())
            .execute(&state.db)
            .await
            .map_err(|err| AppError::internal(format!("failed to reject donation: {err}")))?;

            if update.rows_affected() == 0 {
                return Err(AppError::bad_request(
                    "cannot reject: donation is already answered or rejected",
                ));
            }

            let donation = fetch_donation_by_id(&state.db, donation_id).await?;
            state
                .session_events
                .publish(meta.session_id, SessionEvent::donation_changed(donation_id))
                .await;

            Ok(Json(ModerateDonationResponse {
                donation_id,
                deleted: false,
                donation: Some(donation),
            }))
        }
        ModerateDonationAction::Delete => {
            sqlx::query(
                r#"
                UPDATE donations
                SET status = ?2
                WHERE id = ?1 AND status != ?2;
                "#,
            )
            .bind(donation_id)
            .bind(QuestionStatus::Deleted.as_str())
            .execute(&state.db)
            .await
            .map_err(|err| AppError::internal(format!("failed to delete donation: {err}")))?;

            state
                .session_events
                .publish(meta.session_id, SessionEvent::donation_deleted(donation_id))
                .await;

            Ok(Json(ModerateDonationResponse {
                donation_id,
                deleted: true,
                donation: None,
            }))
        }
        ModerateDonationAction::Restore => {
            let update = sqlx::query(
                r#"
                UPDATE donations
                SET status = ?2,
                    answering_started_at = NULL,
                    answered_at = NULL
                WHERE id = ?1 AND status = ?3;
                "#,
            )
            .bind(donation_id)
            .bind(QuestionStatus::New.as_str())
            .bind(QuestionStatus::Deleted.as_str())
            .execute(&state.db)
            .await
            .map_err(|err| AppError::internal(format!("failed to restore donation: {err}")))?;

            if update.rows_affected() == 0 {
                return Err(AppError::bad_request("donation is not deleted"));
            }

            let donation = fetch_donation_by_id(&state.db, donation_id).await?;
            state
                .session_events
                .publish(meta.session_id, SessionEvent::donation_changed(donation_id))
                .await;

            Ok(Json(ModerateDonationResponse {
                donation_id,
                deleted: false,
                donation: Some(donation),
            }))
        }
        ModerateDonationAction::Reopen => {
            let update = sqlx::query(
                r#"
                UPDATE donations
                SET status = ?2,
                    answering_started_at = NULL,
                    answered_at = NULL
                WHERE id = ?1 AND status IN (?3, ?4);
                "#,
            )
            .bind(donation_id)
            .bind(QuestionStatus::New.as_str())
            .bind(QuestionStatus::Answering.as_str())
            .bind(QuestionStatus::Answered.as_str())
            .execute(&state.db)
            .await
            .map_err(|err| AppError::internal(format!("failed to reopen donation: {err}")))?;

            if update.rows_affected() == 0 {
                return Err(AppError::bad_request(
                    "donation is not in answering or answered state",
                ));
            }

            let donation = fetch_donation_by_id(&state.db, donation_id).await?;
            state
                .session_events
                .publish(meta.session_id, SessionEvent::donation_changed(donation_id))
                .await;

            Ok(Json(ModerateDonationResponse {
                donation_id,
                deleted: false,
                donation: Some(donation),
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
        "UPDATE stream_sessions
         SET is_active = 0,
             stopped_at = unixepoch(),
             donations_enabled = 0,
             donations_enabled_at = NULL,
             donations_min_external_id = NULL
         WHERE id = ?1;",
    )
    .bind(session.id)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to stop session: {err}")))?;

    tracing::info!(
        owner_user_id = auth.user_id,
        session_id = session.id,
        code = %code,
        donations_enabled_before = session.donations_enabled,
        stop_reason = "manual",
        "session stopped"
    );
    state
        .session_events
        .publish(session.id, SessionEvent::resync())
        .await;

    let updated = find_session_by_code(&state.db, &code).await?;
    Ok(Json(updated))
}

async fn expire_stale_sessions_once(state: &AppState) -> anyhow::Result<()> {
    let expired_sessions = sqlx::query_as::<_, (i64, i64, String)>(
        r#"
        SELECT id, owner_user_id, public_code
        FROM stream_sessions
        WHERE is_active = 1
          AND created_at <= unixepoch() - ?1;
        "#,
    )
    .bind(SESSION_MAX_AGE_SECS)
    .fetch_all(&state.db)
    .await?;

    if expired_sessions.is_empty() {
        return Ok(());
    }

    let expired_session_ids: Vec<i64> = expired_sessions.iter().map(|(id, _, _)| *id).collect();

    sqlx::query(
        r#"
        UPDATE stream_sessions
        SET is_active = 0,
            stopped_at = COALESCE(stopped_at, created_at + ?1),
            donations_enabled = 0,
            donations_enabled_at = NULL,
            donations_min_external_id = NULL
        WHERE is_active = 1
          AND created_at <= unixepoch() - ?1;
        "#,
    )
    .bind(SESSION_MAX_AGE_SECS)
    .execute(&state.db)
    .await?;

    for session_id in &expired_session_ids {
        state
            .session_events
            .publish(*session_id, SessionEvent::resync())
            .await;
    }

    tracing::info!(
        count = expired_session_ids.len(),
        expired_sessions = ?expired_sessions,
        stop_reason = "max_duration_reached",
        "auto-closed stream sessions older than 48 hours"
    );

    Ok(())
}

async fn expire_stale_sessions_loop(state: AppState) {
    loop {
        if let Err(err) = expire_stale_sessions_once(&state).await {
            tracing::warn!("failed to auto-close stale sessions: {err}");
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
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

    let mut donations_enabled = session.donations_enabled;
    let mut donations_enabled_at = session.donations_enabled_at;
    let mut donations_min_external_id = session.donations_min_external_id;
    let mut donations_routing_changed = false;
    let previous_active_donation_session_ids = if payload.donations_enabled.is_some() {
        let query = format!(
            r#"
            SELECT s.id
            FROM stream_sessions s
            WHERE s.owner_user_id = ?1
              AND {}
              AND s.donations_enabled = 1
            ORDER BY COALESCE(s.donations_enabled_at, s.created_at) DESC;
            "#,
            active_session_condition_sql("s")
        );
        sqlx::query_scalar::<_, i64>(&query)
            .bind(auth.user_id)
            .fetch_all(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!("failed to list existing donation routing: {err}"))
            })?
    } else {
        Vec::new()
    };

    if let Some(enable_donations) = payload.donations_enabled {
        let next_value = if enable_donations { 1 } else { 0 };
        if next_value != session.donations_enabled {
            if next_value == 1 {
                if session.is_active == 0 {
                    return Err(AppError::forbidden(
                        "cannot enable donations for a stopped session",
                    ));
                }

                let baseline_external_id: i64 = sqlx::query_scalar(
                    r#"
                    SELECT COALESCE(last_seen_external_id, 0)
                    FROM da_integrations
                    WHERE owner_user_id = ?1
                    LIMIT 1;
                    "#,
                )
                .bind(auth.user_id)
                .fetch_optional(&state.db)
                .await
                .map_err(|err| {
                    AppError::internal(format!("failed to resolve donation baseline: {err}"))
                })?
                .unwrap_or(0)
                .max(0);
                donations_enabled = 1;
                donations_enabled_at = Some(now_unix());
                donations_min_external_id = Some(baseline_external_id.max(0));
            } else {
                donations_enabled = 0;
                donations_enabled_at = None;
                donations_min_external_id = None;
            }

            donations_routing_changed = true;
        }
    }

    if donations_routing_changed && donations_enabled == 1 {
        sqlx::query(
            r#"
            UPDATE stream_sessions
            SET donations_enabled = 0,
                donations_enabled_at = NULL,
                donations_min_external_id = NULL
            WHERE owner_user_id = ?1
              AND id != ?2;
            "#,
        )
        .bind(auth.user_id)
        .bind(session.id)
        .execute(&state.db)
        .await
        .map_err(|err| {
            AppError::internal(format!(
                "failed to disable donations for other sessions: {err}"
            ))
        })?;
    }

    sqlx::query(
        r#"
        UPDATE stream_sessions
        SET name = ?1,
            description = ?2,
            stream_link = ?3,
            downvote_threshold = ?4,
            donations_enabled = ?5,
            donations_enabled_at = ?6,
            donations_min_external_id = ?7
        WHERE id = ?8;
        "#,
    )
    .bind(&name)
    .bind(&description)
    .bind(&stream_link)
    .bind(downvote_threshold)
    .bind(donations_enabled)
    .bind(donations_enabled_at)
    .bind(donations_min_external_id)
    .bind(session.id)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to update session: {err}")))?;

    let updated = find_session_by_code(&state.db, &code).await?;
    let current_active_donation_session_ids = if payload.donations_enabled.is_some() {
        let query = format!(
            r#"
            SELECT s.id
            FROM stream_sessions s
            WHERE s.owner_user_id = ?1
              AND {}
              AND s.donations_enabled = 1
            ORDER BY COALESCE(s.donations_enabled_at, s.created_at) DESC;
            "#,
            active_session_condition_sql("s")
        );
        sqlx::query_scalar::<_, i64>(&query)
            .bind(auth.user_id)
            .fetch_all(&state.db)
            .await
            .map_err(|err| {
                AppError::internal(format!("failed to list updated donation routing: {err}"))
            })?
    } else {
        Vec::new()
    };

    tracing::info!(
        owner_user_id = auth.user_id,
        session_id = session.id,
        code = %code,
        %name,
        downvote_threshold,
        session_active = session.is_active,
        previous_donations_enabled = session.donations_enabled,
        donations_enabled,
        donations_routing_changed,
        previous_active_donation_session_ids = ?previous_active_donation_session_ids,
        current_active_donation_session_ids = ?current_active_donation_session_ids,
        "session metadata updated"
    );

    if donations_routing_changed {
        let session_ids = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT id
            FROM stream_sessions
            WHERE owner_user_id = ?1;
            "#,
        )
        .bind(auth.user_id)
        .fetch_all(&state.db)
        .await
        .map_err(|err| AppError::internal(format!("failed to list owner sessions: {err}")))?;

        for session_id in session_ids {
            state
                .session_events
                .publish(session_id, SessionEvent::resync())
                .await;
        }
    }

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
    require_auth_user_by_token(state, &token).await
}

async fn require_auth_user_by_token(state: &AppState, token: &str) -> Result<AuthUser, AppError> {
    let token_hash = hash_token(&token);

    let auth_user = sqlx::query_as::<_, AuthUser>(
        r#"
        SELECT s.user_id, s.last_seen_at
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

    if auth_user.last_seen_at <= now_unix() - AUTH_SESSION_TOUCH_INTERVAL_SECS {
        let db = state.db.clone();
        let token_hash_for_update = token_hash.clone();
        retry_sqlite_busy("touch auth session", move || {
            let db = db.clone();
            let token_hash = token_hash_for_update.clone();
            async move {
                sqlx::query(
                    r#"
                    UPDATE auth_sessions
                    SET last_seen_at = unixepoch()
                    WHERE token_hash = ?1
                      AND last_seen_at <= unixepoch() - ?2;
                    "#,
                )
                .bind(token_hash)
                .bind(AUTH_SESSION_TOUCH_INTERVAL_SECS)
                .execute(&db)
                .await
                .map(|_| ())
            }
        })
        .await
        .map_err(|err| AppError::internal(format!("failed to refresh auth session: {err}")))?;
    }

    Ok(auth_user)
}

async fn find_session_by_code(db: &SqlitePool, code: &str) -> Result<StreamSession, AppError> {
    let query = format!(
        r#"
        SELECT
            {}
        FROM stream_sessions s
        WHERE s.public_code = ?1
        LIMIT 1;
        "#,
        stream_session_select_sql("s")
    );
    sqlx::query_as::<_, StreamSession>(&query)
        .bind(code)
        .fetch_optional(db)
        .await
        .map_err(|err| AppError::internal(format!("failed to find session: {err}")))?
        .ok_or_else(|| AppError::not_found("session not found"))
}

async fn list_feed_items_for_session(
    db: &SqlitePool,
    session_id: i64,
    sort: QuestionSort,
    viewer_user_id: Option<i64>,
    downvote_threshold: i64,
) -> Result<Vec<FeedItemView>, AppError> {
    let mut question_items =
        list_question_items_for_session(db, session_id, sort, viewer_user_id, downvote_threshold)
            .await?;

    let mut donation_items = list_donation_items_for_session(db, session_id, sort).await?;

    let combined = match sort {
        QuestionSort::Top => {
            donation_items.sort_by(|a, b| {
                let a_usd = a.donation_usd_cents.unwrap_or(0);
                let b_usd = b.donation_usd_cents.unwrap_or(0);
                b_usd
                    .cmp(&a_usd)
                    .then_with(|| b.created_at.cmp(&a.created_at))
            });
            donation_items.append(&mut question_items);
            pin_answering_item_first(&mut donation_items);
            donation_items
        }
        QuestionSort::New => {
            question_items.append(&mut donation_items);
            question_items.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| b.id.cmp(&a.id))
            });
            pin_answering_item_first(&mut question_items);
            question_items
        }
        QuestionSort::Answered => {
            question_items.append(&mut donation_items);
            question_items.sort_by(|a, b| {
                let a_time = a.answered_at.unwrap_or(a.created_at);
                let b_time = b.answered_at.unwrap_or(b.created_at);
                b_time.cmp(&a_time).then_with(|| b.id.cmp(&a.id))
            });
            question_items
        }
        QuestionSort::Deleted => {
            question_items.append(&mut donation_items);
            question_items.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| b.id.cmp(&a.id))
            });
            question_items
        }
        QuestionSort::Downvoted => question_items,
    };

    Ok(combined)
}

async fn current_answering_conflict_message(
    db: &SqlitePool,
    session_id: i64,
    exclude_question_id: Option<i64>,
    exclude_donation_id: Option<i64>,
) -> Result<Option<&'static str>, AppError> {
    let conflicts = sqlx::query_as::<_, SessionAnsweringConflicts>(
        r#"
        SELECT
            EXISTS(
                SELECT 1
                FROM questions q
                WHERE q.session_id = ?1
                  AND q.status = ?2
                  AND (?3 IS NULL OR q.id != ?3)
            ) AS has_question,
            EXISTS(
                SELECT 1
                FROM donations d
                WHERE d.session_id = ?1
                  AND d.status = ?2
                  AND (?4 IS NULL OR d.id != ?4)
            ) AS has_donation;
        "#,
    )
    .bind(session_id)
    .bind(QuestionStatus::Answering.as_str())
    .bind(exclude_question_id)
    .bind(exclude_donation_id)
    .fetch_one(db)
    .await
    .map_err(|err| {
        AppError::internal(format!(
            "failed to verify in-progress answer uniqueness: {err}"
        ))
    })?;

    Ok(
        match (conflicts.has_question != 0, conflicts.has_donation != 0) {
            (true, true) => {
                Some("another question or donation is already being answered in this session")
            }
            (true, false) => Some("another question is already being answered in this session"),
            (false, true) => Some("a donation is already being answered in this session"),
            (false, false) => None,
        },
    )
}

fn pin_answering_item_first(items: &mut Vec<FeedItemView>) {
    if let Some(index) = items.iter().position(|item| item.is_answering != 0) {
        if index != 0 {
            let item = items.remove(index);
            items.insert(0, item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_item(id: i64, kind: &str, is_answering: i64, donation_usd_cents: Option<i64>) -> FeedItemView {
        FeedItemView {
            id,
            kind: kind.to_string(),
            question_id: (kind == "question").then_some(id),
            donation_id: (kind == "donation").then_some(id.abs()),
            session_id: 1,
            author_user_id: Some(1),
            author_nickname: "tester".to_string(),
            author_is_banned: 0,
            body: format!("{kind} {id}"),
            is_answering,
            is_answered: 0,
            is_rejected: 0,
            is_deleted: 0,
            created_at: id,
            score: 0,
            votes_count: 0,
            user_vote: 0,
            answering_started_at: None,
            answered_at: None,
            donation_amount_minor: None,
            donation_currency: None,
            donation_usd_cents,
            donation_external_id: None,
        }
    }

    #[test]
    fn pin_answering_item_first_moves_active_question_to_top() {
        let mut items = vec![
            feed_item(-10, "donation", 0, Some(1_000)),
            feed_item(20, "question", 1, None),
            feed_item(30, "question", 0, None),
        ];

        pin_answering_item_first(&mut items);

        assert_eq!(items[0].kind, "question");
        assert_eq!(items[0].id, 20);
    }

    #[test]
    fn pin_answering_item_first_moves_active_donation_to_top() {
        let mut items = vec![
            feed_item(-10, "donation", 0, Some(5_000)),
            feed_item(-20, "donation", 1, Some(100)),
            feed_item(30, "question", 0, None),
        ];

        pin_answering_item_first(&mut items);

        assert_eq!(items[0].kind, "donation");
        assert_eq!(items[0].id, -20);
    }
}

async fn list_question_items_for_session(
    db: &SqlitePool,
    session_id: i64,
    sort: QuestionSort,
    viewer_user_id: Option<i64>,
    downvote_threshold: i64,
) -> Result<Vec<FeedItemView>, AppError> {
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
            q.id AS id,
            'question' AS kind,
            q.id AS question_id,
            NULL AS donation_id,
            q.session_id,
            q.author_user_id AS author_user_id,
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
            q.answered_at,
            NULL AS donation_amount_minor,
            NULL AS donation_currency,
            NULL AS donation_usd_cents,
            NULL AS donation_external_id
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

    sqlx::query_as::<_, FeedItemView>(&sql)
        .bind(session_id)
        .bind(viewer_user_id.unwrap_or(-1))
        .fetch_all(db)
        .await
        .map_err(|err| AppError::internal(format!("failed to list questions: {err}")))
}

async fn list_donation_items_for_session(
    db: &SqlitePool,
    session_id: i64,
    sort: QuestionSort,
) -> Result<Vec<FeedItemView>, AppError> {
    let status_new = QuestionStatus::New.as_str();
    let status_answering = QuestionStatus::Answering.as_str();
    let status_answered = QuestionStatus::Answered.as_str();
    let status_rejected = QuestionStatus::Rejected.as_str();
    let status_deleted = QuestionStatus::Deleted.as_str();

    let (filter, order_by): (String, String) = match sort {
        QuestionSort::Top => (
            format!(
                "WHERE d.session_id = ?1 AND d.status IN ('{status_new}', '{status_answering}')"
            ),
            "ORDER BY d.usd_cents DESC, d.created_at DESC".to_string(),
        ),
        QuestionSort::New => (
            format!(
                "WHERE d.session_id = ?1 AND d.status IN ('{status_new}', '{status_answering}')"
            ),
            "ORDER BY d.created_at DESC".to_string(),
        ),
        QuestionSort::Answered => (
            format!(
                "WHERE d.session_id = ?1 AND d.status IN ('{status_answered}', '{status_rejected}')"
            ),
            "ORDER BY COALESCE(d.answered_at, d.created_at) DESC".to_string(),
        ),
        QuestionSort::Downvoted => return Ok(Vec::new()),
        QuestionSort::Deleted => (
            format!("WHERE d.session_id = ?1 AND d.status = '{status_deleted}'"),
            "ORDER BY d.created_at DESC".to_string(),
        ),
    };

    let sql = format!(
        r#"
        SELECT
            -d.id AS id,
            'donation' AS kind,
            NULL AS question_id,
            d.id AS donation_id,
            d.session_id,
            NULL AS author_user_id,
            d.donor_name AS author_nickname,
            0 AS author_is_banned,
            d.body,
            (d.status = '{status_answering}') AS is_answering,
            (d.status = '{status_answered}') AS is_answered,
            (d.status = '{status_rejected}') AS is_rejected,
            (d.status = '{status_deleted}') AS is_deleted,
            d.created_at,
            d.usd_cents AS score,
            0 AS votes_count,
            0 AS user_vote,
            d.answering_started_at,
            d.answered_at,
            d.amount_minor AS donation_amount_minor,
            d.currency AS donation_currency,
            d.usd_cents AS donation_usd_cents,
            d.external_donation_id AS donation_external_id
        FROM donations d
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

    sqlx::query_as::<_, FeedItemView>(&sql)
        .bind(session_id)
        .fetch_all(db)
        .await
        .map_err(|err| AppError::internal(format!("failed to list donations: {err}")))
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

async fn fetch_donation_by_id(db: &SqlitePool, donation_id: i64) -> Result<DonationView, AppError> {
    let status_answering = QuestionStatus::Answering.as_str();
    let status_answered = QuestionStatus::Answered.as_str();
    let status_rejected = QuestionStatus::Rejected.as_str();
    let status_deleted = QuestionStatus::Deleted.as_str();

    let sql = format!(
        r#"
        SELECT
            d.id,
            d.session_id,
            d.owner_user_id,
            d.external_donation_id,
            d.donor_name,
            d.body,
            d.amount_minor,
            d.currency,
            d.usd_cents,
            (d.status = '{status_answering}') AS is_answering,
            (d.status = '{status_answered}') AS is_answered,
            (d.status = '{status_rejected}') AS is_rejected,
            (d.status = '{status_deleted}') AS is_deleted,
            d.created_at,
            d.answering_started_at,
            d.answered_at
        FROM donations d
        WHERE d.id = ?1
        LIMIT 1;
        "#
    );

    sqlx::query_as::<_, DonationView>(&sql)
        .bind(donation_id)
        .fetch_optional(db)
        .await
        .map_err(|err| AppError::internal(format!("failed to fetch donation: {err}")))?
        .ok_or_else(|| AppError::not_found("donation not found"))
}

async fn run_db_migrations(db: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS schema_migrations (
            id TEXT PRIMARY KEY,
            applied_at INTEGER NOT NULL DEFAULT (unixepoch())
        );
        "#,
    )
    .execute(db)
    .await?;

    let migration_id = "20260305_donations";
    add_column_if_missing(
        db,
        "stream_sessions",
        "donations_enabled",
        "INTEGER NOT NULL DEFAULT 1 CHECK (donations_enabled IN (0, 1))",
    )
    .await?;
    add_column_if_missing(db, "stream_sessions", "donations_enabled_at", "INTEGER").await?;
    add_column_if_missing(
        db,
        "stream_sessions",
        "donations_min_external_id",
        "INTEGER",
    )
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS da_oauth_states (
            state TEXT PRIMARY KEY,
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            return_to TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            expires_at INTEGER NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_da_oauth_states_expires_at ON da_oauth_states(expires_at);",
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS da_integrations (
            owner_user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
            da_user_id INTEGER NOT NULL,
            access_token TEXT NOT NULL,
            refresh_token TEXT NOT NULL,
            token_expires_at INTEGER NOT NULL,
            scope TEXT NOT NULL,
            last_seen_external_id INTEGER,
            last_sync_at INTEGER,
            last_error TEXT,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at INTEGER NOT NULL DEFAULT (unixepoch())
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS donations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id INTEGER NOT NULL REFERENCES stream_sessions(id) ON DELETE CASCADE,
            owner_user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            external_donation_id INTEGER NOT NULL,
            donor_name TEXT NOT NULL,
            body TEXT NOT NULL CHECK (length(body) <= 300),
            amount_minor INTEGER NOT NULL,
            currency TEXT NOT NULL,
            usd_cents INTEGER NOT NULL,
            provider_created_at TEXT,
            status TEXT NOT NULL DEFAULT 'new'
                CHECK (status IN ('new', 'answering', 'answered', 'rejected', 'deleted')),
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            answering_started_at INTEGER,
            answered_at INTEGER,
            UNIQUE(owner_user_id, external_donation_id)
        );
        "#,
    )
    .execute(db)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_donations_session_status_created ON donations(session_id, status, created_at DESC);",
    )
    .execute(db)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_donations_owner_external ON donations(owner_user_id, external_donation_id DESC);",
    )
    .execute(db)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_stream_sessions_owner_active_donations ON stream_sessions(owner_user_id, is_active, donations_enabled);",
    )
    .execute(db)
    .await?;

    mark_migration_applied(db, migration_id).await?;
    Ok(())
}

async fn mark_migration_applied(db: &SqlitePool, id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO schema_migrations (id, applied_at) VALUES (?1, unixepoch());",
    )
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

async fn add_column_if_missing(
    db: &SqlitePool,
    table_name: &str,
    column_name: &str,
    column_sql: &str,
) -> anyhow::Result<()> {
    if column_exists(db, table_name, column_name).await? {
        return Ok(());
    }

    let sql = format!("ALTER TABLE {table_name} ADD COLUMN {column_name} {column_sql};");
    sqlx::query(&sql).execute(db).await?;
    Ok(())
}

async fn column_exists(
    db: &SqlitePool,
    table_name: &str,
    column_name: &str,
) -> anyhow::Result<bool> {
    let query = format!("PRAGMA table_info({table_name});");
    let rows = sqlx::query(&query).fetch_all(db).await?;
    for row in rows {
        let name: String = row.try_get("name")?;
        if name == column_name {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn donation_sync_loop(state: AppState) {
    if state.da_client_id.is_none()
        || state.da_client_secret.is_none()
        || state.da_redirect_uri.is_none()
    {
        tracing::info!("donation sync loop disabled: DA OAuth is not configured");
        return;
    }

    loop {
        if let Err(err) = sync_donations_once(&state).await {
            tracing::warn!("donation sync loop failed: {err}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn sync_donations_once(state: &AppState) -> anyhow::Result<()> {
    let owners_query = format!(
        r#"
        SELECT DISTINCT di.owner_user_id
        FROM da_integrations di
        JOIN stream_sessions s
          ON s.owner_user_id = di.owner_user_id
         AND {}
         AND s.donations_enabled = 1;
        "#,
        active_session_condition_sql("s")
    );
    let owners = sqlx::query_scalar::<_, i64>(&owners_query)
        .fetch_all(&state.db)
        .await?;

    for owner_user_id in owners {
        if let Err(err) = sync_owner_donations(state, owner_user_id).await {
            tracing::warn!(owner_user_id, "failed to sync donations: {err}");
            let db = state.db.clone();
            let error_text = err.to_string();
            let _ = retry_sqlite_busy("store donation sync error", move || {
                let db = db.clone();
                let error_text = error_text.clone();
                async move {
                    sqlx::query(
                        r#"
                        UPDATE da_integrations
                        SET last_error = ?2, updated_at = unixepoch()
                        WHERE owner_user_id = ?1;
                        "#,
                    )
                    .bind(owner_user_id)
                    .bind(error_text)
                    .execute(&db)
                    .await
                    .map(|_| ())
                }
            })
            .await;
        }
    }

    Ok(())
}

async fn sync_owner_donations(state: &AppState, owner_user_id: i64) -> anyhow::Result<()> {
    let session_query = format!(
        r#"
        SELECT
            id AS session_id,
            donations_min_external_id AS min_external_id
        FROM stream_sessions
        WHERE owner_user_id = ?1
          AND {}
          AND donations_enabled = 1
        ORDER BY COALESCE(donations_enabled_at, created_at) DESC
        LIMIT 1;
        "#,
        active_session_condition_sql("stream_sessions")
    );
    let session = sqlx::query_as::<_, DonationSyncSessionRow>(&session_query)
        .bind(owner_user_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no active donation-enabled session"))?;

    let integration = sqlx::query_as::<_, DaIntegrationAuthRow>(
        r#"
        SELECT
            owner_user_id,
            da_user_id,
            access_token,
            refresh_token,
            token_expires_at,
            scope,
            last_seen_external_id,
            last_sync_at,
            last_error
        FROM da_integrations
        WHERE owner_user_id = ?1
        LIMIT 1;
        "#,
    )
    .bind(owner_user_id)
    .fetch_one(&state.db)
    .await?;

    let integration = ensure_da_access_token(state, integration).await?;
    let baseline_id = session
        .min_external_id
        .unwrap_or(0)
        .max(integration.last_seen_external_id.unwrap_or(0));

    let donations =
        fetch_donations_since(&state.http, &integration.access_token, baseline_id).await?;
    let fetched_count = donations.len();
    let mut inserted_count = 0usize;
    let mut duplicate_count = 0usize;
    let mut max_seen = baseline_id;

    for donation in donations {
        max_seen = max_seen.max(donation.id);
        let donor_name = normalize_donation_name(donation.username.as_deref());
        let body = normalize_donation_body(donation.message.as_deref());
        let amount_minor = amount_to_minor(donation.amount);
        let usd_cents = convert_amount_minor_to_usd_cents(amount_minor, &donation.currency);
        let created_at = now_unix();
        let db = state.db.clone();
        let session_id = session.session_id;
        let donation_id = donation.id;
        let currency = donation.currency.to_ascii_uppercase();
        let provider_created_at = donation.created_at.clone();
        let insert = retry_sqlite_busy("insert donation", move || {
            let db = db.clone();
            let donor_name = donor_name.clone();
            let body = body.clone();
            let currency = currency.clone();
            let provider_created_at = provider_created_at.clone();
            async move {
                sqlx::query(
                    r#"
                    INSERT INTO donations (
                        session_id,
                        owner_user_id,
                        external_donation_id,
                        donor_name,
                        body,
                        amount_minor,
                        currency,
                        usd_cents,
                        provider_created_at,
                        status,
                        created_at
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                    ON CONFLICT(owner_user_id, external_donation_id) DO NOTHING;
                    "#,
                )
                .bind(session_id)
                .bind(owner_user_id)
                .bind(donation_id)
                .bind(donor_name)
                .bind(body)
                .bind(amount_minor)
                .bind(currency)
                .bind(usd_cents)
                .bind(provider_created_at.as_deref())
                .bind(QuestionStatus::New.as_str())
                .bind(created_at)
                .execute(&db)
                .await
            }
        })
        .await?;

        if insert.rows_affected() > 0 {
            inserted_count += 1;
            state
                .session_events
                .publish(
                    session.session_id,
                    SessionEvent::donation_created(insert.last_insert_rowid()),
                )
                .await;
        } else {
            duplicate_count += 1;
        }
    }

    let should_update_sync_state = max_seen != integration.last_seen_external_id.unwrap_or(0)
        || integration.last_error.is_some()
        || integration.last_sync_at.unwrap_or(0) <= now_unix() - DA_SYNC_STATE_TOUCH_INTERVAL_SECS;

    if should_update_sync_state {
        let db = state.db.clone();
        retry_sqlite_busy("update donation sync state", move || {
            let db = db.clone();
            async move {
                sqlx::query(
                    r#"
                    UPDATE da_integrations
                    SET last_seen_external_id = ?2,
                        last_sync_at = unixepoch(),
                        last_error = NULL,
                        updated_at = unixepoch()
                    WHERE owner_user_id = ?1;
                    "#,
                )
                .bind(owner_user_id)
                .bind(max_seen)
                .execute(&db)
                .await
                .map(|_| ())
            }
        })
        .await?;
    }

    let previous_last_seen = integration.last_seen_external_id.unwrap_or(0);
    if fetched_count == 0 && !should_update_sync_state {
        tracing::debug!(
            owner_user_id,
            session_id = session.session_id,
            baseline_id,
            fetched_count,
            inserted_count,
            duplicate_count,
            previous_last_seen,
            new_last_seen = max_seen,
            state_write = should_update_sync_state,
            "donation sync completed"
        );
    } else {
        tracing::info!(
            owner_user_id,
            session_id = session.session_id,
            baseline_id,
            fetched_count,
            inserted_count,
            duplicate_count,
            previous_last_seen,
            new_last_seen = max_seen,
            state_write = should_update_sync_state,
            "donation sync completed"
        );
    }

    Ok(())
}

async fn ensure_da_access_token(
    state: &AppState,
    integration: DaIntegrationAuthRow,
) -> anyhow::Result<DaIntegrationAuthRow> {
    if integration.token_expires_at > now_unix() + 60 {
        return Ok(integration);
    }

    let client_id = state
        .da_client_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("DA_CLIENT_ID is not configured"))?;
    let client_secret = state
        .da_client_secret
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("DA_CLIENT_SECRET is not configured"))?;

    let response = state
        .http
        .post("https://www.donationalerts.com/oauth/token")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", integration.refresh_token.as_str()),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("scope", integration.scope.as_str()),
        ])
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "donationalerts refresh token request failed with {}",
            response.status()
        ));
    }

    let refreshed: DonationAlertsTokenResponse = response.json().await?;
    let new_refresh = refreshed
        .refresh_token
        .as_deref()
        .unwrap_or(&integration.refresh_token);
    let new_expires_at = now_unix() + refreshed.expires_in.unwrap_or(3600).max(60) - 30;
    let new_scope = refreshed
        .scope
        .as_deref()
        .unwrap_or(&integration.scope)
        .to_string();

    let db = state.db.clone();
    let access_token = refreshed.access_token.clone();
    let new_refresh_owned = new_refresh.to_string();
    let new_scope_owned = new_scope.clone();
    retry_sqlite_busy("refresh da access token", move || {
        let db = db.clone();
        let access_token = access_token.clone();
        let new_refresh = new_refresh_owned.clone();
        let new_scope = new_scope_owned.clone();
        async move {
            sqlx::query(
                r#"
                UPDATE da_integrations
                SET access_token = ?2,
                    refresh_token = ?3,
                    token_expires_at = ?4,
                    scope = ?5,
                    updated_at = unixepoch()
                WHERE owner_user_id = ?1;
                "#,
            )
            .bind(integration.owner_user_id)
            .bind(access_token)
            .bind(new_refresh)
            .bind(new_expires_at)
            .bind(new_scope)
            .execute(&db)
            .await
            .map(|_| ())
        }
    })
    .await?;

    Ok(DaIntegrationAuthRow {
        owner_user_id: integration.owner_user_id,
        da_user_id: integration.da_user_id,
        access_token: refreshed.access_token,
        refresh_token: new_refresh.to_string(),
        token_expires_at: new_expires_at,
        scope: new_scope,
        last_seen_external_id: integration.last_seen_external_id,
        last_sync_at: integration.last_sync_at,
        last_error: integration.last_error,
    })
}

async fn resolve_donation_baseline_for_owner(
    state: &AppState,
    owner_user_id: i64,
) -> Result<i64, AppError> {
    let Some(integration) = sqlx::query_as::<_, DaIntegrationAuthRow>(
        r#"
        SELECT
            owner_user_id,
            da_user_id,
            access_token,
            refresh_token,
            token_expires_at,
            scope,
            last_seen_external_id,
            last_sync_at,
            last_error
        FROM da_integrations
        WHERE owner_user_id = ?1
        LIMIT 1;
        "#,
    )
    .bind(owner_user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to load da integration: {err}")))?
    else {
        return Ok(0);
    };

    let integration = ensure_da_access_token(state, integration)
        .await
        .map_err(|err| AppError::internal(format!("failed to refresh da token: {err}")))?;

    let fallback = integration.last_seen_external_id.unwrap_or(0).max(0);
    match fetch_latest_donation_external_id(&state.http, &integration.access_token).await {
        Ok(latest) => Ok(latest.max(fallback)),
        Err(err) => {
            tracing::warn!(
                owner_user_id,
                "failed to fetch donation baseline, using fallback: {err}"
            );
            Ok(fallback)
        }
    }
}

async fn fetch_latest_donation_external_id(
    http: &Client,
    access_token: &str,
) -> anyhow::Result<i64> {
    let response = http
        .get("https://www.donationalerts.com/api/v1/alerts/donations")
        .bearer_auth(access_token)
        .query(&[("page", "1"), ("limit", "1")])
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "donationalerts donations request failed with {}",
            response.status()
        ));
    }
    let payload: DonationAlertsDonationsResponse = response.json().await?;
    Ok(payload.data.into_iter().map(|d| d.id).max().unwrap_or(0))
}

async fn fetch_donations_since(
    http: &Client,
    access_token: &str,
    baseline_external_id: i64,
) -> anyhow::Result<Vec<DonationAlertsDonation>> {
    let mut page = 1i64;
    let limit = 100i64;
    let max_pages = 20i64;
    let mut collected = Vec::new();
    let mut reached_known_id = false;

    while page <= max_pages && !reached_known_id {
        let response = http
            .get("https://www.donationalerts.com/api/v1/alerts/donations")
            .bearer_auth(access_token)
            .query(&[("page", page.to_string()), ("limit", limit.to_string())])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "donationalerts donations request failed with {}",
                response.status()
            ));
        }

        let payload: DonationAlertsDonationsResponse = response.json().await?;
        if payload.data.is_empty() {
            break;
        }

        let mut page_has_old = false;
        for donation in payload.data {
            if donation.id <= baseline_external_id {
                page_has_old = true;
                continue;
            }
            collected.push(donation);
        }

        if page_has_old {
            reached_known_id = true;
        }
        page += 1;
    }

    collected.sort_by_key(|d| d.id);
    Ok(collected)
}

fn amount_to_minor(amount: f64) -> i64 {
    if !amount.is_finite() {
        return 0;
    }
    let scaled = (amount * 100.0).round();
    if scaled < i64::MIN as f64 {
        i64::MIN
    } else if scaled > i64::MAX as f64 {
        i64::MAX
    } else {
        scaled as i64
    }
}

fn convert_amount_minor_to_usd_cents(amount_minor: i64, currency: &str) -> i64 {
    let rate = usd_rate_for_currency(currency).unwrap_or(1.0);
    let raw = (amount_minor as f64 * rate).round();
    if raw < i64::MIN as f64 {
        i64::MIN
    } else if raw > i64::MAX as f64 {
        i64::MAX
    } else {
        raw as i64
    }
}

fn usd_rate_for_currency(currency: &str) -> Option<f64> {
    match currency.trim().to_ascii_uppercase().as_str() {
        "USD" => Some(1.0),
        "EUR" => Some(1.09),
        "RUB" => Some(0.011),
        "BYN" => Some(0.31),
        "KZT" => Some(0.0021),
        "UAH" => Some(0.024),
        "BRL" => Some(0.20),
        "TRY" => Some(0.031),
        _ => None,
    }
}

fn normalize_donation_name(raw: Option<&str>) -> String {
    let trimmed = raw.unwrap_or("").trim();
    if trimmed.is_empty() {
        return "donor".to_string();
    }

    let mut out = String::new();
    for ch in trimmed.chars().take(64) {
        if ch.is_control() {
            continue;
        }
        out.push(ch);
    }

    let final_name = out.trim();
    if final_name.is_empty() {
        "donor".to_string()
    } else {
        final_name.to_string()
    }
}

fn normalize_donation_body(raw: Option<&str>) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    for ch in raw.unwrap_or("").chars() {
        if count >= 300 {
            break;
        }
        if ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t' {
            continue;
        }
        out.push(ch);
        count += 1;
    }
    out.trim().to_string()
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

fn is_sqlite_busy_error(err: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db_err) = err else {
        return false;
    };

    db_err.code().as_deref() == Some("5")
        || db_err.message().contains("database is locked")
        || db_err.message().contains("database is busy")
}

async fn retry_sqlite_busy<T, F, Fut>(operation: &'static str, mut op: F) -> Result<T, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, sqlx::Error>>,
{
    let mut attempt = 0usize;

    loop {
        match op().await {
            Ok(value) => {
                if attempt > 0 {
                    tracing::info!(
                        operation,
                        attempts = attempt + 1,
                        "sqlite write succeeded after retries"
                    );
                }
                return Ok(value);
            }
            Err(err) if attempt < SQLITE_BUSY_RETRY_ATTEMPTS && is_sqlite_busy_error(&err) => {
                attempt += 1;
                tracing::warn!(operation, attempt, error = %err, "sqlite busy; retrying write");
                tokio::time::sleep(Duration::from_millis(
                    SQLITE_BUSY_RETRY_DELAY_MS * attempt as u64,
                ))
                .await;
            }
            Err(err) => {
                if is_sqlite_busy_error(&err) {
                    tracing::error!(
                        operation,
                        attempts = attempt + 1,
                        error = %err,
                        "sqlite busy write failed after retries"
                    );
                }
                return Err(err);
            }
        }
    }
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

async fn consume_da_oauth_state(
    db: &SqlitePool,
    state_token: &str,
) -> Result<(i64, String), AppError> {
    let mut tx = db
        .begin()
        .await
        .map_err(|err| AppError::internal(format!("failed to start da oauth state tx: {err}")))?;

    let row: Option<(i64, String)> = sqlx::query_as(
        r#"
        SELECT user_id, return_to
        FROM da_oauth_states
        WHERE state = ?1 AND expires_at > unixepoch()
        LIMIT 1;
        "#,
    )
    .bind(state_token)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|err| AppError::internal(format!("failed to read da oauth state: {err}")))?;

    sqlx::query("DELETE FROM da_oauth_states WHERE state = ?1;")
        .bind(state_token)
        .execute(&mut *tx)
        .await
        .map_err(|err| AppError::internal(format!("failed to consume da oauth state: {err}")))?;

    tx.commit()
        .await
        .map_err(|err| AppError::internal(format!("failed to commit da oauth state tx: {err}")))?;

    row.ok_or_else(|| AppError::unauthorized("invalid or expired da oauth state"))
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

fn session_code_from_return_to(return_to: &str) -> Option<&str> {
    let path = return_to.split('?').next().unwrap_or(return_to);
    let mut parts = path.split('/').filter(|segment| !segment.is_empty());
    if parts.next()? != "s" {
        return None;
    }
    let code = parts.next()?;
    let valid = code
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if valid {
        Some(code)
    } else {
        None
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
