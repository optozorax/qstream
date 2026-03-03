use anyhow::Context;
use axum::{
    extract::{ConnectInfo, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
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
    hcaptcha_secret: Option<String>,
    hcaptcha_site_key: Option<String>,
    hcaptcha_skip_verify: bool,
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
            tracing::debug!(removed, remaining = channels.len(), "cleaned up idle SSE channels");
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

#[derive(Deserialize)]
struct RegisterRequest {
    nickname: String,
    hcaptcha_token: String,
}

#[derive(Serialize)]
struct RegisterResponse {
    user: User,
    auth_token: String,
    session: Option<StreamSession>,
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
}

#[derive(Serialize)]
struct CreateSessionResponse {
    session: StreamSession,
    created: bool,
    public_url: String,
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
    question: Option<QuestionView>,
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

#[derive(Deserialize, Default)]
struct ListQuestionsQuery {
    sort: Option<String>,
}

#[derive(FromRow)]
struct QuestionVoteMeta {
    session_id: i64,
    owner_user_id: i64,
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

    let hcaptcha_secret = env::var("HCAPTCHA_SECRET").ok();
    let hcaptcha_site_key = env::var("HCAPTCHA_SITE_KEY").ok();
    let hcaptcha_skip_verify = env::var("HCAPTCHA_SKIP_VERIFY")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    let reset_db_on_boot = env::var("RESET_DB_ON_BOOT")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);

    tracing::info!(
        db = %db_url,
        addr = %app_addr,
        frontend_origin = %frontend_origin,
        hcaptcha_skip = hcaptcha_skip_verify,
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
        hcaptcha_secret,
        hcaptcha_site_key,
        hcaptcha_skip_verify,
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
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/me", get(get_me))
        .route("/api/register", post(register))
        .route("/api/sessions", post(create_session))
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

async fn get_me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<User>, AppError> {
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

    tracing::info!(ip = %addr.ip(), nickname, "registering new user");

    // Always insert a new user (nicknames are not unique)
    let user_id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO users (nickname, last_login_at, last_hcaptcha_at)
        VALUES (?1, unixepoch(), unixepoch())
        RETURNING id;
        "#,
    )
    .bind(nickname)
    .fetch_one(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to save user: {err}")))?;

    let user = sqlx::query_as::<_, User>(
        r#"
        SELECT
            id,
            nickname,
            CAST(created_at AS INTEGER) AS created_at,
            CAST(last_login_at AS INTEGER) AS last_login_at
        FROM users
        WHERE id = ?1;
        "#,
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to fetch user: {err}")))?;

    let auth_token = format!("{}{}", Uuid::new_v4(), Uuid::new_v4().simple());
    let token_hash = hash_token(&auth_token);

    sqlx::query(
        r#"
        INSERT INTO auth_sessions (user_id, token_hash, created_at, last_seen_at)
        VALUES (?1, ?2, unixepoch(), unixepoch());
        "#,
    )
    .bind(user.id)
    .bind(token_hash)
    .execute(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to create auth session: {err}")))?;

    let session = sqlx::query_as::<_, StreamSession>(
        r#"
        SELECT
            id,
            owner_user_id,
            public_code,
            CAST(created_at AS INTEGER) AS created_at,
            is_active
        FROM stream_sessions
        WHERE owner_user_id = ?1
        LIMIT 1;
        "#,
    )
    .bind(user.id)
    .fetch_optional(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to query stream session: {err}")))?;

    tracing::info!(user_id = user.id, nickname = %user.nickname, "user registered");
    Ok(Json(RegisterResponse {
        user,
        auth_token,
        session,
    }))
}

async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CreateSessionResponse>, AppError> {
    let auth = require_auth_user(&state, &headers).await?;

    if let Some(existing) = sqlx::query_as::<_, StreamSession>(
        r#"
        SELECT
            id,
            owner_user_id,
            public_code,
            CAST(created_at AS INTEGER) AS created_at,
            is_active
        FROM stream_sessions
        WHERE owner_user_id = ?1
        LIMIT 1;
        "#,
    )
    .bind(auth.user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|err| AppError::internal(format!("failed to query stream session: {err}")))?
    {
        tracing::info!(user_id = auth.user_id, code = %existing.public_code, "returning existing session");
        return Ok(Json(CreateSessionResponse {
            public_url: build_public_session_url(&state.public_base_url, &existing.public_code),
            session: existing,
            created: false,
        }));
    }

    let mut inserted = None;
    for _ in 0..8 {
        let public_code = generate_session_code();
        let res = sqlx::query(
            r#"
            INSERT INTO stream_sessions (owner_user_id, public_code, created_at, is_active)
            VALUES (?1, ?2, unixepoch(), 1);
            "#,
        )
        .bind(auth.user_id)
        .bind(&public_code)
        .execute(&state.db)
        .await;

        match res {
            Ok(_) => {
                let session = sqlx::query_as::<_, StreamSession>(
                    r#"
                    SELECT
                        id,
                        owner_user_id,
                        public_code,
                        CAST(created_at AS INTEGER) AS created_at,
                        is_active
                    FROM stream_sessions
                    WHERE owner_user_id = ?1
                    LIMIT 1;
                    "#,
                )
                .bind(auth.user_id)
                .fetch_one(&state.db)
                .await
                .map_err(|err| {
                    AppError::internal(format!("failed to fetch stream session: {err}"))
                })?;
                inserted = Some(session);
                break;
            }
            Err(err) => {
                let err_text = err.to_string();
                if err_text.contains("stream_sessions.public_code") {
                    continue;
                }
                if err_text.contains("stream_sessions.owner_user_id") {
                    return Err(AppError::bad_request(
                        "only one session per user is allowed for now",
                    ));
                }
                return Err(AppError::internal(format!(
                    "failed to create stream session: {err}"
                )));
            }
        }
    }

    let session =
        inserted.ok_or_else(|| AppError::internal("failed to generate unique session code"))?;

    tracing::info!(user_id = auth.user_id, code = %session.public_code, "session created");
    Ok(Json(CreateSessionResponse {
        public_url: build_public_session_url(&state.public_base_url, &session.public_code),
        session,
        created: true,
    }))
}

async fn list_questions(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Query(query): Query<ListQuestionsQuery>,
) -> Result<Json<ListQuestionsResponse>, AppError> {
    let session = find_session_by_code(&state.db, &code).await?;

    let raw_sort = query.sort.as_deref().unwrap_or("top");
    let sort = QuestionSort::parse(raw_sort).ok_or_else(|| {
        AppError::bad_request("sort must be 'top', 'new', or 'answered'")
    })?;

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
        SELECT q.session_id, s.owner_user_id, q.is_answering, q.is_answered, q.is_rejected, q.is_deleted
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
        tracing::warn!(user_id = auth.user_id, "vote rate limit hit ({vote_count}/min)");
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
        SELECT s.id AS session_id, s.owner_user_id
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
            tracing::info!(question_id, user_id = auth.user_id, "question marked answering");
            state
                .session_events
                .publish(meta.session_id, SessionEvent::question_changed(question_id))
                .await;

            Ok(Json(ModerateQuestionResponse {
                question_id,
                deleted: false,
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
            .map_err(|err| {
                AppError::internal(format!("failed to reject question: {err}"))
            })?;

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
                question: None,
            }))
        }
        _ => Err(AppError::bad_request(
            "action must be one of: answer, finish_answering, reject, delete",
        )),
    }
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
            is_active
        FROM stream_sessions
        WHERE public_code = ?1 AND is_active = 1
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
