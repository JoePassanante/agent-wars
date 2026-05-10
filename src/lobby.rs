//! Multi-session lobby with FIFO matchmaking, reconnect-by-username, and
//! replay logging.
//!
//! Players "Hello" with a username and an intent (Play / Watch). Players are
//! pushed onto a queue; when ≥2 are queued the lobby creates a fresh session
//! (random map seeded by the engine), notifies both via oneshot channels,
//! and records each command into a `Replay` log.
//!
//! When a session's game ends (winner set or surrender), a GC task persists
//! the replay to `replays/<session_id>.json` and removes the session from
//! the lobby after a 60s grace period so spectators can still read the
//! final state briefly.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use uuid::Uuid;

use crate::game::{
    ActionReport, Coord, GAME_VERSION, GameState, MAP_GENERATOR_VERSION, PlayerId,
    TURN_DURATION_SECS, UnitKind,
};

pub type SessionId = Uuid;

/// A single live game session. Connections attach to a session and are
/// notified of state changes via the per-session broadcast channel.
pub struct SessionRef {
    pub id: SessionId,
    pub game: Mutex<GameState>,
    pub tx: broadcast::Sender<()>,
    /// Role → username for the two players. Spectators are anonymous.
    pub players: HashMap<PlayerId, String>,
    pub created_at: Instant,
    /// Set when the engine declares a winner; drives the GC task.
    pub finished_at: Mutex<Option<Instant>>,
    /// Append-only log for replay reconstruction.
    pub replay: Mutex<Replay>,
    /// Unix-epoch seconds when the active player's turn auto-ends. Bumped
    /// every time end_turn fires (manual or via the timer task).
    pub turn_deadline_secs: Mutex<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Replay {
    pub session_id: SessionId,
    pub map_seed: u64,
    pub map_generator_version: u32,
    pub game_version: String,
    /// Unix-epoch seconds.
    pub started_at: u64,
    pub ended_at: Option<u64>,
    /// PlayerId → username. Stored for context; replays don't need usernames
    /// to be reproducible (the seed is enough).
    pub players: HashMap<PlayerId, String>,
    pub winner: Option<PlayerId>,
    pub events: Vec<ReplayEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ReplayEvent {
    Move {
        actor: PlayerId,
        unit_id: Uuid,
        to: Coord,
        attack: Option<Coord>,
        report: ActionReport,
    },
    Buy {
        actor: PlayerId,
        factory_id: Uuid,
        kind: UnitKind,
        new_unit_id: Uuid,
    },
    EndTurn {
        actor: PlayerId,
    },
    Surrender {
        actor: PlayerId,
    },
}

#[derive(Debug)]
pub struct QueueEntry {
    pub username: String,
    pub queued_at: Instant,
}

#[derive(Debug, Clone)]
pub enum UserPresence {
    Queued,
    InSession {
        session_id: SessionId,
        role: PlayerId,
    },
}

pub struct Lobby {
    pub sessions: HashMap<SessionId, Arc<SessionRef>>,
    pub queue: VecDeque<QueueEntry>,
    /// Index for reconnect / dedup. A username present here is either
    /// queued or in a live session.
    pub user_index: HashMap<String, UserPresence>,
    /// One-shot channels keyed by username. Used to wake the queued
    /// connection task with `(session_id, role)` when a match is made.
    pub match_notifiers: HashMap<String, oneshot::Sender<(SessionId, PlayerId)>>,
}

impl Lobby {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            queue: VecDeque::new(),
            user_index: HashMap::new(),
            match_notifiers: HashMap::new(),
        }
    }

    /// Pop two queued users and create a fresh session for them. Returns
    /// the new session ref (or `None` if there weren't enough waiters) so
    /// callers can schedule the first turn's timer task.
    pub fn try_match_pair(&mut self) -> Option<Arc<SessionRef>> {
        if self.queue.len() < 2 {
            return None;
        }
        let p1 = self.queue.pop_front()?;
        let p2 = self.queue.pop_front()?;
        let session_id = Uuid::new_v4();
        let game = GameState::new();
        let map_seed = game.map_seed;
        let (tx, _) = broadcast::channel(64);
        let mut players = HashMap::new();
        players.insert(PlayerId::P1, p1.username.clone());
        players.insert(PlayerId::P2, p2.username.clone());
        let replay = Replay {
            session_id,
            map_seed,
            map_generator_version: MAP_GENERATOR_VERSION,
            game_version: GAME_VERSION.to_string(),
            started_at: now_secs(),
            ended_at: None,
            players: players.clone(),
            winner: None,
            events: Vec::new(),
        };
        let session = Arc::new(SessionRef {
            id: session_id,
            game: Mutex::new(game),
            tx,
            players,
            created_at: Instant::now(),
            finished_at: Mutex::new(None),
            replay: Mutex::new(replay),
            turn_deadline_secs: Mutex::new(now_secs() + TURN_DURATION_SECS),
        });
        self.sessions.insert(session_id, session.clone());
        self.user_index.insert(
            p1.username.clone(),
            UserPresence::InSession {
                session_id,
                role: PlayerId::P1,
            },
        );
        self.user_index.insert(
            p2.username.clone(),
            UserPresence::InSession {
                session_id,
                role: PlayerId::P2,
            },
        );
        if let Some(tx) = self.match_notifiers.remove(&p1.username) {
            let _ = tx.send((session_id, PlayerId::P1));
        }
        if let Some(tx) = self.match_notifiers.remove(&p2.username) {
            let _ = tx.send((session_id, PlayerId::P2));
        }
        Some(session)
    }

    /// Remove a queued user (e.g. on disconnect before match).
    pub fn remove_queued(&mut self, username: &str) {
        self.queue.retain(|q| q.username != username);
        self.match_notifiers.remove(username);
        if let Some(UserPresence::Queued) = self.user_index.get(username) {
            self.user_index.remove(username);
        }
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persist a replay to disk and schedule the session for removal after 60s.
/// `user_index` entries for the two players are freed IMMEDIATELY so they
/// can re-queue for a new match without waiting for GC; only the
/// `sessions` entry lingers so any still-attached spectators (or the
/// players' old connections) can see the final state for the grace period.
pub fn schedule_session_finish(state: AppState, session: Arc<SessionRef>, winner: Option<PlayerId>) {
    let session_id = session.id;
    tokio::spawn(async move {
        // Mark finished and finalize replay snapshot.
        let replay_snapshot = {
            let mut finished = session.finished_at.lock().await;
            if finished.is_some() {
                return; // already scheduled
            }
            *finished = Some(Instant::now());
            let mut replay = session.replay.lock().await;
            replay.ended_at = Some(now_secs());
            replay.winner = winner;
            replay.clone()
        };

        if let Err(e) = persist_replay(&replay_snapshot).await {
            tracing::warn!("failed to persist replay {}: {e}", session_id);
        }

        // Free user_index immediately so a new Hello with the same username
        // queues fresh instead of reconnecting into the dead session.
        {
            let mut lobby = state.lobby.lock().await;
            for username in session.players.values() {
                if let Some(UserPresence::InSession {
                    session_id: sid, ..
                }) = lobby.user_index.get(username)
                {
                    if *sid == session_id {
                        lobby.user_index.remove(username);
                    }
                }
            }
        }

        // 60s grace period for spectators, then drop the session.
        tokio::time::sleep(Duration::from_secs(60)).await;
        let mut lobby = state.lobby.lock().await;
        lobby.sessions.remove(&session_id);
    });
}

/// Spawn a task that force-ends the active player's turn after
/// `TURN_DURATION_SECS` if neither manual end_turn nor surrender flips the
/// state in the meantime. Captures the (turn, player) snapshot so a
/// voluntary end_turn just makes the spawned task a no-op when it wakes —
/// no JoinHandle juggling needed.
pub fn schedule_turn_timer(
    state: AppState,
    session: Arc<SessionRef>,
    expected_turn: u32,
    expected_player: PlayerId,
) {
    schedule_turn_timer_with_duration(
        state,
        session,
        expected_turn,
        expected_player,
        Duration::from_secs(TURN_DURATION_SECS),
    );
}

/// Internal variant the tests use to drive the timer with sub-second delays
/// instead of the 10-second production budget.
pub fn schedule_turn_timer_with_duration(
    state: AppState,
    session: Arc<SessionRef>,
    expected_turn: u32,
    expected_player: PlayerId,
    duration: Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(duration).await;

        let mut g = session.game.lock().await;
        let unchanged = g.turn_number == expected_turn
            && g.current_turn == expected_player
            && g.winner.is_none();
        if !unchanged {
            return;
        }
        if g.end_turn(expected_player).is_err() {
            return;
        }
        let next_turn_number = g.turn_number;
        let next_player = g.current_turn;
        drop(g);

        *session.turn_deadline_secs.lock().await = now_secs() + TURN_DURATION_SECS;
        session.replay.lock().await.events.push(ReplayEvent::EndTurn {
            actor: expected_player,
        });
        let _ = session.tx.send(());

        // Recurse for the next turn at the same (test-driven) cadence so a
        // chain of forced ends remains observable.
        schedule_turn_timer_with_duration(state, session, next_turn_number, next_player, duration);
    });
}

async fn persist_replay(replay: &Replay) -> std::io::Result<()> {
    let dir = std::path::Path::new("replays");
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join(format!("{}.json", replay.session_id));
    let json = serde_json::to_string_pretty(replay).expect("replay serialization");
    tokio::fs::write(path, json).await
}

/// Shared-state shell shared between the WS handler and HTTP routes.
#[derive(Clone)]
pub struct AppState {
    pub lobby: Arc<Mutex<Lobby>>,
    /// Broadcast channel that pings every connection in the lobby (queued or
    /// spectating-the-list) when sessions are created or removed. Used by
    /// the lobby browser to keep its session list fresh.
    pub lobby_tx: broadcast::Sender<()>,
}

impl AppState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            lobby: Arc::new(Mutex::new(Lobby::new())),
            lobby_tx: tx,
        }
    }
}

/// Helper for tests / writer task — drains an mpsc into a websocket sink.
pub fn _writer_marker(_: &mpsc::Sender<()>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::GameState;

    async fn make_session() -> (AppState, Arc<SessionRef>) {
        let state = AppState::new();
        let game = GameState::new();
        let map_seed = game.map_seed;
        let (tx, _) = broadcast::channel(64);
        let mut players = HashMap::new();
        players.insert(PlayerId::P1, "alice".to_string());
        players.insert(PlayerId::P2, "bob".to_string());
        let session = Arc::new(SessionRef {
            id: Uuid::new_v4(),
            game: Mutex::new(game),
            tx,
            players: players.clone(),
            created_at: Instant::now(),
            finished_at: Mutex::new(None),
            replay: Mutex::new(Replay {
                session_id: Uuid::nil(),
                map_seed,
                map_generator_version: MAP_GENERATOR_VERSION,
                game_version: GAME_VERSION.to_string(),
                started_at: now_secs(),
                ended_at: None,
                players,
                winner: None,
                events: vec![],
            }),
            turn_deadline_secs: Mutex::new(now_secs() + TURN_DURATION_SECS),
        });
        (state, session)
    }

    #[tokio::test]
    async fn timer_force_ends_idle_player() {
        let (state, session) = make_session().await;
        schedule_turn_timer_with_duration(
            state,
            session.clone(),
            1,
            PlayerId::P1,
            Duration::from_millis(30),
        );
        // Wait comfortably past the timer firing.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let g = session.game.lock().await;
        assert_eq!(
            g.current_turn,
            PlayerId::P2,
            "P1's idle turn should have force-ended"
        );
    }

    #[tokio::test]
    async fn manual_end_turn_makes_timer_noop() {
        let (state, session) = make_session().await;
        schedule_turn_timer_with_duration(
            state,
            session.clone(),
            1,
            PlayerId::P1,
            Duration::from_millis(80),
        );
        // Voluntarily end P1's turn before the timer fires.
        {
            let mut g = session.game.lock().await;
            g.end_turn(PlayerId::P1).unwrap();
        }
        // After the timer would have fired, it should have been a no-op
        // (snapshot mismatch). The turn pointer must still be P2 (not back to P1).
        tokio::time::sleep(Duration::from_millis(160)).await;
        let g = session.game.lock().await;
        assert_eq!(
            g.current_turn,
            PlayerId::P2,
            "voluntary end_turn must make the spawned timer a no-op"
        );
        assert_eq!(g.turn_number, 1, "no double-flip should have occurred");
    }
}
