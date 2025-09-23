mod engine;
mod review;

use anyhow::{Context, anyhow};
use axum::{
    Json, Router,
    extract::{FromRef, FromRequest, Multipart, State},
    http::{
        HeaderMap, HeaderValue, Method, Request, StatusCode, header::CONTENT_TYPE,
        header::SET_COOKIE,
    },
    response::{IntoResponse, Response},
    routing::post,
};
use http::Uri;
use http_body_util::BodyExt;
use reqwest::Client;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{fs, signal};
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
};
use tracing_appender::rolling;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};
// 从环境变量加载配置（仅 backend/.env）；不覆盖已有进程变量
fn load_env() {
    // 明确加载 backend 目录下的 .env，避免因工作目录不同而读取到仓库根 .env
    let backend_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let _ = dotenvy::from_filename(backend_dir.join(".env"));
}

#[derive(Clone)]
struct AppState {
    concurrency_limit_per_sid: u32,
    session_store: Arc<dashmap::DashMap<String, Vec<String>>>, // sid -> [gameId]
    game_store: Arc<dashmap::DashMap<String, GameState>>,      // gameId -> state
    review_store: Arc<dashmap::DashMap<String, review::ReviewState>>, // reviewId -> state
    game_ttl_seconds: i64,
    review_ttl_seconds: i64,
    server_start_at: i64,
    sid_locks: Arc<dashmap::DashMap<String, Arc<tokio::sync::Mutex<()>>>>, // 防止同一 sid 并发新建
}

impl FromRef<AppState> for Arc<dashmap::DashMap<String, Vec<String>>> {
    fn from_ref(state: &AppState) -> Self {
        state.session_store.clone()
    }
}

impl FromRef<AppState> for Arc<dashmap::DashMap<String, GameState>> {
    fn from_ref(state: &AppState) -> Self {
        state.game_store.clone()
    }
}

impl FromRef<AppState> for Arc<dashmap::DashMap<String, review::ReviewState>> {
    fn from_ref(state: &AppState) -> Self {
        state.review_store.clone()
    }
}

#[derive(Clone)]
struct GameState {
    sid: String,
    last_active_at: i64,
    engine: Option<std::sync::Arc<engine::gtp::GtpEngine>>, // None 时使用占位行为
    human_color: String,                                    // "black" or "white"
    board_size: u32,
    komi: f32,
}

#[tokio::main]
async fn main() {
    // init tracing: console + 每日滚动文件日志
    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let file_appender = rolling::daily(&log_dir, "backend.log");
    // ensure gtp logs directory exists for KataGo's own logging
    let gtp_log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("gtp_logs");
    let _ = std::fs::create_dir_all(&gtp_log_dir);
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::registry()
        .with(fmt::layer().compact())
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .init();

    load_env();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let concurrency_limit_per_sid: u32 = std::env::var("CONCURRENCY_PER_SID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let game_ttl_minutes: i64 = std::env::var("GAME_TTL_MINUTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let game_ttl_seconds = game_ttl_minutes * 60;
    let review_ttl_minutes: i64 = std::env::var("REVIEW_TTL_MINUTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let review_ttl_seconds = review_ttl_minutes * 60;

    let state = Arc::new(AppState {
        concurrency_limit_per_sid,
        session_store: Arc::new(dashmap::DashMap::new()),
        game_store: Arc::new(dashmap::DashMap::new()),
        review_store: Arc::new(dashmap::DashMap::new()),
        game_ttl_seconds,
        review_ttl_seconds,
        server_start_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        sid_locks: Arc::new(dashmap::DashMap::new()),
    });
    let state_for_cleaner = state.clone();

    // CORS（若通过同源静态托管，几乎不会命中，但保留更安全）
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers(Any);

    let api = Router::new()
        .route("/api/game/new", post(game_new))
        .route("/api/game/play", post(game_play))
        .route("/api/game/heartbeat", post(game_heartbeat))
        .route("/api/game/close", post(game_close))
        .route("/api/game/score_detail", post(game_score_detail))
        .route("/api/game/hint", post(game_hint))
        .route("/api/review/import", post(review_import))
        .route("/api/review/analyze", post(review_analyze))
        .route("/api/exercise/save", post(exercise_save));

    let static_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap() // project root
        .join("frontend/public");

    let app = Router::new()
        .nest("/", api)
        .fallback_service(
            axum::routing::get_service(ServeDir::new(static_dir)).handle_error(|err| async move {
                tracing::error!(?err, "serve static error");
                StatusCode::INTERNAL_SERVER_ERROR
            }),
        )
        .layer(cors)
        .with_state(state.clone());

    let addr: SocketAddr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "starting server");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    // 过期清理后台任务
    let cleaner_state = state_for_cleaner;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let mut affected_sids: Vec<(String, String)> = Vec::new();
            cleaner_state.game_store.retain(|game_id, gs| {
                let expired = now - gs.last_active_at > cleaner_state.game_ttl_seconds;
                if expired {
                    // 过期对局：尝试优雅退出其引擎
                    if let Some(engine) = gs.engine.as_ref() {
                        let e = engine.clone();
                        // 在后台异步退出，避免阻塞 retain 闭包
                        tokio::spawn(async move {
                            let _ = e.quit().await;
                        });
                    }
                    affected_sids.push((gs.sid.clone(), game_id.clone()));
                }
                !expired
            });
            // 从 session_store 移除已过期的 gameId
            for (sid, gid) in affected_sids {
                if let Some(mut v) = cleaner_state.session_store.get_mut(&sid) {
                    v.retain(|g| g != &gid);
                }
            }
            // 可选：如需日志，可在此输出 affected_sids.len()

            cleaner_state.review_store.retain(|_, review| {
                now - review.last_active_at <= cleaner_state.review_ttl_seconds
            });
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    // 服务退出后，清理剩余引擎子进程（尽力而为）
    let mut engines: Vec<std::sync::Arc<engine::gtp::GtpEngine>> = Vec::new();
    for entry in state.game_store.iter() {
        if let Some(e) = entry.value().engine.clone() {
            engines.push(e);
        }
    }
    for e in engines {
        let _ = e.quit().await;
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("failed to install signal handler");
        term.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

// --- Game routes (stubs) ---
#[derive(Serialize)]
struct NewGameResponse {
    gameId: String,
    expiresAt: i64,
    activeGames: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    engineMove: Option<String>, // 人类执白时，AI 的首手
}

#[derive(serde::Deserialize)]
struct NewGameRequest {
    boardSize: Option<u32>,
    rules: Option<String>,
    komi: Option<f32>,
    handicap: Option<u32>,
    engineLevel: Option<u8>,
    playerColor: Option<String>,
}

async fn game_new(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    maybe_body: Option<Json<NewGameRequest>>,
) -> impl IntoResponse {
    let (sid, set_cookie) = get_or_create_sid(headers);

    // per-sid 互斥，防止同时点多次“新开对局”导致重复启动引擎
    let lock = state
        .sid_locks
        .entry(sid.clone())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;

    let active = state
        .session_store
        .get(&sid)
        .map(|v| v.len() as u32)
        .unwrap_or(0);

    if active >= state.concurrency_limit_per_sid {
        let body = serde_json::json!({
            "error": "CONCURRENCY_LIMIT",
            "message": "最多同时 3 局",
            "retryAfterSeconds": 10,
            "activeGames": active,
        });
        let mut resp = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
        if let Some(sc) = set_cookie {
            resp.headers_mut().insert("set-cookie", sc);
        }
        return resp;
    }

    // 生成 gameId 并登记
    let game_id = format!("g-{}", uuid::Uuid::new_v4());
    let now = time::OffsetDateTime::now_utc().unix_timestamp();

    // 若环境配置齐全则尝试启动引擎，否则置为 None（占位）
    let engine = match (
        std::env::var("ENGINE_PATH"),
        std::env::var("MODEL_PATH"),
        std::env::var("GTP_CONFIG_PATH"),
    ) {
        (Ok(engine_path), Ok(model_path), Ok(config_path)) => {
            let mut args = vec![
                "gtp".to_string(),
                "-model".to_string(),
                model_path,
                "-config".to_string(),
                config_path,
            ];
            // 难度 → 覆盖配置
            let req = maybe_body.as_ref().map(|j| &j.0);
            let level = req.and_then(|r| r.engineLevel).unwrap_or(3);
            for (k, v) in overrides_for_level(level) {
                args.push("-override-config".to_string());
                args.push(format!("{}={}", k, v));
            }
            // 规则 → 覆盖配置（默认 chinese）
            let rule_name = req
                .and_then(|r| r.rules.clone())
                .unwrap_or_else(|| "chinese".to_string());
            args.push("-override-config".to_string());
            args.push(format!("rules={}", rule_name));
            match engine::gtp::GtpEngine::start(&engine_path, &args).await {
                Ok(e) => {
                    // 初始化棋盘参数
                    let board_size = req.and_then(|r| r.boardSize).unwrap_or(19);
                    let komi_req = req.and_then(|r| r.komi).unwrap_or(6.5);
                    let rule_name = req
                        .and_then(|r| r.rules.clone())
                        .unwrap_or_else(|| "chinese".to_string());
                    // 规则化 komi：Chinese 默认 7.5；其他沿用传入/默认值
                    let effective_komi: f32 = if rule_name.eq_ignore_ascii_case("chinese") {
                        7.5
                    } else {
                        komi_req
                    };
                    // 顺序调整：先清盘，再设棋盘大小，最后设贴目，避免 clear_board 重置贴目导致异常（如出现 W+0.5）
                    let _ = e.send_command("clear_board").await;
                    let _ = e.send_command(&format!("boardsize {}", board_size)).await;
                    let _ = e.send_command(&format!("komi {}", effective_komi)).await;
                    Some(e)
                }
                Err(err) => {
                    tracing::warn!(?err, "failed to start katago, fallback to stub engine");
                    None
                }
            }
        }
        _ => None,
    };

    state
        .session_store
        .entry(sid.clone())
        .and_modify(|v| v.push(game_id.clone()))
        .or_insert_with(|| vec![game_id.clone()]);

    let player_color = maybe_body
        .as_ref()
        .and_then(|j| j.playerColor.clone())
        .unwrap_or_else(|| "black".to_string());
    state.game_store.insert(
        game_id.clone(),
        GameState {
            sid: sid.clone(),
            last_active_at: now,
            engine: engine.clone(),
            human_color: player_color.clone(),
            board_size: maybe_body.as_ref().and_then(|j| j.boardSize).unwrap_or(19),
            komi: if maybe_body
                .as_ref()
                .and_then(|j| j.rules.clone())
                .unwrap_or_else(|| "chinese".to_string())
                .eq_ignore_ascii_case("chinese")
            {
                7.5
            } else {
                maybe_body.as_ref().and_then(|j| j.komi).unwrap_or(6.5)
            },
        },
    );

    // 若人类执白，AI 需先手（B）
    let mut first_move: Option<String> = None;
    if player_color == "white" {
        if let Some(ref e) = engine {
            if let Ok(resp) = e.send_command("genmove B").await {
                first_move = Some(parse_gtp_move(&resp));
            }
        } else {
            first_move = Some("Q16".to_string()); // 占位
        }
    }

    let expires = now + state.game_ttl_seconds;
    let res = NewGameResponse {
        gameId: game_id,
        expiresAt: expires,
        activeGames: active + 1,
        engineMove: first_move,
    };
    let mut resp = (StatusCode::CREATED, Json(res)).into_response();
    if let Some(sc) = set_cookie {
        resp.headers_mut().insert("set-cookie", sc);
    }
    resp
}

#[derive(serde::Deserialize)]
struct GameIdPayload {
    gameId: String,
}

#[derive(serde::Deserialize)]
struct PlayPayload {
    gameId: String,
    playerMove: String,
}

async fn game_heartbeat(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<GameIdPayload>,
) -> impl IntoResponse {
    if let Some(mut gs) = state.game_store.get_mut(&payload.gameId) {
        gs.last_active_at = time::OffsetDateTime::now_utc().unix_timestamp();
        drop(gs);
        return StatusCode::NO_CONTENT;
    }
    StatusCode::GONE
}

async fn game_close(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<GameIdPayload>,
) -> impl IntoResponse {
    // 从 game_store 移除
    if let Some(gs) = state.game_store.remove(&payload.gameId) {
        if let Some(engine) = gs.1.engine.as_ref() {
            let _ = engine.quit().await;
        }
        let sid = gs.1.sid;
        if let Some(mut entry) = state.session_store.get_mut(&sid) {
            entry.retain(|g| g != &payload.gameId);
        }
        return StatusCode::NO_CONTENT;
    }
    StatusCode::NO_CONTENT
}

fn get_or_create_sid(headers: HeaderMap) -> (String, Option<HeaderValue>) {
    // 读取 cookie
    let mut sid: Option<String> = None;
    if let Some(cookie_hdr) = headers.get("cookie") {
        if let Ok(s) = cookie_hdr.to_str() {
            for part in s.split(';') {
                let kv = part.trim();
                if let Some(rest) = kv.strip_prefix("sid=") {
                    sid = Some(rest.to_string());
                    break;
                }
            }
        }
    }
    if let Some(sid) = sid {
        return (sid, None);
    }

    // 生成并返回 Set-Cookie
    let new_sid = uuid::Uuid::new_v4().to_string();
    let cookie = format!("sid={}; Path=/; HttpOnly; SameSite=Lax", new_sid);
    let val = HeaderValue::from_str(&cookie).ok();
    (new_sid, val)
}

async fn game_play(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<PlayPayload>,
) -> impl IntoResponse {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    // 读取必要信息后释放 guard，避免跨 await 持有 DashMap 锁
    let (engine_opt, human_is_black) =
        if let Some(mut gs) = state.game_store.get_mut(&payload.gameId) {
            gs.last_active_at = now;
            (gs.engine.clone(), gs.human_color == "black")
        } else {
            return (
                StatusCode::GONE,
                Json(serde_json::json!({"error":"GAME_EXPIRED"})),
            );
        };

    if let Some(engine) = engine_opt {
        let human_color = if human_is_black { 'B' } else { 'W' };
        let ai_color = if human_is_black { 'W' } else { 'B' };
        let _ = engine
            .send_command(&format!("play {} {}", human_color, payload.playerMove))
            .await;
        match engine.send_command(&format!("genmove {}", ai_color)).await {
            Ok(resp) => {
                let mv = parse_gtp_move(&resp);
                let body = serde_json::json!({
                    "engineMove": mv,
                    "captures": [],
                    "end": {"finished": false}
                });
                return (StatusCode::OK, Json(body));
            }
            Err(err) => {
                tracing::error!(?err, "genmove failed");
            }
        }
    }
    // 占位：无引擎时固定应手
    let body = serde_json::json!({
        "engineMove": "Q16",
        "captures": [],
        "end": {"finished": false}
    });
    (StatusCode::OK, Json(body))
}

#[derive(serde::Serialize)]
struct HintResponse {
    suggestion: String,
}

// 为当前人类一方给出建议一手（不改变引擎棋局状态），仅返回坐标
async fn game_hint(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<GameIdPayload>,
) -> impl IntoResponse {
    // 读取必要信息
    let (engine_opt, human_is_black) = if let Some(gs) = state.game_store.get(&payload.gameId) {
        (gs.engine.clone(), gs.human_color == "black")
    } else {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({"error":"GAME_EXPIRED"})),
        );
    };

    if let Some(engine) = engine_opt {
        let human_color = if human_is_black { 'B' } else { 'W' };
        // 使用 genmove + undo，仅提供坐标
        match engine
            .send_command(&format!("genmove {}", human_color))
            .await
        {
            Ok(resp) => {
                let mv = parse_gtp_move(&resp);
                let mv_lc = mv.to_ascii_lowercase();
                if mv_lc != "pass" && mv_lc != "resign" {
                    let _ = engine.send_command("undo").await;
                }
                let body = HintResponse { suggestion: mv };
                let val = serde_json::to_value(body)
                    .unwrap_or_else(|_| serde_json::json!({"suggestion":""}));
                return (StatusCode::OK, Json(val));
            }
            Err(err) => {
                tracing::error!(?err, "genmove for hint failed");
            }
        }
    }
    // 无引擎/失败占位
    {
        let body = HintResponse {
            suggestion: "Q16".to_string(),
        };
        let val =
            serde_json::to_value(body).unwrap_or_else(|_| serde_json::json!({"suggestion":"Q16"}));
        (StatusCode::OK, Json(val))
    }
}

fn parse_gtp_move(resp: &str) -> String {
    // 取第一行，去掉开头的"="与空格
    let line = resp.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let s = line.trim_start_matches('=').trim();
    s.to_string()
}

#[derive(serde::Serialize)]
struct ScoreDetailResponse {
    result: String,    // e.g. "B+2.5" / "W+7.5" / "—"
    dead: Vec<String>, // dead stones positions in GTP coords
    boardSize: u32,
    komi: f32,
}

#[derive(serde::Deserialize)]
struct ScoreDetailRequest {
    gameId: String,
}

// 合并：返回 final_score 结果 + 死子列表 + 棋盘参数（供前端自行计算双方分）
async fn game_score_detail(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ScoreDetailRequest>,
) -> impl IntoResponse {
    let (engine, board_size, komi) = if let Some(gs) = state.game_store.get(&payload.gameId) {
        (gs.engine.clone(), gs.board_size, gs.komi)
    } else {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({"error":"GAME_EXPIRED"})),
        );
    };

    // 无引擎：返回占位信息
    if engine.is_none() {
        let body = ScoreDetailResponse {
            result: "—".to_string(),
            dead: vec![],
            boardSize: board_size,
            komi,
        };
        return (StatusCode::OK, Json(serde_json::to_value(body).unwrap()));
    }
    let e = engine.unwrap();

    // 1) final_status_list dead
    let mut dead: Vec<String> = Vec::new();
    if let Ok(resp) = e.send_command("final_status_list dead").await {
        let mut acc = String::new();
        for line in resp.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let t = t.trim_start_matches('=');
            acc.push(' ');
            acc.push_str(t);
        }
        for tok in acc.split_whitespace() {
            let v = tok.trim();
            if v.is_empty() {
                continue;
            }
            if v.eq_ignore_ascii_case("pass") {
                continue;
            }
            dead.push(v.to_string());
        }
    }

    // 2) final_score，带回退的兜底
    let mut result_str = String::new();
    if let Ok(resp) = e.send_command("final_score").await {
        let line = resp
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        let s = line.trim_start_matches('=').trim();
        if !s.is_empty() {
            result_str = s.to_string();
        }
    }
    if result_str.is_empty() {
        let mut applied: u32 = 0;
        for cmd in ["play B pass", "play W pass", "play B pass", "play W pass"].iter() {
            if applied >= 2 {
                break;
            }
            if e.send_command(cmd).await.is_ok() {
                applied += 1;
            }
        }
        if let Ok(resp) = e.send_command("final_score").await {
            let line = resp
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            result_str = line.trim_start_matches('=').trim().to_string();
        }
        for _ in 0..applied {
            let _ = e.send_command("undo").await;
        }
        if result_str.is_empty() {
            result_str = "—".to_string();
        }
    }

    let body = ScoreDetailResponse {
        result: result_str,
        dead,
        boardSize: board_size,
        komi,
    };
    (StatusCode::OK, Json(serde_json::to_value(body).unwrap()))
}

fn overrides_for_level(level: u8) -> Vec<(&'static str, String)> {
    // 更细分的 5 档难度：搜索预算 + 随机性 + 认输策略
    // 目标：低档更友好/更有趣（更随机、不轻易认输），高档更强/求最优
    let (
        visits,
        time_sec,
        root_temp,
        chosen_early,
        chosen_halflife,
        allow_resign,
        resign_threshold,
    ) = match level {
        1 => (
            80,    // 搜索访问数
            0.35,  // 时间上限（秒）
            1.6,   // 根温度：更活泼
            0.95,  // 前期选点温度：更随机
            30,    // 温度衰减半衰期（手数）更长
            false, // 不允许认输
            -0.99, // 阈值占位（无效，因为不允许认输）
        ),
        2 => (220, 0.55, 1.1, 0.8, 26, false, -0.99),
        3 => (
            650, 1.1, 0.6, 0.6, 19, true, -0.97, // 不要太早投降
        ),
        4 => (2200, 2.2, 0.25, 0.35, 15, true, -0.93),
        _ => (
            // 稳版 5★：在 4★ 基础上小幅提升预算，其他保持一致，优先稳定
            3000, // maxVisits（4★为 2200）
            2.5,  // maxTime（4★为 2.2）
            0.25, // 与 4★ 相同
            0.35, // 与 4★ 相同
            15,   // 与 4★ 相同
            true, -0.93,
        ),
    };

    let mut v = vec![
        ("maxVisits", visits.to_string()),
        ("maxTime", format!("{:.2}", time_sec)),
        ("rootPolicyTemperature", format!("{:.2}", root_temp)),
        ("chosenMoveTemperatureEarly", format!("{:.2}", chosen_early)),
        ("chosenMoveTemperatureHalflife", chosen_halflife.to_string()),
        ("allowResignation", allow_resign.to_string()),
    ];
    if allow_resign {
        v.push(("resignThreshold", format!("{:.2}", resign_threshold)));
    }
    v
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewImportResponse {
    review_id: String,
    board_size: u32,
    komi: f32,
    meta: review::GameMeta,
    initial_setup: review::InitialSetup,
    final_stones: review::BoardStones,
    moves: Vec<review::MoveNode>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewImportUrlRequest {
    source_url: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewAnalyzeRequest {
    review_id: String,
    move_index: u32,
    #[serde(default)]
    max_visits: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewAnalyzeResponse {
    review_id: String,
    move_index: u32,
    analysis: review::KataAnalysis,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExerciseSaveRequest {
    review_id: String,
    move_index: u32,
    category: String,
    #[serde(default)]
    answer: Option<ExerciseAnswerRequest>,
    #[serde(default)]
    include_raw_sgf: Option<bool>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExerciseSaveResponse {
    exercise_id: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnswerAlternativeRequest {
    moves: Vec<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    winrate: Option<f32>,
    #[serde(default)]
    score_lead: Option<f32>,
    #[serde(default)]
    visits: Option<u32>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum ExerciseAnswerRequest {
    SgfMainline {
        #[serde(default)]
        length: Option<u32>,
        #[serde(default)]
        alternatives: Vec<AnswerAlternativeRequest>,
    },
    Katago {
        pv: Vec<String>,
        #[serde(default)]
        winrate: Option<f32>,
        #[serde(default)]
        score_lead: Option<f32>,
        #[serde(default)]
        visits: Option<u32>,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        alternatives: Vec<AnswerAlternativeRequest>,
    },
    Manual {
        primary: Vec<String>,
        #[serde(default)]
        alternatives: Vec<AnswerAlternativeRequest>,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExercisePayload {
    exercise_id: String,
    category: String,
    created_at: String,
    board_size: u32,
    komi: f32,
    move_index: u32,
    initial_setup: review::InitialSetup,
    question: ExerciseQuestion,
    answer: ExerciseAnswer,
    #[serde(skip_serializing_if = "Option::is_none")]
    analysis: Option<ExerciseAnalysis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_sgf: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExerciseQuestion {
    to_play: review::StoneColor,
    stones: review::BoardStones,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExerciseAnswer {
    source: String,
    primary: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_label: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    alternatives: Vec<AnswerAlternative>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnswerAlternative {
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    moves: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    winrate: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score_lead: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visits: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExerciseAnalysis {
    #[serde(skip_serializing_if = "Option::is_none")]
    katago: Option<KatagoAnalysisEntry>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct KatagoAnalysisEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    winrate: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score_lead: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visits: Option<u32>,
    pv: Vec<String>,
}

#[derive(Clone, Copy)]
enum ExerciseCategoryKind {
    Beginner,
    Advanced,
}

impl ExerciseCategoryKind {
    fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "beginner" => Some(Self::Beginner),
            "advanced" => Some(Self::Advanced),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Beginner => "beginner",
            Self::Advanced => "advanced",
        }
    }
}

const MAX_SGF_BYTES: usize = 1_048_576; // 1 MiB 上限，防止异常大文件

async fn review_import(
    State(state): State<Arc<AppState>>,
    mut req: Request<axum::body::Body>,
) -> impl IntoResponse {
    let headers = req.headers().clone();
    let (sid, set_cookie) = get_or_create_sid(headers.clone());

    enum IncomingSource {
        Local(Vec<u8>),
        Remote(String),
    }

    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let source = if content_type.contains("multipart/form-data") {
        match Multipart::from_request(req, &state).await {
            Ok(mut multipart) => {
                let mut file_bytes: Option<Vec<u8>> = None;
                let mut remote_url: Option<String> = None;
                loop {
                    let next = match multipart.next_field().await {
                        Ok(opt) => opt,
                        Err(err) => {
                            tracing::warn!(?err, "multipart parse error");
                            return with_cookie(
                                (
                                    StatusCode::BAD_REQUEST,
                                    Json(serde_json::json!({"error":"INVALID_MULTIPART"})),
                                )
                                    .into_response(),
                                set_cookie,
                            );
                        }
                    };
                    let Some(field) = next else {
                        break;
                    };
                    let name = field.name().unwrap_or("");
                    if name == "sgf_file" {
                        match field.bytes().await {
                            Ok(bytes) => {
                                file_bytes = Some(bytes.to_vec());
                            }
                            Err(err) => {
                                tracing::warn!(?err, "failed to read sgf_file field");
                                return with_cookie(
                                    (
                                        StatusCode::BAD_REQUEST,
                                        Json(serde_json::json!({"error":"INVALID_FILE_FIELD"})),
                                    )
                                        .into_response(),
                                    set_cookie,
                                );
                            }
                        }
                    } else if name == "source_url" {
                        match field.text().await {
                            Ok(text) => {
                                let trimmed = text.trim();
                                if !trimmed.is_empty() {
                                    remote_url = Some(trimmed.to_string());
                                }
                            }
                            Err(err) => {
                                tracing::warn!(?err, "failed to read source_url field");
                            }
                        }
                    }
                }
                match (file_bytes, remote_url) {
                    (Some(bytes), _) => IncomingSource::Local(bytes),
                    (None, Some(url)) => IncomingSource::Remote(url),
                    _ => {
                        return with_cookie(
                            (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({"error":"SGF_FILE_REQUIRED"})),
                            )
                                .into_response(),
                            set_cookie,
                        );
                    }
                }
            }
            Err(err) => {
                tracing::warn!(?err, "multipart extractor failed");
                return with_cookie(
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error":"INVALID_MULTIPART"})),
                    )
                        .into_response(),
                    set_cookie,
                );
            }
        }
    } else {
        let body_bytes = match req.into_body().collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(err) => {
                tracing::warn!(?err, "failed to read json body");
                return with_cookie(
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error":"INVALID_JSON"})),
                    )
                        .into_response(),
                    set_cookie,
                );
            }
        };
        if body_bytes.is_empty() {
            return with_cookie(
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"EMPTY_BODY"})),
                )
                    .into_response(),
                set_cookie,
            );
        }
        let req: ReviewImportUrlRequest = match serde_json::from_slice(body_bytes.as_ref()) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(?err, "failed to parse json body");
                return with_cookie(
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error":"INVALID_JSON"})),
                    )
                        .into_response(),
                    set_cookie,
                );
            }
        };
        let url = req.source_url.trim().to_string();
        if url.is_empty() {
            return with_cookie(
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"SOURCE_URL_REQUIRED"})),
                )
                    .into_response(),
                set_cookie,
            );
        }
        IncomingSource::Remote(url)
    };

    let (raw_bytes, review_source) = match source {
        IncomingSource::Local(bytes) => {
            if bytes.len() > MAX_SGF_BYTES {
                return with_cookie(
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error":"SGF_TOO_LARGE"})),
                    )
                        .into_response(),
                    set_cookie,
                );
            }
            (bytes, review::ReviewSource::LocalUpload)
        }
        IncomingSource::Remote(url) => match fetch_remote_sgf(&url).await {
            Ok(bytes) => (bytes, review::ReviewSource::RemoteUrl(url)),
            Err(code) => {
                return with_cookie(
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error": code})),
                    )
                        .into_response(),
                    set_cookie,
                );
            }
        },
    };

    let sgf_text = match String::from_utf8(raw_bytes) {
        Ok(s) => s,
        Err(_) => {
            return with_cookie(
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"SGF_NOT_UTF8"})),
                )
                    .into_response(),
                set_cookie,
            );
        }
    };

    let parsed = match review::parser::parse_sgf(&sgf_text) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(?err, "sgf parse failed");
            return with_cookie(
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"SGF_PARSE_FAILED"})),
                )
                    .into_response(),
                set_cookie,
            );
        }
    };

    let review_state =
        review::ReviewState::from_parsed(sid.clone(), sgf_text, review_source, parsed);

    let review_id = format!("r-{}", uuid::Uuid::new_v4());
    let response_payload = ReviewImportResponse {
        review_id: review_id.clone(),
        board_size: review_state.board_size,
        komi: review_state.komi,
        meta: review_state.meta.clone(),
        initial_setup: review_state.initial_setup.clone(),
        final_stones: review_state.final_stones.clone(),
        moves: review_state.moves.clone(),
    };

    state.review_store.insert(review_id.clone(), review_state);

    let resp = (StatusCode::OK, Json(response_payload)).into_response();
    with_cookie(resp, set_cookie)
}

async fn review_analyze(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<ReviewAnalyzeRequest>,
) -> impl IntoResponse {
    let (sid, set_cookie) = get_or_create_sid(headers);

    let move_index_usize = payload.move_index as usize;
    let mut cached: Option<review::KataAnalysis> = None;
    let mut analysis_lock_opt = None;
    let mut raw_sgf = String::new();
    let mut engine_opt = None;
    let mut to_play = review::StoneColor::Black;

    {
        let mut review_entry = match state.review_store.get_mut(&payload.review_id) {
            Some(entry) => entry,
            None => {
                return with_cookie(
                    (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({"error":"REVIEW_NOT_FOUND"})),
                    )
                        .into_response(),
                    set_cookie,
                );
            }
        };

        if review_entry.sid != sid {
            return with_cookie(
                (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error":"REVIEW_NOT_OWNED"})),
                )
                    .into_response(),
                set_cookie,
            );
        }
        if move_index_usize > review_entry.moves.len() {
            return with_cookie(
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"MOVE_INDEX_OUT_OF_RANGE"})),
                )
                    .into_response(),
                set_cookie,
            );
        }
        review_entry.touch();
        if let Some(existing) = review_entry.analysis_cache.get(&payload.move_index) {
            cached = Some(existing.clone());
        } else {
            analysis_lock_opt = Some(review_entry.analysis_lock.clone());
            raw_sgf = review_entry.raw_sgf.clone();
            engine_opt = review_entry.engine.clone();
            to_play = next_player_to_move(
                &review_entry.initial_setup,
                &review_entry.moves,
                move_index_usize,
            );
        }
    }

    if let Some(analysis) = cached {
        let response = ReviewAnalyzeResponse {
            review_id: payload.review_id,
            move_index: payload.move_index,
            analysis,
        };
        return with_cookie((StatusCode::OK, Json(response)).into_response(), set_cookie);
    }

    let analysis_lock = match analysis_lock_opt {
        Some(lock) => lock,
        None => {
            return with_cookie(
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error":"REVIEW_STATE_CORRUPTED"})),
                )
                    .into_response(),
                set_cookie,
            );
        }
    };

    let mut engine = engine_opt;
    if engine.is_none() {
        match start_review_engine().await {
            Ok(new_engine) => {
                engine = Some(new_engine.clone());
                if let Some(mut entry) = state.review_store.get_mut(&payload.review_id) {
                    if entry.sid == sid {
                        entry.engine = Some(new_engine);
                    }
                }
            }
            Err(err) => {
                tracing::warn!(?err, "failed to start review engine");
                return with_cookie(
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(serde_json::json!({"error":"ENGINE_UNAVAILABLE"})),
                    )
                        .into_response(),
                    set_cookie,
                );
            }
        }
    }

    let engine = match engine {
        Some(e) => e,
        None => {
            return with_cookie(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error":"ENGINE_UNAVAILABLE"})),
                )
                    .into_response(),
                set_cookie,
            );
        }
    };

    let visit_limit = payload.max_visits.unwrap_or(400).clamp(50, 5000);

    let guard = analysis_lock.lock().await;
    let prepare_result =
        load_review_position(&engine, &payload.review_id, &raw_sgf, payload.move_index).await;
    if let Err(err) = prepare_result {
        tracing::warn!(?err, "failed to prepare review position");
        drop(guard);
        return with_cookie(
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":"FAILED_TO_PREPARE_POSITION"})),
            )
                .into_response(),
            set_cookie,
        );
    }

    let color_char = match to_play {
        review::StoneColor::Black => 'B',
        review::StoneColor::White => 'W',
    };
    let cmd = format!("kata-analyze {} {}", color_char, visit_limit);
    let raw = match engine.send_command(&cmd).await {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(?err, "kata-analyze command failed");
            drop(guard);
            return with_cookie(
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"ENGINE_ANALYZE_FAILED"})),
                )
                    .into_response(),
                set_cookie,
            );
        }
    };
    drop(guard);

    let analysis = match parse_kata_analyze(&raw) {
        Some(a) => a,
        None => {
            tracing::warn!(raw, "kata-analyze response unparseable");
            return with_cookie(
                (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error":"ENGINE_ANALYZE_UNPARSEABLE"})),
                )
                    .into_response(),
                set_cookie,
            );
        }
    };

    // 回写缓存
    if let Some(mut entry) = state.review_store.get_mut(&payload.review_id) {
        if entry.sid == sid {
            entry
                .analysis_cache
                .insert(payload.move_index, analysis.clone());
            entry.touch();
        }
    }

    let response = ReviewAnalyzeResponse {
        review_id: payload.review_id,
        move_index: payload.move_index,
        analysis,
    };
    with_cookie((StatusCode::OK, Json(response)).into_response(), set_cookie)
}

async fn exercise_save(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<ExerciseSaveRequest>,
) -> impl IntoResponse {
    let (sid, set_cookie) = get_or_create_sid(headers);
    let include_raw_sgf = payload.include_raw_sgf.unwrap_or(true);

    let answer_request = match payload.answer {
        Some(answer) => answer,
        None => {
            return error_response(StatusCode::BAD_REQUEST, "ANSWER_REQUIRED", None, set_cookie);
        }
    };
    let category = match ExerciseCategoryKind::parse(&payload.category) {
        Some(cat) => cat,
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "INVALID_CATEGORY",
                None,
                set_cookie,
            );
        }
    };

    let move_index_usize = payload.move_index as usize;

    let (board_size, komi, initial_setup, moves, raw_sgf, cached_analysis) = {
        let mut review_entry = match state.review_store.get_mut(&payload.review_id) {
            Some(entry) => entry,
            None => {
                return error_response(StatusCode::NOT_FOUND, "REVIEW_NOT_FOUND", None, set_cookie);
            }
        };
        if review_entry.sid != sid {
            return error_response(StatusCode::FORBIDDEN, "REVIEW_NOT_OWNED", None, set_cookie);
        }
        if move_index_usize > review_entry.moves.len() {
            return error_response(
                StatusCode::BAD_REQUEST,
                "MOVE_INDEX_OUT_OF_RANGE",
                None,
                set_cookie,
            );
        }
        review_entry.touch();
        (
            review_entry.board_size,
            review_entry.komi,
            review_entry.initial_setup.clone(),
            review_entry.moves.clone(),
            review_entry.raw_sgf.clone(),
            review_entry
                .analysis_cache
                .get(&payload.move_index)
                .cloned(),
        )
    };

    let question_stones = match review::parser::board_stones_after(
        board_size as usize,
        &initial_setup,
        &moves,
        move_index_usize,
    ) {
        Ok(stones) => stones,
        Err(err) => {
            tracing::warn!(?err, "failed to build stones for exercise");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "FAILED_TO_BUILD_POSITION",
                None,
                set_cookie,
            );
        }
    };

    let to_play = next_player_to_move(&initial_setup, &moves, move_index_usize);

    let mut analysis_opt: Option<ExerciseAnalysis> = None;
    let answer = match answer_request {
        ExerciseAnswerRequest::SgfMainline {
            length,
            alternatives,
        } => {
            let desired_len = length.unwrap_or(1);
            if desired_len == 0 {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "ANSWER_LENGTH_INVALID",
                    Some("length must be >= 1".to_string()),
                    set_cookie,
                );
            }
            let available = moves.len().saturating_sub(move_index_usize);
            if desired_len as usize > available {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "ANSWER_LENGTH_TOO_LONG",
                    Some(format!(
                        "mainline has only {} moves after index {}",
                        available, payload.move_index
                    )),
                    set_cookie,
                );
            }
            let primary: Vec<String> = moves
                .iter()
                .skip(move_index_usize)
                .take(desired_len as usize)
                .map(move_node_to_answer_coord)
                .collect();
            let alternatives = match prepare_alternatives(alternatives, board_size) {
                Ok(v) => v,
                Err(err) => return error_response(err.0, err.1, err.2, set_cookie),
            };
            ExerciseAnswer {
                source: "sgf_mainline".to_string(),
                primary,
                primary_label: None,
                alternatives,
            }
        }
        ExerciseAnswerRequest::Katago {
            pv,
            winrate,
            score_lead,
            visits,
            label,
            alternatives,
        } => {
            let primary = match normalise_user_sequence(pv, board_size) {
                Ok(seq) => seq,
                Err(err) => return error_response(err.0, err.1, err.2, set_cookie),
            };
            if primary.is_empty() {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "ANSWER_SEQUENCE_EMPTY",
                    Some("primary sequence requires at least one move".to_string()),
                    set_cookie,
                );
            }
            let alternatives = match prepare_alternatives(alternatives, board_size) {
                Ok(v) => v,
                Err(err) => return error_response(err.0, err.1, err.2, set_cookie),
            };
            let mut katago_entry = KatagoAnalysisEntry {
                winrate,
                score_lead,
                visits,
                pv: primary.clone(),
            };
            if let Some(cached) = cached_analysis {
                if katago_entry.winrate.is_none() {
                    katago_entry.winrate = Some(cached.winrate);
                }
                if katago_entry.score_lead.is_none() {
                    katago_entry.score_lead = Some(cached.score_lead);
                }
                if katago_entry.visits.is_none() || katago_entry.visits == Some(0) {
                    katago_entry.visits = Some(cached.visits);
                }
                if katago_entry.pv.is_empty() {
                    katago_entry.pv = cached.pv.clone();
                }
            }
            if katago_entry.visits == Some(0) {
                katago_entry.visits = None;
            }
            let has_metrics = katago_entry.winrate.is_some()
                || katago_entry.score_lead.is_some()
                || katago_entry.visits.is_some();
            if has_metrics || !katago_entry.pv.is_empty() {
                analysis_opt = Some(ExerciseAnalysis {
                    katago: Some(katago_entry.clone()),
                });
            }
            ExerciseAnswer {
                source: "katago".to_string(),
                primary,
                primary_label: label,
                alternatives,
            }
        }
        ExerciseAnswerRequest::Manual {
            primary,
            alternatives,
        } => {
            let primary = match normalise_user_sequence(primary, board_size) {
                Ok(seq) => seq,
                Err(err) => return error_response(err.0, err.1, err.2, set_cookie),
            };
            if primary.is_empty() {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "ANSWER_SEQUENCE_EMPTY",
                    Some("primary sequence requires at least one move".to_string()),
                    set_cookie,
                );
            }
            let alternatives = match prepare_alternatives(alternatives, board_size) {
                Ok(v) => v,
                Err(err) => return error_response(err.0, err.1, err.2, set_cookie),
            };
            ExerciseAnswer {
                source: "manual".to_string(),
                primary,
                primary_label: None,
                alternatives,
            }
        }
    };

    let digest = Sha256::digest(raw_sgf.as_bytes());
    let mut hash_hex = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        hash_hex.push_str(&format!("{:02x}", byte));
    }
    let prefix_len = hash_hex.len().min(12);
    let exercise_id = format!("ex-{}-{}", &hash_hex[..prefix_len], payload.move_index);

    let created_at = time::OffsetDateTime::now_utc();
    let created_at_text = match created_at.format(&time::format_description::well_known::Rfc3339) {
        Ok(text) => text,
        Err(err) => {
            tracing::error!(?err, "failed to format timestamp");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "FAILED_TO_FORMAT_TIMESTAMP",
                None,
                set_cookie,
            );
        }
    };

    let question = ExerciseQuestion {
        to_play,
        stones: question_stones,
    };

    let raw_sgf_payload = if include_raw_sgf { Some(raw_sgf) } else { None };

    let payload_json = ExercisePayload {
        exercise_id: exercise_id.clone(),
        category: category.as_str().to_string(),
        created_at: created_at_text,
        board_size,
        komi,
        move_index: payload.move_index,
        initial_setup,
        question,
        answer,
        analysis: analysis_opt,
        raw_sgf: raw_sgf_payload,
    };

    let base_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join("exercises")
        .join(&exercise_id);
    if let Err(err) = fs::create_dir_all(&base_dir).await {
        tracing::error!(?err, "failed to create exercise dir");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "FAILED_TO_WRITE_EXERCISE",
            None,
            set_cookie,
        );
    }
    let file_path = base_dir.join("payload.json");
    let json_bytes = match serde_json::to_vec_pretty(&payload_json) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::error!(?err, "failed to serialize exercise payload");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "FAILED_TO_SERIALIZE_EXERCISE",
                None,
                set_cookie,
            );
        }
    };
    if let Err(err) = fs::write(&file_path, json_bytes).await {
        tracing::error!(?err, path=%file_path.display(), "failed to persist exercise");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "FAILED_TO_WRITE_EXERCISE",
            None,
            set_cookie,
        );
    }

    let response = ExerciseSaveResponse { exercise_id };
    with_cookie((StatusCode::OK, Json(response)).into_response(), set_cookie)
}

type SaveError = (StatusCode, &'static str, Option<String>);

fn error_response(
    status: StatusCode,
    code: &str,
    detail: Option<String>,
    set_cookie: Option<HeaderValue>,
) -> Response {
    let mut body = serde_json::json!({"error": code});
    if let Some(detail_text) = detail {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("detail".to_string(), detail_text.into());
        }
    }
    with_cookie((status, Json(body)).into_response(), set_cookie)
}

fn prepare_alternatives(
    alternatives: Vec<AnswerAlternativeRequest>,
    board_size: u32,
) -> Result<Vec<AnswerAlternative>, SaveError> {
    let mut result = Vec::with_capacity(alternatives.len());
    for (idx, alt) in alternatives.into_iter().enumerate() {
        let AnswerAlternativeRequest {
            moves,
            label,
            winrate,
            score_lead,
            visits,
        } = alt;
        let moves = normalise_user_sequence(moves, board_size)?;
        if moves.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "ANSWER_SEQUENCE_EMPTY",
                Some(format!(
                    "alternative #{} must contain at least one move",
                    idx + 1
                )),
            ));
        }
        result.push(AnswerAlternative {
            label,
            moves,
            winrate,
            score_lead,
            visits,
        });
    }
    Ok(result)
}

fn normalise_user_sequence(
    raw_moves: Vec<String>,
    board_size: u32,
) -> Result<Vec<String>, SaveError> {
    let mut result = Vec::with_capacity(raw_moves.len());
    for (idx, mv) in raw_moves.into_iter().enumerate() {
        match normalise_user_move(&mv, board_size) {
            Ok(value) => result.push(value),
            Err(detail) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "INVALID_MOVE_COORD",
                    Some(format!("move #{}: {}", idx + 1, detail)),
                ));
            }
        }
    }
    Ok(result)
}

fn normalise_user_move(value: &str, board_size: u32) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("value is empty".to_string());
    }
    if trimmed.eq_ignore_ascii_case("pass") {
        return Ok("pass".to_string());
    }
    if trimmed.len() != 2 {
        return Err(format!(
            "\"{}\" should be a two-letter SGF coordinate or 'pass'",
            trimmed
        ));
    }
    let lower = trimmed.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    if !bytes.iter().all(|b| b.is_ascii_lowercase()) {
        return Err(format!("\"{}\" must use letters a-z", trimmed));
    }
    let size = board_size as u8;
    let x = bytes[0];
    let y = bytes[1];
    if x < b'a' || y < b'a' || x >= b'a' + size || y >= b'a' + size {
        return Err(format!(
            "\"{}\" outside board (size {})",
            trimmed, board_size
        ));
    }
    Ok(lower)
}

fn move_node_to_answer_coord(node: &review::MoveNode) -> String {
    node.coord.clone().unwrap_or_else(|| "pass".to_string())
}

fn with_cookie(resp: Response, set_cookie: Option<HeaderValue>) -> Response {
    let mut response = resp;
    if let Some(val) = set_cookie {
        response.headers_mut().insert(SET_COOKIE, val);
    }
    response
}

async fn fetch_remote_sgf(url: &str) -> Result<Vec<u8>, String> {
    let uri: Uri = url.parse().map_err(|_| "INVALID_SOURCE_URL".to_string())?;
    if uri.scheme_str() != Some("https") {
        return Err("REMOTE_URL_NOT_HTTPS".to_string());
    }
    let host = uri
        .host()
        .ok_or_else(|| "REMOTE_URL_HOST_REQUIRED".to_string())?;
    if !remote_host_allowed(host) {
        return Err("REMOTE_HOST_NOT_ALLOWED".to_string());
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|err| {
            tracing::error!(?err, "failed to build reqwest client");
            "REMOTE_FETCH_FAILED".to_string()
        })?;

    let resp = client.get(url).send().await.map_err(|err| {
        tracing::warn!(?err, "failed to download remote sgf");
        "REMOTE_FETCH_FAILED".to_string()
    })?;

    if !resp.status().is_success() {
        tracing::warn!(status=?resp.status(), "remote sgf returned non-200");
        return Err("REMOTE_FETCH_FAILED".to_string());
    }

    let bytes = resp.bytes().await.map_err(|err| {
        tracing::warn!(?err, "failed to read remote sgf body");
        "REMOTE_FETCH_FAILED".to_string()
    })?;
    if bytes.len() > MAX_SGF_BYTES {
        return Err("SGF_TOO_LARGE".to_string());
    }
    Ok(bytes.to_vec())
}

fn remote_host_allowed(host: &str) -> bool {
    match std::env::var("REVIEW_IMPORT_HOST_WHITELIST") {
        Ok(list) => {
            let trimmed = list.trim();
            if trimmed.is_empty() {
                return true;
            }
            trimmed
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .any(|allowed| allowed.eq_ignore_ascii_case(host))
        }
        Err(_) => true,
    }
}

async fn start_review_engine() -> anyhow::Result<std::sync::Arc<engine::gtp::GtpEngine>> {
    let engine_path = std::env::var("ENGINE_PATH").context("ENGINE_PATH not set")?;
    let model_path = std::env::var("MODEL_PATH").context("MODEL_PATH not set")?;
    let config_path = std::env::var("GTP_CONFIG_PATH").context("GTP_CONFIG_PATH not set")?;

    let mut args = vec![
        "gtp".to_string(),
        "-model".to_string(),
        model_path,
        "-config".to_string(),
        config_path,
    ];
    for (k, v) in overrides_for_level(5) {
        args.push("-override-config".to_string());
        args.push(format!("{}={}", k, v));
    }
    args.push("-override-config".to_string());
    args.push("rules=chinese".to_string());

    let engine = engine::gtp::GtpEngine::start(&engine_path, &args)
        .await
        .context("failed to spawn katago")?;
    Ok(engine)
}

async fn load_review_position(
    engine: &std::sync::Arc<engine::gtp::GtpEngine>,
    review_id: &str,
    raw_sgf: &str,
    move_index: u32,
) -> anyhow::Result<()> {
    let temp_dir = std::env::temp_dir().join("katago_review_cache");
    fs::create_dir_all(&temp_dir)
        .await
        .context("failed to create temp dir for sgf cache")?;
    let file_path = temp_dir.join(format!("{}.sgf", review_id));
    fs::write(&file_path, raw_sgf.as_bytes())
        .await
        .with_context(|| format!("failed to write sgf cache: {}", file_path.display()))?;
    let path_text = file_path
        .to_str()
        .ok_or_else(|| anyhow!("temporary SGF path not valid UTF-8"))?;
    let cmd = if move_index == 0 {
        format!("loadsgf {}", path_text)
    } else {
        format!("loadsgf {} {}", path_text, move_index)
    };
    engine
        .send_command(&cmd)
        .await
        .context("loadsgf command failed")?;
    Ok(())
}

fn parse_kata_analyze(raw: &str) -> Option<review::KataAnalysis> {
    let line = raw
        .lines()
        .find(|l| l.trim_start().starts_with("info"))?
        .trim();
    let mut winrate = None;
    let mut score_lead = None;
    let mut visits = None;
    let mut pv: Vec<String> = Vec::new();

    let mut tokens = line.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        match token {
            "winrate" => {
                if let Some(value) = tokens.next() {
                    winrate = value.parse::<f32>().ok();
                }
            }
            "scoreLead" | "scorelead" => {
                if let Some(value) = tokens.next() {
                    score_lead = value.parse::<f32>().ok();
                }
            }
            "visits" => {
                if let Some(value) = tokens.next() {
                    visits = value.parse::<u32>().ok();
                }
            }
            "pv" => {
                pv = tokens.map(|t| t.to_string()).collect();
                break;
            }
            _ => {}
        }
    }

    Some(review::KataAnalysis {
        winrate: winrate.unwrap_or(0.5),
        score_lead: score_lead.unwrap_or(0.0),
        pv,
        visits: visits.unwrap_or(0),
    })
}

fn next_player_to_move(
    setup: &review::InitialSetup,
    moves: &[review::MoveNode],
    move_index: usize,
) -> review::StoneColor {
    if move_index == 0 {
        return setup.to_play.unwrap_or(review::StoneColor::Black);
    }
    moves
        .get(move_index - 1)
        .map(|m| m.color.opponent())
        .unwrap_or_else(|| setup.to_play.unwrap_or(review::StoneColor::Black))
}
