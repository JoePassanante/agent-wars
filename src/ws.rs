//! WebSocket connection lifecycle for the lobby/queue/session model.
//!
//! 1. Client connects to /ws and sends `Hello { username, intent }`.
//! 2. Server validates, registers username in the lobby, and either:
//!    - Matchmakes the user into a fresh session (Play intent), or
//!    - Attaches them to an existing session as a spectator (Watch intent).
//! 3. Server pushes the initial `State`, then on every game tick sends a
//!    fresh fog-filtered `State` to that connection.
//! 4. Read loop processes Move/BuyUnit/EndTurn/Surrender/Leave commands
//!    scoped to the attached session, and appends each successful action
//!    to the session's replay log.
//! 5. When the engine declares a winner, schedules the session for replay
//!    persistence + GC after a 60s grace.

use std::sync::Arc;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::game::{GAME_VERSION, PlayerId, TURN_DURATION_SECS, View};
use crate::lobby::{
    AppState, QueueEntry, ReplayEvent, SessionId, SessionRef, UserPresence, now_secs,
    schedule_session_finish, schedule_turn_timer,
};
use crate::proto::{ClientIntent, ClientMsg, ServerMsg, TurnAction};

const MIN_USERNAME_LEN: usize = 1;
const MAX_USERNAME_LEN: usize = 32;

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

struct Attachment {
    session: Arc<SessionRef>,
    view: View,
    /// Username for this connection. None for anonymous spectators.
    username: Option<String>,
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (ws_sink, mut receiver) = socket.split();
    let (out_tx, out_rx) = mpsc::channel::<ServerMsg>(64);
    let writer = spawn_writer(ws_sink, out_rx);

    // 1. Read first message; must be Hello.
    let (username, intent) = match read_hello(&mut receiver, &out_tx).await {
        Some(h) => h,
        None => {
            drop(out_tx);
            let _ = writer.await;
            return;
        }
    };

    if !valid_username(&username) {
        let _ = out_tx
            .send(ServerMsg::Error {
                message: format!(
                    "username must be {MIN_USERNAME_LEN}..={MAX_USERNAME_LEN} chars, alphanumeric/_-"
                ),
            })
            .await;
        drop(out_tx);
        let _ = writer.await;
        return;
    }

    let _ = out_tx
        .send(ServerMsg::Hello {
            username: username.clone(),
            server_version: GAME_VERSION.to_string(),
        })
        .await;

    // 2. Process intent → attach.
    let attachment = match intent {
        ClientIntent::Play => attach_as_player(&state, username.clone(), &out_tx).await,
        ClientIntent::Watch { session_id } => {
            attach_as_spectator(&state, session_id, &out_tx).await
        }
    };
    let attachment = match attachment {
        Ok(a) => a,
        Err(e) => {
            let _ = out_tx.send(ServerMsg::Error { message: e }).await;
            drop(out_tx);
            let _ = writer.await;
            return;
        }
    };

    let session = attachment.session.clone();
    let view = attachment.view;
    let attached_username = attachment.username.clone();

    // 3. Push initial state.
    {
        let mut g = session.game.lock().await;
        let mut pview = g.view_for(view);
        pview.session_id = session.id;
        pview.turn_deadline_secs = *session.turn_deadline_secs.lock().await;
        if out_tx.send(ServerMsg::State(pview)).await.is_err() {
            drop(out_tx);
            let _ = writer.await;
            return;
        }
    }

    // 4. Spawn push task: re-send filtered state on every broadcast tick.
    let push = {
        let s = session.clone();
        let tx = out_tx.clone();
        let mut rx = session.tx.subscribe();
        tokio::spawn(async move {
            while rx.recv().await.is_ok() {
                let mut g = s.game.lock().await;
                let mut pview = g.view_for(view);
                pview.session_id = s.id;
                let deadline = *s.turn_deadline_secs.lock().await;
                pview.turn_deadline_secs = deadline;
                if tx.send(ServerMsg::State(pview)).await.is_err() {
                    break;
                }
            }
        })
    };

    // 5. Read loop.
    while let Some(msg) = receiver.next().await {
        let Ok(msg) = msg else { break };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let cmd: ClientMsg = match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                let _ = out_tx
                    .send(ServerMsg::Error {
                        message: format!("bad message: {e}"),
                    })
                    .await;
                continue;
            }
        };
        match handle_command(&state, &session, view, cmd).await {
            Ok(should_disconnect) => {
                if should_disconnect {
                    break;
                }
            }
            Err(message) => {
                let _ = out_tx.send(ServerMsg::Error { message }).await;
            }
        }
    }

    push.abort();
    drop(out_tx);
    let _ = writer.await;

    // Note: we deliberately do NOT remove the user's user_index entry on
    // disconnect — they should be able to reconnect to the same session
    // by Hello'ing again with the same username. GC happens via the
    // 60s grace timer when the engine declares a winner.
    let _ = attached_username; // suppress unused-warning if no per-connection cleanup
}

fn spawn_writer(
    mut ws_sink: SplitSink<WebSocket, Message>,
    mut out_rx: mpsc::Receiver<ServerMsg>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if ws_sink.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
        let _ = ws_sink.send(Message::Close(None)).await;
    })
}

async fn read_hello(
    receiver: &mut futures_util::stream::SplitStream<WebSocket>,
    out_tx: &mpsc::Sender<ServerMsg>,
) -> Option<(String, ClientIntent)> {
    while let Some(msg) = receiver.next().await {
        let Ok(msg) = msg else { return None };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => return None,
            _ => continue,
        };
        match serde_json::from_str::<ClientMsg>(&text) {
            Ok(ClientMsg::Hello { username, intent }) => return Some((username, intent)),
            Ok(_) => {
                let _ = out_tx
                    .send(ServerMsg::Error {
                        message: "first message must be Hello".into(),
                    })
                    .await;
            }
            Err(e) => {
                let _ = out_tx
                    .send(ServerMsg::Error {
                        message: format!("bad message: {e}"),
                    })
                    .await;
            }
        }
    }
    None
}

fn valid_username(name: &str) -> bool {
    let len = name.chars().count();
    if len < MIN_USERNAME_LEN || len > MAX_USERNAME_LEN {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

async fn attach_as_player(
    state: &AppState,
    username: String,
    out_tx: &mpsc::Sender<ServerMsg>,
) -> Result<Attachment, String> {
    // Either reconnect to an existing session or queue. The match-notifier
    // oneshot is registered while we hold the lobby lock; matching happens
    // synchronously inside try_match_pair.
    let oneshot_rx = {
        let mut lobby = state.lobby.lock().await;

        // Reconnect path.
        if let Some(presence) = lobby.user_index.get(&username).cloned() {
            match presence {
                UserPresence::InSession { session_id, role } => {
                    if let Some(session) = lobby.sessions.get(&session_id).cloned() {
                        drop(lobby);
                        let _ = out_tx
                            .send(ServerMsg::Reconnected { session_id, role })
                            .await;
                        return Ok(Attachment {
                            session,
                            view: View::Player(role),
                            username: Some(username),
                        });
                    }
                    // Stale entry — clear and fall through to queue.
                    lobby.user_index.remove(&username);
                }
                UserPresence::Queued => {
                    return Err("a connection is already queued under that username".into());
                }
            }
        }

        // Queue path.
        let (tx, rx) = oneshot::channel::<(SessionId, PlayerId)>();
        lobby.queue.push_back(QueueEntry {
            username: username.clone(),
            queued_at: std::time::Instant::now(),
        });
        lobby.user_index.insert(username.clone(), UserPresence::Queued);
        lobby.match_notifiers.insert(username.clone(), tx);
        let pos = lobby.queue.len() as u32;
        let _ = out_tx.send(ServerMsg::Queued { position: pos }).await;

        // Try to immediately match; harmless if nobody else is waiting.
        if let Some(new_session) = lobby.try_match_pair() {
            // Kick the turn-1 timer for the freshly-created session. The
            // recursive scheduling inside the timer task handles every
            // subsequent turn.
            schedule_turn_timer(state.clone(), new_session, 1, PlayerId::P1);
            let _ = state.lobby_tx.send(()); // notify any lobby browsers
        }
        rx
    };

    // Await match (or cancellation).
    let (sid, role) = oneshot_rx.await.map_err(|_| {
        // Receiver dropped — most likely server is shutting down. Clean up
        // best-effort.
        format!("matchmaking canceled for {username}")
    })?;

    let session = state
        .lobby
        .lock()
        .await
        .sessions
        .get(&sid)
        .cloned()
        .ok_or_else(|| format!("session {sid} disappeared before attach"))?;
    let _ = out_tx
        .send(ServerMsg::Matched {
            session_id: sid,
            role,
        })
        .await;
    Ok(Attachment {
        session,
        view: View::Player(role),
        username: Some(username),
    })
}

async fn attach_as_spectator(
    state: &AppState,
    session_id: SessionId,
    out_tx: &mpsc::Sender<ServerMsg>,
) -> Result<Attachment, String> {
    let session = state
        .lobby
        .lock()
        .await
        .sessions
        .get(&session_id)
        .cloned()
        .ok_or_else(|| format!("no active session {session_id}"))?;
    let _ = out_tx.send(ServerMsg::Spectating { session_id }).await;
    Ok(Attachment {
        session,
        view: View::Spectator,
        username: None,
    })
}

/// Handle a per-session command. Returns Ok(true) if the connection should
/// disconnect (only for Leave), Ok(false) to keep going, Err for surfaced
/// per-command errors.
async fn handle_command(
    state: &AppState,
    session: &Arc<SessionRef>,
    view: View,
    cmd: ClientMsg,
) -> Result<bool, String> {
    match cmd {
        ClientMsg::Hello { .. } => Err("already hello'd".into()),
        ClientMsg::Move {
            unit_id,
            to,
            attack,
        } => {
            let actor = require_player(view)?;
            let report = {
                let mut g = session.game.lock().await;
                g.try_action(actor, unit_id, to, attack)?
            };
            session.replay.lock().await.events.push(ReplayEvent::Move {
                actor,
                unit_id,
                to,
                attack,
                report,
            });
            let _ = session.tx.send(());
            check_finish(state, session).await;
            Ok(false)
        }
        ClientMsg::BuyUnit { factory_id, kind } => {
            let actor = require_player(view)?;
            let new_unit_id = {
                let mut g = session.game.lock().await;
                g.try_buy_unit(actor, factory_id, kind)?
            };
            session.replay.lock().await.events.push(ReplayEvent::Buy {
                actor,
                factory_id,
                kind,
                new_unit_id,
            });
            let _ = session.tx.send(());
            Ok(false)
        }
        ClientMsg::EndTurn => {
            let actor = require_player(view)?;
            let next_state = {
                let mut g = session.game.lock().await;
                g.end_turn(actor)?;
                (g.turn_number, g.current_turn, g.winner)
            };
            session
                .replay
                .lock()
                .await
                .events
                .push(ReplayEvent::EndTurn { actor });
            *session.turn_deadline_secs.lock().await = now_secs() + TURN_DURATION_SECS;
            let _ = session.tx.send(());
            // Schedule next turn's timer (no-op if game already over).
            if next_state.2.is_none() {
                schedule_turn_timer(state.clone(), session.clone(), next_state.0, next_state.1);
            }
            Ok(false)
        }
        ClientMsg::Surrender => {
            let actor = require_player(view)?;
            session.game.lock().await.try_surrender(actor)?;
            session
                .replay
                .lock()
                .await
                .events
                .push(ReplayEvent::Surrender { actor });
            let _ = session.tx.send(());
            check_finish(state, session).await;
            Ok(false)
        }
        ClientMsg::PlayTurn { actions } => {
            let actor = require_player(view)?;
            // Apply actions in order, stop on first error. Prior actions
            // remain committed so a partial turn still progresses the
            // game forward.
            let mut error: Option<String> = None;
            let mut turn_ended = false;
            for action in actions {
                let result =
                    apply_turn_action(&state, session, actor, action.clone(), &mut turn_ended)
                        .await;
                if let Err(e) = result {
                    error = Some(e);
                    break;
                }
            }
            // One broadcast for the whole batch.
            let _ = session.tx.send(());
            check_finish(state, session).await;
            if let Some(e) = error { Err(e) } else { Ok(false) }
        }
        ClientMsg::Leave => Ok(true),
    }
}

/// Apply one element of a `PlayTurn` batch. Mirrors the per-message handlers
/// but returns Err on the first failure so the caller can stop. `turn_ended`
/// is set to true once an EndTurn lands so subsequent actions in the same
/// batch are rejected (you can't keep moving after ending the turn).
async fn apply_turn_action(
    state: &AppState,
    session: &Arc<SessionRef>,
    actor: PlayerId,
    action: TurnAction,
    turn_ended: &mut bool,
) -> Result<(), String> {
    if *turn_ended {
        return Err("EndTurn already issued earlier in this batch".into());
    }
    match action {
        TurnAction::Move {
            unit_id,
            to,
            attack,
        } => {
            let report = {
                let mut g = session.game.lock().await;
                g.try_action(actor, unit_id, to, attack)?
            };
            session.replay.lock().await.events.push(ReplayEvent::Move {
                actor,
                unit_id,
                to,
                attack,
                report,
            });
            Ok(())
        }
        TurnAction::BuyUnit { factory_id, kind } => {
            let new_unit_id = {
                let mut g = session.game.lock().await;
                g.try_buy_unit(actor, factory_id, kind)?
            };
            session.replay.lock().await.events.push(ReplayEvent::Buy {
                actor,
                factory_id,
                kind,
                new_unit_id,
            });
            Ok(())
        }
        TurnAction::EndTurn => {
            let next_state = {
                let mut g = session.game.lock().await;
                g.end_turn(actor)?;
                (g.turn_number, g.current_turn, g.winner)
            };
            session
                .replay
                .lock()
                .await
                .events
                .push(ReplayEvent::EndTurn { actor });
            *session.turn_deadline_secs.lock().await = now_secs() + TURN_DURATION_SECS;
            if next_state.2.is_none() {
                schedule_turn_timer(state.clone(), session.clone(), next_state.0, next_state.1);
            }
            *turn_ended = true;
            Ok(())
        }
    }
}

fn require_player(view: View) -> Result<PlayerId, String> {
    match view {
        View::Player(p) => Ok(p),
        View::Spectator => Err("spectators cannot act".into()),
    }
}

async fn check_finish(state: &AppState, session: &Arc<SessionRef>) {
    let winner = session.game.lock().await.winner;
    if winner.is_some() {
        schedule_session_finish(state.clone(), session.clone(), winner);
        let _ = state.lobby_tx.send(()); // notify lobby browsers
    }
}

// Backward-compat shim: old code imported AppState from this module.
// Re-export the lobby AppState here so external paths (main.rs) don't need
// to change.
#[allow(unused)]
pub fn _appstate_marker(_: AppState) {}
