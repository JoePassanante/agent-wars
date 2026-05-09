use std::sync::Arc;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::game::{GameState, View};
use crate::proto::{ClientMsg, ServerMsg};

#[derive(Clone)]
pub struct AppState {
    pub game: Arc<Mutex<GameState>>,
    /// Pings every connection when the game state changes.
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
    let (mut ws_sink, mut receiver) = socket.split();

    // Single writer task drains an mpsc into the socket; everything else
    // produces ServerMsgs into this channel.
    let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg>(32);
    let writer = tokio::spawn(async move {
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
    });

    // Wait for the Join message before doing anything else.
    let join_view = loop {
        match receiver.next().await {
            Some(Ok(Message::Text(t))) => match serde_json::from_str::<ClientMsg>(&t) {
                Ok(ClientMsg::Join { view }) => break view,
                Ok(_) => {
                    let _ = out_tx
                        .send(ServerMsg::Error {
                            message: "join required first".into(),
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
            },
            Some(Ok(Message::Close(_))) | None => {
                drop(out_tx);
                let _ = writer.await;
                return;
            }
            Some(Ok(_)) => {}
            Some(Err(_)) => {
                drop(out_tx);
                let _ = writer.await;
                return;
            }
        }
    };

    if out_tx
        .send(ServerMsg::Joined { view: join_view })
        .await
        .is_err()
    {
        return;
    }

    // Initial state push.
    {
        let mut g = state.game.lock().await;
        let view = g.view_for(join_view);
        if out_tx.send(ServerMsg::State(view)).await.is_err() {
            return;
        }
    }

    // Push task: re-send filtered state on every broadcast tick.
    let mut rx = state.tx.subscribe();
    let push_game = state.game.clone();
    let push_tx = out_tx.clone();
    let push = tokio::spawn(async move {
        while rx.recv().await.is_ok() {
            let mut g = push_game.lock().await;
            let view = g.view_for(join_view);
            if push_tx.send(ServerMsg::State(view)).await.is_err() {
                break;
            }
        }
    });

    // Read loop.
    while let Some(msg) = receiver.next().await {
        let Ok(msg) = msg else { break };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        match serde_json::from_str::<ClientMsg>(&text) {
            Ok(cmd) => {
                if let Err(message) = handle_command(&state, join_view, cmd).await {
                    let _ = out_tx.send(ServerMsg::Error { message }).await;
                }
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

    push.abort();
    drop(out_tx);
    let _ = writer.await;
}

async fn handle_command(state: &AppState, view: View, cmd: ClientMsg) -> Result<(), String> {
    // Reset is allowed for anyone, including spectators.
    if matches!(cmd, ClientMsg::Reset) {
        let mut g = state.game.lock().await;
        *g = GameState::new();
        drop(g);
        let _ = state.tx.send(());
        return Ok(());
    }

    let actor = match view {
        View::Spectator => return Err("spectators cannot act".into()),
        View::Player(p) => p,
    };
    let mut g = state.game.lock().await;
    match cmd {
        ClientMsg::Join { .. } => Err("already joined".into()),
        ClientMsg::Move {
            unit_id,
            to,
            attack,
        } => {
            g.try_action(actor, unit_id, to, attack)?;
            drop(g);
            let _ = state.tx.send(());
            Ok(())
        }
        ClientMsg::BuyUnit { factory_id, kind } => {
            g.try_buy_unit(actor, factory_id, kind)?;
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
        ClientMsg::Surrender => {
            g.try_surrender(actor)?;
            drop(g);
            let _ = state.tx.send(());
            Ok(())
        }
        ClientMsg::Reset => unreachable!("handled above"),
    }
}
