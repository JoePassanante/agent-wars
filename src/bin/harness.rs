//! agent-wars-harness — a programmable QuickJS sandbox sitting next to the
//! game so an agent can compile a strategy script once and let it drive
//! turn decisions without paying a full LLM round-trip every action.
//!
//! Architecture:
//!   * Connects to the game WebSocket as a player (same Hello → Queue →
//!     Matched dance the MCP binary does).
//!   * Maintains a cached PlayerView from broadcast pushes so JS reads are
//!     cheap and synchronous.
//!   * Runs a dedicated **QuickJS thread** that holds the user's script and
//!     runs `onTurn(ctx)` on demand. The thread is sync; bindings block on
//!     mpsc/oneshot to the async tokio runtime when they need to talk to
//!     the game or to OpenRouter.
//!   * Exposes a tiny MCP surface over stdio so the meta-agent can:
//!       - set_script / get_script
//!       - on_turn (run the user's onTurn handler)
//!       - eval (one-shot JS for testing)
//!       - get_state (pass-through cache)
//!
//! Globals available inside the script:
//!   * `game` — { getState, act, playTurn, endTurn, buyUnit, surrender }
//!   * `subAgent.ask(prompt, opts?)` — sync POST to OpenRouter; defaults
//!     to anthropic/claude-haiku, overridable via `opts.model`. Reads
//!     OPENROUTER_API_KEY from env at startup.
//!   * `log.info / warn / error` — captured per-turn, returned with
//!     on_turn so the agent can debug script behaviour.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use agent_wars::game::{PlayerId, PlayerView, UnitKind};
use agent_wars::proto::{ClientIntent, ClientMsg, ServerMsg, TurnAction};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(name = "agent-wars-harness", about = "QuickJS scripting sandbox for agent-wars")]
struct Args {
    /// Username this harness uses to queue / reconnect. Required.
    #[arg(long, alias = "player")]
    username: String,
    /// agent-wars WebSocket URL.
    #[arg(long, default_value = "ws://127.0.0.1:8080/ws")]
    url: String,
    /// Where to read OPENROUTER_API_KEY from (env var name). Defaults to
    /// `OPENROUTER_API_KEY`.
    #[arg(long, default_value = "OPENROUTER_API_KEY")]
    openrouter_key_env: String,
}

/// Game-side commands the JS thread sends to the async runtime when a
/// binding fires. Each command carries a oneshot that the runtime fills in
/// once it has a response.
enum GameCmd {
    Send {
        msg: ClientMsg,
        ack: oneshot::Sender<Result<ServerMsg, String>>,
    },
    /// Read the cached PlayerView synchronously (no WS round-trip needed).
    Snapshot {
        ack: oneshot::Sender<Option<PlayerView>>,
    },
}

/// Sub-agent (LLM) commands.
enum AiCmd {
    Ask {
        prompt: String,
        model: String,
        ack: oneshot::Sender<Result<String, String>>,
    },
}

struct HarnessState {
    username: String,
    /// Set once the server matches us into a session.
    player: OnceLock<PlayerId>,
    session_id: OnceLock<Uuid>,
    /// Cached most-recent PlayerView from the WS broadcast.
    state: Mutex<Option<PlayerView>>,
    /// Sender for the JS runtime's game commands.
    game_tx: mpsc::Sender<GameCmd>,
    /// Sender for the JS runtime's sub-agent commands.
    ai_tx: mpsc::Sender<AiCmd>,
    /// Sender for the QuickJS thread's command queue.
    qjs_tx: std::sync::mpsc::Sender<JsCmd>,
    /// Where script files live.
    scripts_dir: PathBuf,
}

impl HarnessState {
    fn script_path(&self) -> PathBuf {
        self.scripts_dir.join(format!("{}.js", self.username))
    }
}

/// Commands the QuickJS thread accepts. The thread is single-purpose: hold
/// the JS context, accept set/eval/onTurn, return results.
enum JsCmd {
    SetScript {
        code: String,
        ack: std::sync::mpsc::Sender<Result<(), String>>,
    },
    Eval {
        code: String,
        ack: std::sync::mpsc::Sender<Result<String, String>>,
    },
    OnTurn {
        context: serde_json::Value,
        ack: std::sync::mpsc::Sender<Result<TurnResult, String>>,
    },
}

#[derive(serde::Serialize)]
struct TurnResult {
    /// Whatever onTurn returned (stringified if not already a string).
    value: String,
    /// Captured log.info / warn / error lines.
    logs: Vec<LogLine>,
}

#[derive(serde::Serialize, Clone)]
struct LogLine {
    level: String,
    message: String,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    eprintln!(
        "agent-wars-harness starting as username={}, connecting to {}",
        args.username, args.url
    );

    // Read OpenRouter key once at startup; missing key just disables subAgent.
    let openrouter_key = std::env::var(&args.openrouter_key_env).ok();
    if openrouter_key.is_none() {
        eprintln!(
            "(no {} set — subAgent.ask will return an error if scripts call it)",
            args.openrouter_key_env
        );
    }

    let (game_tx, mut game_rx) = mpsc::channel::<GameCmd>(64);
    let (ai_tx, mut ai_rx) = mpsc::channel::<AiCmd>(32);
    let (qjs_tx, qjs_rx) = std::sync::mpsc::channel::<JsCmd>();

    // ---- WebSocket setup (mirrors mcp.rs) -------------------------------
    let (ws_stream, _) = connect_async(&args.url).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();
    let (out_cmd_tx, mut out_cmd_rx) = mpsc::channel::<ClientMsg>(32);
    let (events_tx, _) = broadcast::channel::<ServerMsg>(64);

    let scripts_dir = PathBuf::from("scripts");
    let _ = std::fs::create_dir_all(&scripts_dir);

    let state = Arc::new(HarnessState {
        username: args.username.clone(),
        player: OnceLock::new(),
        session_id: OnceLock::new(),
        state: Mutex::new(None),
        game_tx: game_tx.clone(),
        ai_tx: ai_tx.clone(),
        qjs_tx: qjs_tx.clone(),
        scripts_dir,
    });

    // Reader: drains WS into events broadcast + state cache.
    let reader_state = Arc::clone(&state);
    tokio::spawn(async move {
        while let Some(msg) = ws_rx.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("ws read error: {e}");
                    break;
                }
            };
            let WsMessage::Text(t) = msg else { continue };
            let server_msg: ServerMsg = match serde_json::from_str(&t) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("bad server msg: {e}");
                    continue;
                }
            };
            if let ServerMsg::State(view) = &server_msg {
                *reader_state.state.lock().await = Some(view.clone());
            }
            // events broadcast — used by GameCmd::Send awaits.
            let _ = events_tx_handle().send(server_msg);
        }
        eprintln!("ws reader exiting");
    });

    // Writer: drains out_cmd_rx into the WS sink.
    tokio::spawn(async move {
        while let Some(cmd) = out_cmd_rx.recv().await {
            let s = match serde_json::to_string(&cmd) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if ws_tx.send(WsMessage::Text(s)).await.is_err() {
                break;
            }
        }
    });

    // Stash events_tx in a OnceLock so the reader closure can reach it
    // without taking ownership at spawn time.
    EVENTS_TX.set(events_tx).expect("events_tx initialized once");

    // GameCmd dispatcher: turn JS-thread requests into WS sends + waits.
    let dispatch_state = Arc::clone(&state);
    let dispatch_out_tx = out_cmd_tx.clone();
    tokio::spawn(async move {
        while let Some(cmd) = game_rx.recv().await {
            match cmd {
                GameCmd::Snapshot { ack } => {
                    let snapshot = dispatch_state.state.lock().await.clone();
                    let _ = ack.send(snapshot);
                }
                GameCmd::Send { msg, ack } => {
                    let mut events = events_tx_handle().subscribe();
                    if dispatch_out_tx.send(msg).await.is_err() {
                        let _ = ack.send(Err("writer task dead".into()));
                        continue;
                    }
                    // Wait for the next post-handshake response (Error or State).
                    let resp = wait_response(&mut events).await;
                    let _ = ack.send(resp);
                }
            }
        }
    });

    // Sub-agent dispatcher: POSTs to OpenRouter on demand.
    let openrouter_key_owned = openrouter_key.clone();
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .expect("reqwest client");
        while let Some(cmd) = ai_rx.recv().await {
            match cmd {
                AiCmd::Ask { prompt, model, ack } => {
                    let key = match openrouter_key_owned.as_deref() {
                        Some(k) => k,
                        None => {
                            let _ = ack.send(Err("OPENROUTER_API_KEY not set".into()));
                            continue;
                        }
                    };
                    let resp = call_openrouter(&client, key, &model, &prompt).await;
                    let _ = ack.send(resp);
                }
            }
        }
    });

    // ---- Hello → Queue → Matched ---------------------------------------
    let mut bootstrap = events_tx_handle().subscribe();
    out_cmd_tx
        .send(ClientMsg::Hello {
            username: args.username.clone(),
            intent: ClientIntent::Play,
        })
        .await?;
    handshake(&state, &mut bootstrap).await?;

    eprintln!("agent-wars-harness ready (player={:?})", state.player.get());

    // ---- QuickJS thread ------------------------------------------------
    let qjs_state = Arc::clone(&state);
    let qjs_game_tx = game_tx.clone();
    let qjs_ai_tx = ai_tx.clone();
    thread::Builder::new()
        .name("quickjs".into())
        .spawn(move || {
            quickjs_main(qjs_rx, qjs_state, qjs_game_tx, qjs_ai_tx);
        })?;

    // Load existing script if present so the runtime starts warm.
    if let Ok(code) = std::fs::read_to_string(state.script_path()) {
        eprintln!(
            "loaded existing script for {} ({} bytes)",
            state.username,
            code.len()
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = qjs_tx.send(JsCmd::SetScript { code, ack: tx });
        if let Ok(Err(e)) = rx.recv_timeout(Duration::from_secs(5)) {
            eprintln!("warning: existing script failed to compile: {e}");
        }
    }

    // ---- JSON-RPC stdio loop -------------------------------------------
    json_rpc_loop(state).await?;
    Ok(())
}

// Static so the WS reader can reach the events broadcast without
// per-spawn ownership juggling.
static EVENTS_TX: OnceLock<broadcast::Sender<ServerMsg>> = OnceLock::new();
fn events_tx_handle() -> &'static broadcast::Sender<ServerMsg> {
    EVENTS_TX.get().expect("events_tx initialized")
}

async fn handshake(
    state: &Arc<HarnessState>,
    bootstrap: &mut broadcast::Receiver<ServerMsg>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut got_state = false;
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !got_state {
        let now = tokio::time::Instant::now();
        let remaining = if now >= deadline {
            Duration::from_secs(0)
        } else {
            deadline - now
        };
        match timeout(remaining, bootstrap.recv()).await {
            Ok(Ok(ServerMsg::Hello { server_version, .. })) => {
                eprintln!("connected to server v{server_version}");
            }
            Ok(Ok(ServerMsg::Queued { position })) => {
                eprintln!("queued at position {position}; waiting for opponent…");
                deadline = tokio::time::Instant::now() + Duration::from_secs(3600);
            }
            Ok(Ok(ServerMsg::Matched { session_id, role })) => {
                eprintln!("matched into session {session_id} as {role:?}");
                let _ = state.player.set(role);
                let _ = state.session_id.set(session_id);
                deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            }
            Ok(Ok(ServerMsg::Reconnected { session_id, role })) => {
                eprintln!("reconnected to session {session_id} as {role:?}");
                let _ = state.player.set(role);
                let _ = state.session_id.set(session_id);
            }
            Ok(Ok(ServerMsg::Spectating { session_id })) => {
                return Err(format!(
                    "harness only supports playing, not spectating ({session_id})"
                )
                .into());
            }
            Ok(Ok(ServerMsg::State(_))) => got_state = true,
            Ok(Ok(ServerMsg::Error { message })) => {
                return Err(format!("server error during handshake: {message}").into());
            }
            Ok(Err(_)) => return Err("event channel closed during handshake".into()),
            Err(_) => return Err("timed out waiting for matchmaking".into()),
        }
    }
    Ok(())
}

async fn wait_response(rx: &mut broadcast::Receiver<ServerMsg>) -> Result<ServerMsg, String> {
    loop {
        match timeout(Duration::from_secs(8), rx.recv()).await {
            Ok(Ok(ServerMsg::Hello { .. }))
            | Ok(Ok(ServerMsg::Queued { .. }))
            | Ok(Ok(ServerMsg::Matched { .. }))
            | Ok(Ok(ServerMsg::Reconnected { .. }))
            | Ok(Ok(ServerMsg::Spectating { .. })) => continue,
            Ok(Ok(other)) => return Ok(other),
            Ok(Err(_)) => return Err("event channel closed".into()),
            Err(_) => return Err("timed out waiting for server response".into()),
        }
    }
}

async fn call_openrouter(
    client: &reqwest::Client,
    key: &str,
    model: &str,
    prompt: &str,
) -> Result<String, String> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
    });
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("openrouter request failed: {e}"))?;
    let status = resp.status();
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("openrouter response parse: {e}"))?;
    if !status.is_success() {
        return Err(format!("openrouter http {}: {}", status, json));
    }
    json.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("openrouter response missing content: {json}"))
}

// =================== QuickJS thread ===================

fn quickjs_main(
    rx: std::sync::mpsc::Receiver<JsCmd>,
    state: Arc<HarnessState>,
    game_tx: mpsc::Sender<GameCmd>,
    ai_tx: mpsc::Sender<AiCmd>,
) {
    use rquickjs::{CatchResultExt, Context, Function, Object, Runtime};

    let runtime = match Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("failed to start quickjs runtime: {e}");
            return;
        }
    };
    let context = match Context::full(&runtime) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to build quickjs context: {e}");
            return;
        }
    };

    // The captured logs for the current OnTurn invocation. Reset each turn.
    let log_buffer: Arc<std::sync::Mutex<Vec<LogLine>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // Install globals once.
    let install_result: Result<(), String> = context.with(|ctx| -> Result<(), String> {
        let globals = ctx.globals();

        // ---- log namespace ----
        let log_obj = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
        for (level_name, level_owned) in [
            ("info", "info"),
            ("warn", "warn"),
            ("error", "error"),
        ] {
            let buf = log_buffer.clone();
            let level_str = level_owned.to_string();
            let f = Function::new(ctx.clone(), move |msg: rquickjs::Value| {
                let s = stringify_value(&msg);
                eprintln!("[js {level_str}] {s}");
                buf.lock()
                    .unwrap()
                    .push(LogLine { level: level_str.clone(), message: s });
            })
            .map_err(|e| e.to_string())?;
            log_obj.set(level_name, f).map_err(|e| e.to_string())?;
        }
        globals.set("log", log_obj).map_err(|e| e.to_string())?;

        // ---- game namespace ----
        let game_obj = Object::new(ctx.clone()).map_err(|e| e.to_string())?;

        // game.getState(): returns the cached PlayerView as a JSON STRING.
        // Scripts call JSON.parse(game.getState()) themselves — keeps the
        // binding side ctx-free.
        {
            let game_tx = game_tx.clone();
            let f = Function::new(ctx.clone(), move || -> String {
                let (tx, rx) = oneshot::channel();
                if game_tx.blocking_send(GameCmd::Snapshot { ack: tx }).is_err() {
                    return "{}".into();
                }
                match rx.blocking_recv() {
                    Ok(Some(view)) => serde_json::to_string(&view).unwrap_or_else(|_| "{}".into()),
                    _ => "{}".into(),
                }
            })
            .map_err(|e| e.to_string())?;
            game_obj.set("getState", f).map_err(|e| e.to_string())?;
        }

        // game.act(unitId, [tx,ty], [targetX,targetY] | null)
        {
            let game_tx = game_tx.clone();
            let f = Function::new(ctx.clone(), move |unit_id: String, to: rquickjs::Value, target: rquickjs::Value| -> rquickjs::Result<String> {
                let to_coord = parse_coord(&to).map_err(|e| rquickjs::Error::Exception)?;
                let attack = if target.is_undefined() || target.is_null() {
                    None
                } else {
                    Some(parse_coord(&target).map_err(|e| rquickjs::Error::Exception)?)
                };
                let unit_uuid = unit_id.parse::<Uuid>().map_err(|_| rquickjs::Error::Exception)?;
                let (tx, rx) = oneshot::channel();
                game_tx
                    .blocking_send(GameCmd::Send {
                        msg: ClientMsg::Move { unit_id: unit_uuid, to: to_coord, attack },
                        ack: tx,
                    })
                    .map_err(|_| rquickjs::Error::Exception)?;
                match rx.blocking_recv() {
                    Ok(Ok(ServerMsg::State(_))) => Ok("ok".into()),
                    Ok(Ok(ServerMsg::Error { message })) => Ok(format!("error: {message}")),
                    Ok(Ok(_)) => Ok("unexpected".into()),
                    Ok(Err(e)) => Ok(format!("error: {e}")),
                    Err(_) => Ok("error: ack channel closed".into()),
                }
            })
            .map_err(|e| e.to_string())?;
            game_obj.set("act", f).map_err(|e| e.to_string())?;
        }

        // game.endTurn()
        {
            let game_tx = game_tx.clone();
            let f = Function::new(ctx.clone(), move || -> rquickjs::Result<String> {
                let (tx, rx) = oneshot::channel();
                game_tx
                    .blocking_send(GameCmd::Send { msg: ClientMsg::EndTurn, ack: tx })
                    .map_err(|_| rquickjs::Error::Exception)?;
                match rx.blocking_recv() {
                    Ok(Ok(ServerMsg::State(_))) => Ok("ok".into()),
                    Ok(Ok(ServerMsg::Error { message })) => Ok(format!("error: {message}")),
                    _ => Ok("unexpected".into()),
                }
            })
            .map_err(|e| e.to_string())?;
            game_obj.set("endTurn", f).map_err(|e| e.to_string())?;
        }

        // game.buyUnit(factoryId, kindString)
        {
            let game_tx = game_tx.clone();
            let f = Function::new(ctx.clone(), move |factory_id: String, kind: String| -> rquickjs::Result<String> {
                let factory_uuid = factory_id.parse::<Uuid>().map_err(|_| rquickjs::Error::Exception)?;
                let unit_kind = match kind.as_str() {
                    "scout" => UnitKind::Scout,
                    "infantry" => UnitKind::Infantry,
                    "heavy_infantry" | "heavyInfantry" | "heavy" => UnitKind::HeavyInfantry,
                    _ => return Ok(format!("error: unknown kind {kind}")),
                };
                let (tx, rx) = oneshot::channel();
                game_tx
                    .blocking_send(GameCmd::Send {
                        msg: ClientMsg::BuyUnit { factory_id: factory_uuid, kind: unit_kind },
                        ack: tx,
                    })
                    .map_err(|_| rquickjs::Error::Exception)?;
                match rx.blocking_recv() {
                    Ok(Ok(ServerMsg::State(_))) => Ok("ok".into()),
                    Ok(Ok(ServerMsg::Error { message })) => Ok(format!("error: {message}")),
                    _ => Ok("unexpected".into()),
                }
            })
            .map_err(|e| e.to_string())?;
            game_obj.set("buyUnit", f).map_err(|e| e.to_string())?;
        }

        // game.surrender()
        {
            let game_tx = game_tx.clone();
            let f = Function::new(ctx.clone(), move || -> rquickjs::Result<String> {
                let (tx, rx) = oneshot::channel();
                game_tx
                    .blocking_send(GameCmd::Send { msg: ClientMsg::Surrender, ack: tx })
                    .map_err(|_| rquickjs::Error::Exception)?;
                match rx.blocking_recv() {
                    Ok(Ok(ServerMsg::State(_))) => Ok("ok".into()),
                    Ok(Ok(ServerMsg::Error { message })) => Ok(format!("error: {message}")),
                    _ => Ok("unexpected".into()),
                }
            })
            .map_err(|e| e.to_string())?;
            game_obj.set("surrender", f).map_err(|e| e.to_string())?;
        }

        // game.playTurn(actions[]) — single round-trip batch.
        {
            let game_tx = game_tx.clone();
            let f = Function::new(ctx.clone(), move |actions_val: rquickjs::Value| -> rquickjs::Result<String> {
                let actions = parse_actions(&actions_val)
                    .map_err(|_| rquickjs::Error::Exception)?;
                let (tx, rx) = oneshot::channel();
                game_tx
                    .blocking_send(GameCmd::Send {
                        msg: ClientMsg::PlayTurn { actions },
                        ack: tx,
                    })
                    .map_err(|_| rquickjs::Error::Exception)?;
                match rx.blocking_recv() {
                    Ok(Ok(ServerMsg::State(_))) => Ok("ok".into()),
                    Ok(Ok(ServerMsg::Error { message })) => Ok(format!("error: {message}")),
                    _ => Ok("unexpected".into()),
                }
            })
            .map_err(|e| e.to_string())?;
            game_obj.set("playTurn", f).map_err(|e| e.to_string())?;
        }

        // Identity hint so the script knows which player it is.
        if let Some(p) = state.player.get() {
            let role = match p {
                PlayerId::P1 => "p1",
                PlayerId::P2 => "p2",
            };
            game_obj.set("you", role).map_err(|e| e.to_string())?;
        }

        globals.set("game", game_obj).map_err(|e| e.to_string())?;

        // ---- subAgent namespace ----
        let agent_obj = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
        let ai_tx_inner = ai_tx.clone();
        let f = Function::new(ctx.clone(), move |prompt: String, opts: rquickjs::Value| -> rquickjs::Result<String> {
            let model = opts
                .as_object()
                .and_then(|o| o.get::<_, String>("model").ok())
                .unwrap_or_else(|| "anthropic/claude-haiku-4-5".to_string());
            let (tx, rx) = oneshot::channel();
            ai_tx_inner
                .blocking_send(AiCmd::Ask { prompt, model, ack: tx })
                .map_err(|_| rquickjs::Error::Exception)?;
            match rx.blocking_recv() {
                Ok(Ok(s)) => Ok(s),
                Ok(Err(e)) => Ok(format!("error: {e}")),
                Err(_) => Ok("error: ai channel closed".into()),
            }
        })
        .map_err(|e| e.to_string())?;
        agent_obj.set("ask", f).map_err(|e| e.to_string())?;
        globals.set("subAgent", agent_obj).map_err(|e| e.to_string())?;

        Ok(())
    });
    if let Err(e) = install_result {
        eprintln!("failed to install JS globals: {e}");
        return;
    }

    // Command loop.
    while let Ok(cmd) = rx.recv() {
        match cmd {
            JsCmd::SetScript { code, ack } => {
                let result: Result<(), String> = context.with(|ctx| {
                    ctx.eval::<(), _>(code.as_bytes())
                        .catch(&ctx)
                        .map(|_| ())
                        .map_err(|e| format!("{e:?}"))
                });
                let _ = ack.send(result);
            }
            JsCmd::Eval { code, ack } => {
                let result: Result<String, String> = context.with(|ctx| {
                    let v: rquickjs::Result<rquickjs::Value> = ctx.eval(code.as_bytes());
                    match v.catch(&ctx) {
                        Ok(val) => Ok(stringify_value(&val)),
                        Err(e) => Err(format!("{e:?}")),
                    }
                });
                let _ = ack.send(result);
            }
            JsCmd::OnTurn { context: js_ctx, ack } => {
                log_buffer.lock().unwrap().clear();
                let result: Result<TurnResult, String> = context.with(|ctx| -> Result<TurnResult, String> {
                    let globals = ctx.globals();
                    let on_turn: rquickjs::Value =
                        globals.get("onTurn").map_err(|_| {
                            "no global function `onTurn` defined; set_script must define one"
                                .to_string()
                        })?;
                    let func: rquickjs::Function = on_turn
                        .into_function()
                        .ok_or_else(|| "global `onTurn` is not a function".to_string())?;
                    let arg_json = serde_to_json_string(&js_ctx);
                    let v: rquickjs::Result<rquickjs::Value> = func.call((arg_json,));
                    let value_str = match v.catch(&ctx) {
                        Ok(val) => stringify_value(&val),
                        Err(e) => return Err(format!("{e:?}")),
                    };
                    Ok(TurnResult {
                        value: value_str,
                        logs: log_buffer.lock().unwrap().clone(),
                    })
                });
                let _ = ack.send(result);
            }
        }
    }
}

fn stringify_value(v: &rquickjs::Value) -> String {
    if let Some(s) = v.as_string() {
        return s.to_string().unwrap_or_default();
    }
    if let Some(n) = v.as_number() {
        return n.to_string();
    }
    if v.is_bool() {
        return v.as_bool().unwrap_or(false).to_string();
    }
    if v.is_null() {
        return "null".into();
    }
    if v.is_undefined() {
        return "undefined".into();
    }
    // Fall back to JSON stringify via the runtime.
    if let Some(obj) = v.as_object() {
        if let Ok(json_str) = obj.ctx().json_stringify(v.clone()) {
            if let Some(s) = json_str.and_then(|s| s.to_string().ok()) {
                return s;
            }
        }
    }
    "<unprintable>".into()
}

fn parse_coord(v: &rquickjs::Value) -> Result<(i32, i32), String> {
    let arr = v.as_array().ok_or("expected [x, y] array")?;
    if arr.len() != 2 {
        return Err("coord must have exactly 2 elements".into());
    }
    let x: i32 = arr.get::<i32>(0).map_err(|e| e.to_string())?;
    let y: i32 = arr.get::<i32>(1).map_err(|e| e.to_string())?;
    Ok((x, y))
}

fn parse_actions(v: &rquickjs::Value) -> Result<Vec<TurnAction>, String> {
    let arr = v.as_array().ok_or("actions must be an array")?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        let item: rquickjs::Value = arr.get(i).map_err(|e| e.to_string())?;
        let obj = item.as_object().ok_or("action must be an object")?;
        let kind: String = obj.get("type").map_err(|e| e.to_string())?;
        match kind.as_str() {
            "move" => {
                let unit_id_str: String = obj.get("unitId").map_err(|e| e.to_string())?;
                let unit_id: Uuid = unit_id_str.parse().map_err(|e: uuid::Error| e.to_string())?;
                let to_v: rquickjs::Value = obj.get("to").map_err(|e| e.to_string())?;
                let to = parse_coord(&to_v)?;
                let attack = if let Ok(t) = obj.get::<_, rquickjs::Value>("target") {
                    if t.is_undefined() || t.is_null() {
                        None
                    } else {
                        Some(parse_coord(&t)?)
                    }
                } else {
                    None
                };
                out.push(TurnAction::Move { unit_id, to, attack });
            }
            "buyUnit" => {
                let factory_id_str: String = obj.get("factoryId").map_err(|e| e.to_string())?;
                let factory_id: Uuid =
                    factory_id_str.parse().map_err(|e: uuid::Error| e.to_string())?;
                let kind_str: String = obj.get("kind").map_err(|e| e.to_string())?;
                let unit_kind = match kind_str.as_str() {
                    "scout" => UnitKind::Scout,
                    "infantry" => UnitKind::Infantry,
                    "heavy_infantry" | "heavy" => UnitKind::HeavyInfantry,
                    other => return Err(format!("unknown unit kind {other}")),
                };
                out.push(TurnAction::BuyUnit { factory_id, kind: unit_kind });
            }
            "endTurn" => out.push(TurnAction::EndTurn),
            other => return Err(format!("unknown action type {other}")),
        }
    }
    Ok(out)
}

/// Serialize any serde value to a JSON string. Scripts pass it through
/// `JSON.parse(...)` on the JS side; that keeps the bindings ctx-free.
fn serde_to_json_string<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

// =================== JSON-RPC over stdio ===================

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

async fn json_rpc_loop(state: Arc<HarnessState>) -> std::io::Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let stdout = tokio::io::stdout();
    let mut stdout = stdout;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                send_err(&mut stdout, Value::Null, -32700, format!("parse error: {e}")).await?;
                continue;
            }
        };
        let is_notification = req.id.is_none();
        let id = req.id.clone().unwrap_or(Value::Null);

        match req.method.as_str() {
            "initialize" => {
                let result = json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "agent-wars-harness", "version": env!("CARGO_PKG_VERSION") }
                });
                send_result(&mut stdout, id, result).await?;
            }
            "notifications/initialized" => {}
            "tools/list" => {
                send_result(&mut stdout, id, json!({ "tools": list_tools() })).await?;
            }
            "tools/call" => {
                let name = req
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = req
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                match dispatch_tool(&state, &name, &args).await {
                    Ok(text) => {
                        send_result(
                            &mut stdout,
                            id,
                            json!({
                                "content": [{ "type": "text", "text": text }],
                                "isError": false,
                            }),
                        )
                        .await?
                    }
                    Err(err) => {
                        send_result(
                            &mut stdout,
                            id,
                            json!({
                                "content": [{ "type": "text", "text": format!("Error: {err}") }],
                                "isError": true,
                            }),
                        )
                        .await?
                    }
                }
            }
            "ping" => send_result(&mut stdout, id, json!({})).await?,
            "shutdown" => {
                send_result(&mut stdout, id, Value::Null).await?;
                break;
            }
            other => {
                if is_notification {
                    continue;
                }
                send_err(&mut stdout, id, -32601, format!("method not found: {other}")).await?;
            }
        }
    }
    Ok(())
}

fn list_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "set_script",
            "description": "Replace the current QuickJS script. Code is saved to scripts/<username>.js and compiled into the persistent runtime. Define a global `function onTurn(ctx) { ... }` (or async equivalent) — the harness calls that function from `on_turn`. Globals available: game.{getState, act, playTurn, endTurn, buyUnit, surrender}; subAgent.ask(prompt, opts?); log.{info, warn, error}. game.getState returns the cached PlayerView as a JSON STRING — call JSON.parse on it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Full JS source to install" }
                },
                "required": ["code"]
            }
        }),
        json!({
            "name": "get_script",
            "description": "Return the current script source (or empty if none has been set).",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "on_turn",
            "description": "Invoke the user's `onTurn(ctx)` function. The optional `context` argument is forwarded as the first argument to onTurn so you can pass extra hints from the meta-agent. Returns the captured logs and the function's return value (stringified).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "context": { "description": "Anything JSON-serializable; passed as the first argument to onTurn." }
                },
                "required": []
            }
        }),
        json!({
            "name": "eval",
            "description": "Evaluate an arbitrary JS snippet in the same persistent runtime. Useful for ad-hoc inspection and testing snippets before committing them to the script.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": { "type": "string" }
                },
                "required": ["code"]
            }
        }),
        json!({
            "name": "get_state",
            "description": "Return the harness's most recent cached PlayerView (no WS call). Same shape as the game MCP's get_state but without the rules block — this tool is for sanity-checking what the script will see.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
    ]
}

async fn dispatch_tool(
    state: &Arc<HarnessState>,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    match name {
        "set_script" => handle_set_script(state, args).await,
        "get_script" => handle_get_script(state).await,
        "on_turn" => handle_on_turn(state, args).await,
        "eval" => handle_eval(state, args).await,
        "get_state" => handle_get_state(state).await,
        other => Err(format!("unknown tool: {other}")),
    }
}

async fn handle_set_script(state: &Arc<HarnessState>, args: &Value) -> Result<String, String> {
    let code = args
        .get("code")
        .and_then(|v| v.as_str())
        .ok_or("`code` (string) is required")?
        .to_string();
    // Persist first so a compile failure still leaves a record.
    if let Err(e) = std::fs::write(state.script_path(), &code) {
        return Err(format!("write failed: {e}"));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    state
        .qjs_tx
        .send(JsCmd::SetScript { code, ack: tx })
        .map_err(|_| "QuickJS thread dead")?;
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(format!(
            "script installed and saved to scripts/{}.js",
            state.username
        )),
        Ok(Err(e)) => Err(format!("compile error: {e}")),
        Err(_) => Err("QuickJS did not respond in 5s".into()),
    }
}

async fn handle_get_script(state: &Arc<HarnessState>) -> Result<String, String> {
    match std::fs::read_to_string(state.script_path()) {
        Ok(s) => Ok(s),
        Err(_) => Ok(String::new()),
    }
}

async fn handle_on_turn(state: &Arc<HarnessState>, args: &Value) -> Result<String, String> {
    let ctx = args.get("context").cloned().unwrap_or(Value::Null);
    let (tx, rx) = std::sync::mpsc::channel();
    state
        .qjs_tx
        .send(JsCmd::OnTurn { context: ctx, ack: tx })
        .map_err(|_| "QuickJS thread dead")?;
    let res = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(15)))
        .await
        .map_err(|e| format!("await join: {e}"))?;
    match res {
        Ok(Ok(turn_result)) => {
            let mut s = String::new();
            for l in &turn_result.logs {
                s.push_str(&format!("[{}] {}\n", l.level, l.message));
            }
            s.push_str(&format!("\nonTurn returned: {}\n", turn_result.value));
            Ok(s)
        }
        Ok(Err(e)) => Err(format!("script error: {e}")),
        Err(_) => Err("QuickJS did not respond in 15s".into()),
    }
}

async fn handle_eval(state: &Arc<HarnessState>, args: &Value) -> Result<String, String> {
    let code = args
        .get("code")
        .and_then(|v| v.as_str())
        .ok_or("`code` (string) is required")?
        .to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    state
        .qjs_tx
        .send(JsCmd::Eval { code, ack: tx })
        .map_err(|_| "QuickJS thread dead")?;
    let res = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(8)))
        .await
        .map_err(|e| format!("await join: {e}"))?;
    match res {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("QuickJS did not respond in 8s".into()),
    }
}

async fn handle_get_state(state: &Arc<HarnessState>) -> Result<String, String> {
    let view = state.state.lock().await.clone();
    match view {
        Some(v) => serde_json::to_string_pretty(&v).map_err(|e| e.to_string()),
        None => Ok("{}".into()),
    }
}

async fn send_result<W: AsyncWriteExt + Unpin>(
    stdout: &mut W,
    id: Value,
    result: Value,
) -> std::io::Result<()> {
    let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
    let line = format!("{}\n", resp);
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await
}

async fn send_err<W: AsyncWriteExt + Unpin>(
    stdout: &mut W,
    id: Value,
    code: i32,
    msg: String,
) -> std::io::Result<()> {
    let resp = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": msg }
    });
    let line = format!("{}\n", resp);
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await
}

// Suppress unused warning for HashMap import if env-feature changes shake out.
#[allow(dead_code)]
fn _hashmap_marker(_: &HashMap<String, String>) {}
