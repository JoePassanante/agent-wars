use std::sync::Arc;

use agent_wars::game::{
    GAME_VERSION, MAP_GENERATOR_VERSION, Map, PlayerId, RememberedBuilding, Unit, View,
};
use agent_wars::lobby::{AppState, Replay};
use agent_wars::ws;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use serde::Serialize;
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,agent_wars=debug")),
        )
        .init();

    let state = AppState::new();

    let app = Router::new()
        .route("/ws", get(ws::ws_handler))
        .route("/api/sessions", get(list_sessions))
        .route("/api/replays", get(list_replays))
        .route("/api/replays/:id", get(get_replay))
        .route("/api/version", get(server_version))
        .fallback_service(ServeDir::new("web"))
        .with_state(state);

    let addr = "127.0.0.1:8080";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!(
        "agent-wars {} (map_gen v{}) listening on http://{addr}",
        GAME_VERSION,
        MAP_GENERATOR_VERSION,
    );
    axum::serve(listener, app).await.unwrap();
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionSummary {
    id: uuid::Uuid,
    map_seed: u64,
    map_width: i32,
    map_height: i32,
    turn_number: u32,
    current_turn: PlayerId,
    has_winner: bool,
    winner: Option<PlayerId>,
    p1_units: usize,
    p2_units: usize,
    started_at_secs: u64,
    /// Live map snapshot + unit/building positions for spectator-style
    /// preview rendering on the lobby page. Same data a fresh spectator
    /// would receive in a State message.
    map: Map,
    units: Vec<Unit>,
    buildings: Vec<RememberedBuilding>,
}

async fn list_sessions(State(state): State<AppState>) -> Json<Vec<SessionSummary>> {
    let lobby = state.lobby.lock().await;
    let mut out = Vec::new();
    for s in lobby.sessions.values() {
        let mut g = s.game.lock().await;
        let p1_units = g.units.values().filter(|u| u.owner == PlayerId::P1).count();
        let p2_units = g.units.values().filter(|u| u.owner == PlayerId::P2).count();
        let replay_started_at = s.replay.lock().await.started_at;
        let view = g.view_for(View::Spectator);
        out.push(SessionSummary {
            id: s.id,
            map_seed: g.map_seed,
            map_width: g.map.width,
            map_height: g.map.height,
            turn_number: g.turn_number,
            current_turn: g.current_turn,
            has_winner: g.winner.is_some(),
            winner: g.winner,
            p1_units,
            p2_units,
            started_at_secs: replay_started_at,
            map: view.map,
            units: view.units,
            buildings: view.buildings,
        });
    }
    Json(out)
}

#[derive(Serialize)]
struct ReplaySummary {
    id: uuid::Uuid,
    map_seed: u64,
    map_generator_version: u32,
    game_version: String,
    started_at: u64,
    ended_at: Option<u64>,
    winner: Option<PlayerId>,
    event_count: usize,
}

async fn list_replays() -> Result<Json<Vec<ReplaySummary>>, (StatusCode, String)> {
    let dir = std::path::Path::new("replays");
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return Ok(Json(Vec::new())),
    };
    let mut out = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = tokio::fs::read(&path).await.ok();
        let Some(bytes) = bytes else { continue };
        let Ok(replay) = serde_json::from_slice::<Replay>(&bytes) else {
            continue;
        };
        out.push(ReplaySummary {
            id: replay.session_id,
            map_seed: replay.map_seed,
            map_generator_version: replay.map_generator_version,
            game_version: replay.game_version,
            started_at: replay.started_at,
            ended_at: replay.ended_at,
            winner: replay.winner,
            event_count: replay.events.len(),
        });
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    Ok(Json(out))
}

async fn get_replay(Path(id): Path<uuid::Uuid>) -> Result<Json<Replay>, (StatusCode, String)> {
    let path = std::path::Path::new("replays").join(format!("{id}.json"));
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, format!("no replay {id}")))?;
    let replay: Replay = serde_json::from_slice(&bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(replay))
}

#[derive(Serialize)]
struct VersionInfo {
    game_version: &'static str,
    map_generator_version: u32,
}

async fn server_version() -> Json<VersionInfo> {
    let _ = Arc::<u8>::default; // touch Arc so the import isn't unused
    Json(VersionInfo {
        game_version: GAME_VERSION,
        map_generator_version: MAP_GENERATOR_VERSION,
    })
}
