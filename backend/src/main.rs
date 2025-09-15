mod engine;

use axum::{
    extract::{FromRef, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use tower_http::{cors::{Any, CorsLayer}, services::ServeDir};
use std::path::Path;
use serde::Serialize;
use std::{net::SocketAddr, sync::Arc};
use tokio::signal;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};
use tracing_appender::rolling;
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
    game_ttl_seconds: i64,
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

#[derive(Clone)]
struct GameState {
    sid: String,
    last_active_at: i64,
    engine: Option<std::sync::Arc<engine::gtp::GtpEngine>>, // None 时使用占位行为
    human_color: String, // "black" or "white"
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

    let port: u16 = std::env::var("PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(8080);
    let concurrency_limit_per_sid: u32 = std::env::var("CONCURRENCY_PER_SID").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let game_ttl_minutes: i64 = std::env::var("GAME_TTL_MINUTES").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
    let game_ttl_seconds = game_ttl_minutes * 60;

    let state = Arc::new(AppState {
        concurrency_limit_per_sid,
        session_store: Arc::new(dashmap::DashMap::new()),
        game_store: Arc::new(dashmap::DashMap::new()),
        game_ttl_seconds,
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
        .route("/api/game/close", post(game_close));

    let static_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap() // project root
        .join("frontend/public");

    let app = Router::new()
        .nest("/", api)
        .fallback_service(axum::routing::get_service(ServeDir::new(static_dir)).handle_error(|err| async move {
            tracing::error!(?err, "serve static error");
            StatusCode::INTERNAL_SERVER_ERROR
        }))
        .layer(cors)
        .with_state(state.clone());

    let addr: SocketAddr = SocketAddr::from(([0,0,0,0], port));
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
                        tokio::spawn(async move { let _ = e.quit().await; });
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
    for e in engines { let _ = e.quit().await; }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
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
        if let Some(sc) = set_cookie { resp.headers_mut().insert("set-cookie", sc); }
        return resp;
    }

    // 生成 gameId 并登记
    let game_id = format!("g-{}", uuid::Uuid::new_v4());
    let now = time::OffsetDateTime::now_utc().unix_timestamp();

    // 若环境配置齐全则尝试启动引擎，否则置为 None（占位）
    let engine = match (std::env::var("ENGINE_PATH"), std::env::var("MODEL_PATH"), std::env::var("GTP_CONFIG_PATH")) {
        (Ok(engine_path), Ok(model_path), Ok(config_path)) => {
            let mut args = vec!["gtp".to_string(), "-model".to_string(), model_path, "-config".to_string(), config_path];
            // 难度 → 覆盖配置
            let req = maybe_body.as_ref().map(|j| &j.0);
            let level = req.and_then(|r| r.engineLevel).unwrap_or(3);
            for (k,v) in overrides_for_level(level) {
                args.push("-override-config".to_string());
                args.push(format!("{}={}", k, v));
            }
            // 规则 → 覆盖配置（默认 chinese）
            let rule_name = req.and_then(|r| r.rules.clone()).unwrap_or_else(|| "chinese".to_string());
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
                    let effective_komi: f32 = if rule_name.eq_ignore_ascii_case("chinese") { 7.5 } else { komi_req };
                    let _ = e.send_command(&format!("boardsize {}", board_size)).await;
                    let _ = e.send_command(&format!("komi {}", effective_komi)).await;
                    let _ = e.send_command("clear_board").await;
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
        GameState { sid: sid.clone(), last_active_at: now, engine: engine.clone(), human_color: player_color.clone() }
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
    let res = NewGameResponse { gameId: game_id, expiresAt: expires, activeGames: active + 1, engineMove: first_move };
    let mut resp = (StatusCode::CREATED, Json(res)).into_response();
    if let Some(sc) = set_cookie { resp.headers_mut().insert("set-cookie", sc); }
    resp
}

#[derive(serde::Deserialize)]
struct GameIdPayload { gameId: String }

#[derive(serde::Deserialize)]
struct PlayPayload { gameId: String, playerMove: String }

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
        if let Some(engine) = gs.1.engine.as_ref() { let _ = engine.quit().await; }
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
    if let Some(sid) = sid { return (sid, None); }

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
    let (engine_opt, human_is_black) = if let Some(mut gs) = state.game_store.get_mut(&payload.gameId) {
        gs.last_active_at = now;
        (gs.engine.clone(), gs.human_color == "black")
    } else {
        return (StatusCode::GONE, Json(serde_json::json!({"error":"GAME_EXPIRED"})));
    };

    if let Some(engine) = engine_opt {
        let human_color = if human_is_black { 'B' } else { 'W' };
        let ai_color = if human_is_black { 'W' } else { 'B' };
        let _ = engine.send_command(&format!("play {} {}", human_color, payload.playerMove)).await;
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

fn parse_gtp_move(resp: &str) -> String {
    // 取第一行，去掉开头的"="与空格
    let line = resp.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let s = line.trim_start_matches('=').trim();
    s.to_string()
}

fn overrides_for_level(level: u8) -> Vec<(&'static str, String)> {
    // 更细分的 5 档难度：搜索预算 + 随机性 + 认输策略
    // 目标：低档更友好/更有趣（更随机、不轻易认输），高档更强/求最优
    let (visits, time_sec, root_temp, chosen_early, chosen_halflife, allow_resign, resign_threshold) = match level {
        1 => (
            80,    // 搜索访问数
            0.35,  // 时间上限（秒）
            1.6,   // 根温度：更活泼
            0.95,  // 前期选点温度：更随机
            30,    // 温度衰减半衰期（手数）更长
            false, // 不允许认输
            -0.99, // 阈值占位（无效，因为不允许认输）
        ),
        2 => (
            220,
            0.55,
            1.1,
            0.8,
            26,
            false,
            -0.99,
        ),
        3 => (
            650,
            1.1,
            0.6,
            0.6,
            19,
            true,
            -0.97, // 不要太早投降
        ),
        4 => (
            2200,
            2.2,
            0.25,
            0.35,
            15,
            true,
            -0.93,
        ),
        _ => (
            // 稳版 5★：在 4★ 基础上小幅提升预算，其他保持一致，优先稳定
            3000,  // maxVisits（4★为 2200）
            2.5,   // maxTime（4★为 2.2）
            0.25,  // 与 4★ 相同
            0.35,  // 与 4★ 相同
            15,    // 与 4★ 相同
            true,
            -0.93,
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
