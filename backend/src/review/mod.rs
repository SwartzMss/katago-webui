use crate::engine::gtp::GtpEngine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

pub mod parser;

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct ReviewState {
    pub sid: String,
    pub created_at: i64,
    pub last_active_at: i64,
    pub board_size: u32,
    pub komi: f32,
    pub meta: GameMeta,
    pub moves: Vec<MoveNode>,
    pub initial_setup: InitialSetup,
    pub final_stones: BoardStones,
    pub source: ReviewSource,
    pub raw_sgf: String,
    pub analysis_cache: HashMap<u32, KataAnalysis>,
    pub engine: Option<Arc<GtpEngine>>,
    pub analysis_lock: Arc<tokio::sync::Mutex<()>>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GameMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub black: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub komi: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MoveNode {
    pub index: u32,
    pub color: StoneColor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coord: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum StoneColor {
    Black,
    White,
}

#[allow(dead_code)]
impl StoneColor {
    pub fn opponent(self) -> Self {
        match self {
            StoneColor::Black => StoneColor::White,
            StoneColor::White => StoneColor::Black,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InitialSetup {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub black: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub white: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub empty: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_play: Option<StoneColor>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BoardStones {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub black: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub white: Vec<String>,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct ParsedReview {
    pub board_size: u32,
    pub komi: f32,
    pub meta: GameMeta,
    pub moves: Vec<MoveNode>,
    pub initial_setup: InitialSetup,
    pub final_stones: BoardStones,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KataAnalysis {
    pub winrate: f32,
    pub score_lead: f32,
    pub pv: Vec<String>,
    pub visits: u32,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum ReviewSource {
    LocalUpload,
    RemoteUrl(String),
}

#[allow(dead_code)]
impl ReviewState {
    pub fn from_parsed(
        sid: String,
        raw_sgf: String,
        source: ReviewSource,
        parsed: ParsedReview,
    ) -> Self {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        Self {
            sid,
            created_at: now,
            last_active_at: now,
            board_size: parsed.board_size,
            komi: parsed.komi,
            meta: parsed.meta,
            moves: parsed.moves,
            initial_setup: parsed.initial_setup,
            final_stones: parsed.final_stones,
            source,
            raw_sgf,
            analysis_cache: HashMap::new(),
            engine: None,
            analysis_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn touch(&mut self) {
        self.last_active_at = time::OffsetDateTime::now_utc().unix_timestamp();
    }
}
