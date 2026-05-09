use std::sync::Arc;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Mutex, broadcast};

use crate::game::{GameState, PlayerId, View};
use crate::proto::{ClientMsg, ServerMsg};

#[derive(Clone)]
pub struct AppState {
    pub game: Arc<Mutex<GameState>>,
    /// Broadcast channel that pings every client when state changes.
    pub tx: broadcast::Sender<()>,
}

impl AppState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            game: Arc::new(Mutex::new(GameState::new())),
            tx,
        }
    }
}

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.tx.subscribe();

    // Wait for the Join message before doing anything else.
    let join_view = loop {
        match receiver.next().await {
            Some(Ok(Message::Text(t))) => match serde_json::from_str::<ClientMsg>(&t) {
                Ok(ClientMsg::Join { view }) => break view,
                Ok(_) => {
                    let _ = send(&mut sender, &ServerMsg::Error {
                        message: "join required first".into(),
                    })
                    .await;
                }
                Err(e) => {
                    let _ = send(&mut sender, &ServerMsg::Error {
                        message: format!("bad message: {e}"),
                    })
                    .await;
                }
            },
            Some(Ok(Message::Close(_))) | None => return,
            Some(Ok(_)) => {} // ignore binary/ping/pong
            Some(Err(_)) => return,
        }
    };

    if send(&mut sender, &ServerMsg::Joined { view: join_view })
        .await
        .is_err()
    {
        return;
    }

    // Push the initial state.
    {
        let g = state.game.lock().await;
        let view = g.view_for(join_view);
        if send(&mut sender, &ServerMsg::State(view)).await.is_err() {
            return;
        }
    }

    // Spawn a task that pushes filtered state to this client whenever the game changes.
    let game = state.game.clone();
    let push_view = join_view;
    let mut sender = sender;
    let push = tokio::spawn(async move {
        while rx.recv().await.is_ok() {
            let g = game.lock().await;
            let view = g.view_for(push_view);
            if send(&mut sender, &ServerMsg::State(view)).await.is_err() {
                break;
            }
        }
        // Try to close cleanly.
        let _ = sender.send(Message::Close(None)).await;
    });

    // Read loop: handle commands.
    while let Some(msg) = receiver.next().await {
        let Ok(msg) = msg else { break };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let cmd = match serde_json::from_str::<ClientMsg>(&text) {
            Ok(c) => c,
            Err(e) => {
                let _ = state.tx.send(()); // no-op nudge
                tracing::debug!(error = %e, "bad client message");
                continue;
            }
        };
        let result = handle_command(&state, join_view, cmd).await;
        if let Err(message) = result {
            tracing::debug!(error = %message, "command rejected");
            // We can't push an error to *this* client through the broadcast channel,
            // so we don't bother sending Error frames here for the MVP. The next State
            // push reflects the unchanged game.
            let _ = message;
        }
    }

    push.abort();
}

async fn handle_command(
    state: &AppState,
    view: View,
    cmd: ClientMsg,
) -> Result<(), String> {
    let actor = match view {
        View::Spectator => return Err("spectators cannot act".into()),
        View::Player(p) => p,
    };
    let mut g = state.game.lock().await;
    match cmd {
        ClientMsg::Join { .. } => Err("already joined".into()),
        ClientMsg::Move { unit_id, to } => {
            g.try_move(actor, unit_id, to)?;
            drop(g);
            let _ = state.tx.send(());
            Ok(())
        }
        ClientMsg::EndTurn => {
            g.end_turn(actor)?;
            drop(g);
            let _ = state.tx.send(());
            Ok(())
        }
    }
}

async fn send<S>(sender: &mut S, msg: &ServerMsg) -> Result<(), ()>
where
    S: SinkExt<Message> + Unpin,
{
    let json = serde_json::to_string(msg).map_err(|_| ())?;
    sender.send(Message::Text(json)).await.map_err(|_| ())
}

// Helper to silence dead-code warnings if PlayerId is only used through serde paths.
#[allow(dead_code)]
fn _player_id_keepalive(p: PlayerId) -> PlayerId {
    p
}
