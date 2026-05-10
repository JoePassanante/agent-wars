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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use agent_wars::game::{
    Building, Coord, GameState, PlayerId, PlayerView, RememberedBuilding, Terrain, Unit, UnitKind,
};
use agent_wars::lobby::now_secs;
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
    /// Username this harness uses to queue / reconnect. Optional when
    /// `--agent-workspace` is set — defaults to the workspace directory's
    /// basename so `./agent1` runs as username `agent1`.
    #[arg(long, alias = "player")]
    username: Option<String>,
    /// Path to an agent's workspace directory. The harness reads/writes
    /// `script.js` inside this directory instead of the default
    /// `scripts/<username>.js`. Lets you keep one tree per agent (script,
    /// notes, logs, etc.) checked into version control independently.
    #[arg(long, value_name = "DIR")]
    agent_workspace: Option<PathBuf>,
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
    url: String,
    /// Set once the server matches us into a session. RwLock so a future
    /// requeue feature can reset it.
    player: std::sync::RwLock<Option<PlayerId>>,
    session_id: std::sync::RwLock<Option<Uuid>>,
    /// Cached most-recent PlayerView from the WS broadcast.
    state: Mutex<Option<PlayerView>>,
    /// Sender for the JS runtime's game commands.
    game_tx: mpsc::Sender<GameCmd>,
    /// Sender for the JS runtime's sub-agent commands.
    ai_tx: mpsc::Sender<AiCmd>,
    /// Sender for the QuickJS thread's command queue.
    qjs_tx: std::sync::mpsc::Sender<JsCmd>,
    /// Resolved on disk path where the script is read/written.
    script_file: PathBuf,
    /// Workspace dir (or current dir) — workflow.yaml lives here.
    workspace_dir: PathBuf,
    /// Outgoing WS sink. None until `join_queue` is called.
    out_cmd_tx: tokio::sync::RwLock<Option<mpsc::Sender<ClientMsg>>>,
    /// OpenRouter API key, captured at startup so spawn_sub_agent and
    /// the JS subAgent.ask binding both have access without env lookups.
    openrouter_key: Option<String>,
}

impl HarnessState {
    fn script_path(&self) -> &PathBuf {
        &self.script_file
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
    ConfigureSandbox {
        /// Empty list ⇒ default-all. Otherwise: each entry is a global path
        /// like "game.act", "subAgent.ask", "log.info". Top-level objects
        /// are created if any of their members are listed.
        allowed: Vec<String>,
        ack: std::sync::mpsc::Sender<Result<(), String>>,
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

    // Resolve username + script storage. When --agent-workspace is set, we
    // treat the directory as the agent's home: script.js lives there, and
    // the dir's basename is the default username.
    let username = match (&args.username, &args.agent_workspace) {
        (Some(u), _) => u.clone(),
        (None, Some(ws)) => ws
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .ok_or("--agent-workspace path has no basename")?,
        (None, None) => {
            return Err("either --username or --agent-workspace is required".into());
        }
    };
    let script_file = match &args.agent_workspace {
        Some(ws) => {
            std::fs::create_dir_all(ws).ok();
            ws.join("script.js")
        }
        None => {
            let default_dir = PathBuf::from("scripts");
            std::fs::create_dir_all(&default_dir).ok();
            default_dir.join(format!("{username}.js"))
        }
    };

    eprintln!(
        "agent-wars-harness starting as username={username}, script_file={}, connecting to {}",
        script_file.display(),
        args.url,
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
    let (events_tx, _) = broadcast::channel::<ServerMsg>(64);

    // Resolve workspace dir for the workflow.yaml lookup.
    let workspace_dir = args
        .agent_workspace
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));

    let state = Arc::new(HarnessState {
        username: username.clone(),
        url: args.url.clone(),
        player: std::sync::RwLock::new(None),
        session_id: std::sync::RwLock::new(None),
        state: Mutex::new(None),
        game_tx: game_tx.clone(),
        ai_tx: ai_tx.clone(),
        qjs_tx: qjs_tx.clone(),
        script_file,
        workspace_dir,
        out_cmd_tx: tokio::sync::RwLock::new(None),
        openrouter_key: openrouter_key.clone(),
    });

    // Stash events_tx in a OnceLock so reader spawns can reach it whenever
    // join_queue establishes a fresh WS connection.
    EVENTS_TX.set(events_tx).expect("events_tx initialized once");

    // GameCmd dispatcher: turn JS-thread requests into WS sends + waits.
    // The dispatcher itself is connection-agnostic — it consults
    // state.out_cmd_tx every time. Before join_queue: returns "not
    // connected" errors. After: forwards normally.
    let dispatch_state = Arc::clone(&state);
    tokio::spawn(async move {
        while let Some(cmd) = game_rx.recv().await {
            match cmd {
                GameCmd::Snapshot { ack } => {
                    let snapshot = dispatch_state.state.lock().await.clone();
                    let _ = ack.send(snapshot);
                }
                GameCmd::Send { msg, ack } => {
                    let tx = dispatch_state.out_cmd_tx.read().await.clone();
                    let Some(tx) = tx else {
                        let _ = ack.send(Err(
                            "not connected to game — call join_queue first".into(),
                        ));
                        continue;
                    };
                    let mut events = events_tx_handle().subscribe();
                    if tx.send(msg).await.is_err() {
                        let _ = ack.send(Err("writer task dead".into()));
                        continue;
                    }
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
                            eprintln!("[SUB] subAgent.ask called but OPENROUTER_API_KEY unset");
                            let _ = ack.send(Err("OPENROUTER_API_KEY not set".into()));
                            continue;
                        }
                    };
                    let prompt_preview = preview(&prompt, 120);
                    eprintln!(
                        "[SUB] → model={model} prompt({} chars): {}",
                        prompt.len(),
                        prompt_preview
                    );
                    let started = std::time::Instant::now();
                    let resp = call_openrouter(&client, key, &model, &prompt).await;
                    match &resp {
                        Ok(text) => eprintln!(
                            "[SUB] ← {} chars in {}ms: {}",
                            text.len(),
                            started.elapsed().as_millis(),
                            preview(text, 120)
                        ),
                        Err(e) => eprintln!("[SUB] ← error in {}ms: {e}", started.elapsed().as_millis()),
                    }
                    let _ = ack.send(resp);
                }
            }
        }
    });

    // Apply workflow.yaml's sandbox.allowed_globals if present so the
    // agent's persisted gating loads automatically.
    let workflow_path = state.workspace_dir.join("workflow.yaml");
    let workflow = load_workflow(&workflow_path);
    if let Some(wf) = &workflow {
        if let Some(sandbox) = &wf.sandbox {
            if let Some(globals) = &sandbox.allowed_globals {
                eprintln!(
                    "[HARNESS] applying workflow.yaml sandbox.allowed_globals: {globals:?}"
                );
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = state.qjs_tx.send(JsCmd::ConfigureSandbox {
                    allowed: globals.clone(),
                    ack: tx,
                });
                let _ = rx.recv_timeout(Duration::from_secs(5));
            }
        }
    }

    eprintln!(
        "[HARNESS] ready (NOT yet connected to game). Use the `join_queue` MCP tool when the agent is ready to play."
    );

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
            "loaded existing script ({} bytes) from {}",
            code.len(),
            state.script_path().display()
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
                *state.player.write().unwrap() = Some(role);
                *state.session_id.write().unwrap() = Some(session_id);
                deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            }
            Ok(Ok(ServerMsg::Reconnected { session_id, role })) => {
                eprintln!("reconnected to session {session_id} as {role:?}");
                *state.player.write().unwrap() = Some(role);
                *state.session_id.write().unwrap() = Some(session_id);
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

/// Open a fresh WS connection to the game server, spawn reader/writer tasks
/// against it, send Hello + intent: Play, and walk the matchmaking handshake.
/// Mutates state.out_cmd_tx, state.player, state.session_id on success.
async fn connect_to_game(state: &Arc<HarnessState>) -> Result<(), String> {
    if state.out_cmd_tx.read().await.is_some() {
        return Err("already connected — duplicate join_queue".into());
    }
    eprintln!("[HARNESS] connecting to game at {}", state.url);
    let (ws_stream, _) = connect_async(&state.url)
        .await
        .map_err(|e| format!("connect_async failed: {e}"))?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let (out_tx, mut out_rx) = mpsc::channel::<ClientMsg>(32);
    *state.out_cmd_tx.write().await = Some(out_tx.clone());

    // Reader: drains WS into events broadcast + state cache.
    let reader_state = Arc::clone(state);
    tokio::spawn(async move {
        while let Some(msg) = ws_rx.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[WS<-] read error: {e}");
                    break;
                }
            };
            let WsMessage::Text(t) = msg else { continue };
            let server_msg: ServerMsg = match serde_json::from_str(&t) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[WS<-] bad server msg: {e}");
                    continue;
                }
            };
            log_server_msg(&server_msg);
            if let ServerMsg::State(view) = &server_msg {
                *reader_state.state.lock().await = Some(view.clone());
            }
            let _ = events_tx_handle().send(server_msg);
        }
        eprintln!("[WS<-] reader exiting (connection closed)");
    });

    // Writer: drains out_rx → ws sink.
    tokio::spawn(async move {
        while let Some(cmd) = out_rx.recv().await {
            log_client_msg(&cmd);
            let s = match serde_json::to_string(&cmd) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if ws_tx.send(WsMessage::Text(s)).await.is_err() {
                eprintln!("[WS->] sink closed; writer exiting");
                break;
            }
        }
    });

    // Send Hello + walk handshake.
    let mut bootstrap = events_tx_handle().subscribe();
    out_tx
        .send(ClientMsg::Hello {
            username: state.username.clone(),
            intent: ClientIntent::Play,
        })
        .await
        .map_err(|_| "writer task dead before Hello".to_string())?;
    handshake(state, &mut bootstrap)
        .await
        .map_err(|e| e.to_string())?;
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

    // (Re)install all JS globals, gated by an optional allowlist. An empty
    // allowlist exposes everything. Otherwise an entry like "game" enables
    // all members of the game namespace, and "game.act" enables only that
    // method. Called once at startup (allowed=empty) and again whenever
    // configure_sandbox arrives.
    let do_install = |allowed: HashSet<String>| -> Result<(), String> {
        let want = |path: &str| -> bool {
            if allowed.is_empty() {
                return true;
            }
            if allowed.contains(path) {
                return true;
            }
            if let Some((parent, _)) = path.rsplit_once('.') {
                if allowed.contains(parent) {
                    return true;
                }
            }
            false
        };
        context.with(|ctx| -> Result<(), String> {
            let globals = ctx.globals();

            // ---- log namespace ----
            if want("log") || want("log.info") || want("log.warn") || want("log.error") {
                let log_obj = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
                for (level_name, level_owned) in [
                    ("info", "info"),
                    ("warn", "warn"),
                    ("error", "error"),
                ] {
                    let path = format!("log.{level_name}");
                    if !want(&path) {
                        continue;
                    }
                    let buf = log_buffer.clone();
                    let level_str = level_owned.to_string();
                    let f = Function::new(ctx.clone(), move |msg: rquickjs::Value| {
                        let s = stringify_value(&msg);
                        eprintln!("[JS] {level_str}: {s}");
                        buf.lock().unwrap().push(LogLine {
                            level: level_str.clone(),
                            message: s,
                        });
                    })
                    .map_err(|e| e.to_string())?;
                    log_obj.set(level_name, f).map_err(|e| e.to_string())?;
                }
                globals.set("log", log_obj).map_err(|e| e.to_string())?;
            } else {
                let _ = globals.remove("log");
            }

            // ---- game namespace ----
            let game_obj = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
            let _want_game_root = want("game");

            // game.getState
            if want("game.getState") {
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

            if want("game.act") {
                let game_tx = game_tx.clone();
                let f = Function::new(ctx.clone(), move |unit_id: String, to: rquickjs::Value, target: rquickjs::Value| -> rquickjs::Result<String> {
                    let to_coord = parse_coord_js(&to).map_err(|_| rquickjs::Error::Exception)?;
                    let attack = if target.is_undefined() || target.is_null() {
                        None
                    } else {
                        Some(parse_coord_js(&target).map_err(|_| rquickjs::Error::Exception)?)
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

            if want("game.endTurn") {
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

            if want("game.buyUnit") {
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

            if want("game.surrender") {
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

            if want("game.playTurn") {
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

            // Identity hint — re-read state.player on every (re)install so a
            // configure_sandbox after join_queue picks up the role.
            if let Some(p) = *state.player.read().unwrap() {
                let role = match p {
                    PlayerId::P1 => "p1",
                    PlayerId::P2 => "p2",
                };
                game_obj.set("you", role).map_err(|e| e.to_string())?;
            }

            globals.set("game", game_obj).map_err(|e| e.to_string())?;

            // ---- subAgent namespace ----
            if want("subAgent.ask") {
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
            } else {
                let _ = globals.remove("subAgent");
            }

            Ok(())
        })
    };

    if let Err(e) = do_install(HashSet::new()) {
        eprintln!("failed to install JS globals: {e}");
        return;
    }

    // Command loop.
    while let Ok(cmd) = rx.recv() {
        match cmd {
            JsCmd::SetScript { code, ack } => {
                eprintln!("[JS] set_script ({} chars)", code.len());
                let result: Result<(), String> = context.with(|ctx| {
                    ctx.eval::<(), _>(code.as_bytes())
                        .catch(&ctx)
                        .map(|_| ())
                        .map_err(|e| format!("{e:?}"))
                });
                if let Err(e) = &result {
                    eprintln!("[JS] set_script compile error: {}", preview(e, 120));
                }
                let _ = ack.send(result);
            }
            JsCmd::Eval { code, ack } => {
                eprintln!("[JS] eval ({} chars): {}", code.len(), preview(&code, 80));
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
                eprintln!("[JS] on_turn invoked");
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
            JsCmd::ConfigureSandbox { allowed, ack } => {
                eprintln!("[JS] configure_sandbox: {} entries", allowed.len());
                let set: HashSet<String> = allowed.into_iter().collect();
                let _ = ack.send(do_install(set));
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

fn parse_coord_js(v: &rquickjs::Value) -> Result<(i32, i32), String> {
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
                let to = parse_coord_js(&to_v)?;
                let attack = if let Ok(t) = obj.get::<_, rquickjs::Value>("target") {
                    if t.is_undefined() || t.is_null() {
                        None
                    } else {
                        Some(parse_coord_js(&t)?)
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

/// One-line stderr summary of an outgoing client message. Avoids spamming
/// the full payload — usually one or two key fields per variant.
fn log_client_msg(msg: &ClientMsg) {
    match msg {
        ClientMsg::Hello { username, intent } => {
            let intent_label = match intent {
                ClientIntent::Play => "play",
                ClientIntent::Watch { session_id } => &format!("watch({session_id})"),
            };
            eprintln!("[WS->] Hello username={username} intent={intent_label}");
        }
        ClientMsg::Move { unit_id, to, attack } => {
            let atk = attack
                .map(|c| format!(" attack=[{},{}]", c.0, c.1))
                .unwrap_or_default();
            eprintln!("[WS->] Move {unit_id} → [{},{}]{atk}", to.0, to.1);
        }
        ClientMsg::BuyUnit { factory_id, kind } => {
            eprintln!("[WS->] BuyUnit {kind:?} at factory {factory_id}");
        }
        ClientMsg::EndTurn => eprintln!("[WS->] EndTurn"),
        ClientMsg::Surrender => eprintln!("[WS->] Surrender"),
        ClientMsg::Leave => eprintln!("[WS->] Leave"),
        ClientMsg::PlayTurn { actions } => {
            eprintln!("[WS->] PlayTurn batch ({} actions)", actions.len());
        }
    }
}

/// Truncate a string for log lines, replacing newlines with `⏎` so each
/// log entry stays one line.
fn preview(s: &str, max: usize) -> String {
    let one_line: String = s.chars().map(|c| if c == '\n' { '⏎' } else { c }).collect();
    if one_line.chars().count() <= max {
        one_line
    } else {
        let truncated: String = one_line.chars().take(max).collect();
        format!("{truncated}…")
    }
}

fn log_server_msg(msg: &ServerMsg) {
    match msg {
        ServerMsg::Hello {
            username,
            server_version,
        } => eprintln!("[WS<-] Hello username={username} server_v{server_version}"),
        ServerMsg::Queued { position } => {
            eprintln!("[WS<-] Queued at position {position}")
        }
        ServerMsg::Matched { session_id, role } => {
            eprintln!("[WS<-] Matched into {session_id} as {role:?}")
        }
        ServerMsg::Reconnected { session_id, role } => {
            eprintln!("[WS<-] Reconnected to {session_id} as {role:?}")
        }
        ServerMsg::Spectating { session_id } => {
            eprintln!("[WS<-] Spectating {session_id}")
        }
        ServerMsg::State(view) => {
            let outcome = if let Some(w) = view.winner {
                format!(" WINNER={w:?}")
            } else if view.is_draw {
                " DRAW".into()
            } else {
                String::new()
            };
            eprintln!(
                "[WS<-] State turn={} active={:?}{}",
                view.turn_number, view.current_turn, outcome,
            );
        }
        ServerMsg::Error { message } => eprintln!("[WS<-] Error: {message}"),
    }
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
        // ---- Lifecycle / preparation -----
        json!({
            "name": "join_queue",
            "description": "Connect to the game server and queue this player for matchmaking. The harness DOES NOT auto-queue at startup — call this only when your script and any sub-agent setup are ready to fight. Once connected, the server matches you against another queued player and the rest of the game-control tools (act, play_turn, etc.) start working.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "configure_sandbox",
            "description": "Restrict which globals the QuickJS script can see. Pass a list of allowed-global paths like ['game.act', 'game.getState', 'subAgent.ask', 'log.info']. The script's runtime is rebuilt with only those exposed; everything else is unset. Pass an empty list (or omit) to reset to the default of ALL globals exposed. Useful when you want to gate what your scripted sub-harness can do.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "allowed": {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                },
                "required": []
            }
        }),
        json!({
            "name": "get_workflow",
            "description": "Read <workspace>/workflow.yaml as a string. Returns empty string if not present. Schema (all optional):\nsandbox:\n  allowed_globals: [list of paths]\nsub_agents:\n  <name>:\n    model: <openrouter model id>\n    system: <system prompt>\n    tools: [list of tool names]\nThe harness loads this file at startup and re-loads on set_workflow.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "set_workflow",
            "description": "Write workflow.yaml to the agent's workspace. Replaces any existing file. Triggers a reload so configure_sandbox / sub-agent presets take effect immediately.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "yaml": { "type": "string" }
                },
                "required": ["yaml"]
            }
        }),
        json!({
            "name": "spawn_sub_agent",
            "description": "Run a smaller LLM with tool-call access via OpenRouter. The sub-agent receives `prompt`, runs a tool-use loop using only `allowedTools` (each tool is a name from this harness's tool surface), and returns its final assistant message. Use this for the 'mid-case' workflow branch — heavy enough to need an LLM but cheaper than waking the meta-agent. Default model: anthropic/claude-haiku-4-5. Caps at 8 iterations.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" },
                    "model": { "type": "string", "description": "OpenRouter model id; default anthropic/claude-haiku-4-5" },
                    "system": { "type": "string", "description": "Optional system prompt." },
                    "allowedTools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Subset of this harness's tool names the sub-agent may call. Empty = no tools (just answer)."
                    },
                    "maxIterations": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 32,
                        "description": "Max chat turns before bailing. Default 8."
                    }
                },
                "required": ["prompt"]
            }
        }),
        json!({
            "name": "wait_for_turn",
            "description": "Block until it's THIS player's turn (or the game ends), then return. Use this between rounds — the canonical loop is `wait_for_turn → on_turn / act / play_turn → wait_for_turn → ...`. Soft `timeoutSeconds` budget (default 50) so the call returns even if no progress happens; just call again if you get back a 'still waiting' response.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timeoutSeconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 120,
                        "description": "Max seconds to block. Default 50."
                    }
                },
                "required": []
            }
        }),

        // ---- Direct game control. The meta-agent can play without writing any
        //      script by calling these directly. They mirror the agent-wars-mcp
        //      binary so a single harness registration is all you need.
        json!({
            "name": "act",
            "description": "Move a unit and optionally attack from the destination. `to` is the destination [x, y]; `target` (optional) is [x, y] of an enemy unit or HQ that must be in attack range from `to`. Sets has_moved on the unit. Defenders counterattack at post-hit HP if alive. Cities/factories are captured by ending your turn standing on them, NOT attacked.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "unitId": { "type": "string" },
                    "to": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2 },
                    "target": { "type": ["array", "null"], "items": { "type": "integer" }, "minItems": 2, "maxItems": 2 }
                },
                "required": ["unitId", "to"]
            }
        }),
        json!({
            "name": "play_turn",
            "description": "Submit your entire turn in ONE round-trip: an ordered list of move/buy/endTurn actions. Stops at first error (prior actions stay committed). Strongly recommended for the 10s budget — one MCP call instead of N. Action format: { type:'move', unitId, to:[x,y], target?:[x,y] } | { type:'buyUnit', factoryId, kind } | { type:'endTurn' }.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "actions": {
                        "type": "array",
                        "items": { "type": "object", "properties": { "type": { "type": "string" } }, "required": ["type"] }
                    }
                },
                "required": ["actions"]
            }
        }),
        json!({
            "name": "end_turn",
            "description": "End your turn. Capture flips for any of your infantry-class units sitting on a non-friendly capturable building, then the other player gets active and collects income.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "buy_unit",
            "description": "Spend funds to spawn a unit at one of your factories. Constraints: factory.owner == you, factory tile empty, factory hasn't produced this turn, sufficient funds. Costs: scout=1000g, infantry=2000g, heavy_infantry=3000g. Newly bought units have has_moved=true and CAN'T act this turn.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "factoryId": { "type": "string" },
                    "kind": { "type": "string", "enum": ["infantry", "scout", "heavy_infantry"] }
                },
                "required": ["factoryId", "kind"]
            }
        }),
        json!({
            "name": "surrender",
            "description": "Concede the match. Allowed only after turn 4. Idle 5 consecutive turns also auto-surrenders.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "legal_moves",
            "description": "List tiles a unit can move to this turn, with movement-point cost. The unit's current tile is included with cost 0 (stay put — legal as a stationary attack base).",
            "inputSchema": {
                "type": "object",
                "properties": { "unitId": { "type": "string" } },
                "required": ["unitId"]
            }
        }),
        json!({
            "name": "attackable_targets",
            "description": "List enemies a unit could attack this turn, including the cheapest reachable tile to stand on. Covers both visible enemy units and live enemy HQs.",
            "inputSchema": {
                "type": "object",
                "properties": { "unitId": { "type": "string" } },
                "required": ["unitId"]
            }
        }),
        json!({
            "name": "simulate_attack",
            "description": "Predict the outcome of attacking a target without committing. Returns expected damage to the defender, defender HP after, counter damage to attacker. Honors current HP and defense-stars stacking.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "unitId": { "type": "string" },
                    "from": { "type": ["array", "null"], "items": { "type": "integer" }, "minItems": 2, "maxItems": 2 },
                    "target": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2 }
                },
                "required": ["unitId", "target"]
            }
        }),
        json!({
            "name": "unit_stats",
            "description": "Return the full unit roster (cost / move / vision / hp / range) plus the base damage matrix and HQ damage row. Static info — useful for one-shot planning during script authoring.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
    ]
}

async fn dispatch_tool(
    state: &Arc<HarnessState>,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    eprintln!("[TOOL] → {name}");
    let started = std::time::Instant::now();
    let result = match name {
        // Lifecycle.
        "join_queue" => handle_join_queue(state).await,
        "configure_sandbox" => handle_configure_sandbox(state, args).await,
        "get_workflow" => handle_get_workflow(state).await,
        "set_workflow" => handle_set_workflow(state, args).await,
        "spawn_sub_agent" => handle_spawn_sub_agent(state, args).await,
        // Script / sandbox tools.
        "set_script" => handle_set_script(state, args).await,
        "get_script" => handle_get_script(state).await,
        "on_turn" => handle_on_turn(state, args).await,
        "eval" => handle_eval(state, args).await,
        // Inspection / planning.
        "get_state" => handle_get_state(state).await,
        "wait_for_turn" => handle_wait_for_turn(state, args).await,
        "legal_moves" => handle_legal_moves(state, args).await,
        "attackable_targets" => handle_attackable(state, args).await,
        "simulate_attack" => handle_simulate_attack(state, args).await,
        "unit_stats" => Ok(handle_unit_stats()),
        // Direct game control.
        "act" => handle_act(state, args).await,
        "play_turn" => handle_play_turn_mcp(state, args).await,
        "end_turn" => handle_end_turn(state).await,
        "buy_unit" => handle_buy_unit(state, args).await,
        "surrender" => handle_surrender_mcp(state).await,
        other => Err(format!("unknown tool: {other}")),
    };
    let elapsed = started.elapsed().as_millis();
    match &result {
        Ok(text) => eprintln!(
            "[TOOL] ← {name} ok in {elapsed}ms ({} chars)",
            text.len()
        ),
        Err(e) => eprintln!("[TOOL] ← {name} ERR in {elapsed}ms: {}", preview(e, 120)),
    }
    result
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
            "script installed and saved to {}",
            state.script_path().display()
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

async fn handle_wait_for_turn(
    state: &Arc<HarnessState>,
    args: &Value,
) -> Result<String, String> {
    let timeout_secs = args
        .get("timeoutSeconds")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .clamp(1, 120);
    let me = current_player(state)?;

    // Subscribe BEFORE checking state to avoid missing the transition.
    let mut events = events_tx_handle().subscribe();

    // Fast path: maybe it's already our turn.
    if let Some(view) = state.state.lock().await.clone() {
        if let Some(w) = view.winner {
            let outcome = if w == me { "YOU WIN" } else { "YOU LOSE" };
            return Ok(format!(
                "GAME OVER. Winner: {w:?} ({outcome}). The harness is still attached to this finished session — \
                 to play another match you must restart this binary (the MCP doesn't currently support \
                 in-process re-queueing). Stop accepting tool calls now."
            ));
        }
        if view.is_draw {
            return Ok("GAME OVER (draw — both players idled). Restart the harness binary to play another match.".into());
        }
        if view.current_turn == me {
            return Ok(format!(
                "It's your turn now (turn {}, deadline in ~{}s). Call on_turn next.",
                view.turn_number,
                view.turn_deadline_secs.saturating_sub(now_secs())
            ));
        }
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            let view = state.state.lock().await.clone();
            return Ok(match view {
                Some(v) => format!(
                    "Still {:?}'s turn (turn {}) after {}s. Call wait_for_turn again to keep waiting.",
                    v.current_turn, v.turn_number, timeout_secs
                ),
                None => "No state yet — server may not be running.".into(),
            });
        }
        match timeout(deadline - now, events.recv()).await {
            Ok(Ok(_)) => {
                // State changed somehow — re-check.
                let view = state.state.lock().await.clone();
                if let Some(v) = view {
                    if let Some(w) = v.winner {
                        let outcome = if w == me { "YOU WIN" } else { "YOU LOSE" };
                        return Ok(format!(
                            "GAME OVER. Winner: {w:?} ({outcome}). Restart the harness binary to play another match."
                        ));
                    }
                    if v.is_draw {
                        return Ok("GAME OVER (draw — both players idled). Restart the harness binary to play another match.".into());
                    }
                    if v.current_turn == me {
                        return Ok(format!(
                            "It's your turn now (turn {}, deadline in ~{}s). Call on_turn next.",
                            v.turn_number,
                            v.turn_deadline_secs.saturating_sub(now_secs())
                        ));
                    }
                }
            }
            Ok(Err(_)) => return Err("event channel closed".into()),
            Err(_) => {} // outer loop hits the deadline branch
        }
    }
}

async fn handle_get_state(state: &Arc<HarnessState>) -> Result<String, String> {
    let view = state.state.lock().await.clone().ok_or("no state yet")?;
    let me = current_player(state)?;
    Ok(format_state(&view, me))
}

async fn handle_join_queue(state: &Arc<HarnessState>) -> Result<String, String> {
    connect_to_game(state).await?;
    let player = current_player(state)?;
    let session_id = state
        .session_id
        .read()
        .unwrap()
        .map(|u| u.to_string())
        .unwrap_or_else(|| "?".into());
    Ok(format!(
        "Matched! You are {player:?} in session {session_id}. Direct play and on_turn are now live."
    ))
}

async fn handle_configure_sandbox(
    state: &Arc<HarnessState>,
    args: &Value,
) -> Result<String, String> {
    let allowed: Vec<String> = args
        .get("allowed")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let (tx, rx) = std::sync::mpsc::channel();
    state
        .qjs_tx
        .send(JsCmd::ConfigureSandbox {
            allowed: allowed.clone(),
            ack: tx,
        })
        .map_err(|_| "QuickJS thread dead")?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| "QuickJS did not respond in 5s")??;
    Ok(if allowed.is_empty() {
        "Sandbox reset: all globals exposed.".into()
    } else {
        format!(
            "Sandbox reconfigured: only [{}] exposed.",
            allowed.join(", ")
        )
    })
}

async fn handle_get_workflow(state: &Arc<HarnessState>) -> Result<String, String> {
    let path = state.workspace_dir.join("workflow.yaml");
    Ok(std::fs::read_to_string(&path).unwrap_or_default())
}

async fn handle_set_workflow(
    state: &Arc<HarnessState>,
    args: &Value,
) -> Result<String, String> {
    let yaml = args
        .get("yaml")
        .and_then(|v| v.as_str())
        .ok_or("`yaml` (string) is required")?;
    // Validate it parses.
    let _: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|e| format!("invalid YAML: {e}"))?;
    let path = state.workspace_dir.join("workflow.yaml");
    std::fs::write(&path, yaml).map_err(|e| format!("write failed: {e}"))?;
    Ok(format!("workflow.yaml saved at {}", path.display()))
}

#[derive(Default, serde::Deserialize)]
struct WorkflowFile {
    sandbox: Option<WorkflowSandbox>,
    sub_agents: Option<HashMap<String, WorkflowSubAgent>>,
}

#[derive(serde::Deserialize)]
struct WorkflowSandbox {
    allowed_globals: Option<Vec<String>>,
}

#[derive(Clone, serde::Deserialize)]
struct WorkflowSubAgent {
    model: Option<String>,
    system: Option<String>,
    tools: Option<Vec<String>>,
}

fn load_workflow(path: &std::path::Path) -> Option<WorkflowFile> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_yaml::from_str::<WorkflowFile>(&text) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("[HARNESS] workflow.yaml parse error: {e}");
            None
        }
    }
}

async fn handle_spawn_sub_agent(
    state: &Arc<HarnessState>,
    args: &Value,
) -> Result<String, String> {
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("prompt required")?
        .to_string();
    let model = args
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("anthropic/claude-haiku-4-5")
        .to_string();
    let system = args
        .get("system")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let allowed_tools: Vec<String> = args
        .get("allowedTools")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let max_iter = args
        .get("maxIterations")
        .and_then(|v| v.as_u64())
        .unwrap_or(8) as usize;

    let api_key = state
        .openrouter_key
        .as_deref()
        .ok_or("OPENROUTER_API_KEY not set")?;

    eprintln!(
        "[SUB-AGENT] → model={model} tools={} prompt({} chars)",
        allowed_tools.len(),
        prompt.len()
    );

    // Build OpenAI-compatible tool definitions for whichever names the
    // meta-agent allowed. We re-export each tool's MCP schema verbatim.
    let all_tools = list_tools();
    let tool_defs: Vec<Value> = all_tools
        .iter()
        .filter(|t| {
            t.get("name")
                .and_then(|n| n.as_str())
                .map(|n| allowed_tools.iter().any(|a| a == n))
                .unwrap_or(false)
        })
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t["name"],
                    "description": t["description"],
                    "parameters": t["inputSchema"],
                }
            })
        })
        .collect();

    let mut messages: Vec<Value> = Vec::new();
    if let Some(s) = system {
        messages.push(json!({"role": "system", "content": s}));
    }
    messages.push(json!({"role": "user", "content": prompt}));

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    for iter in 0..max_iter {
        let mut body = json!({
            "model": &model,
            "messages": &messages,
        });
        if !tool_defs.is_empty() {
            body["tools"] = Value::Array(tool_defs.clone());
        }
        let resp = client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("openrouter request: {e}"))?;
        let status = resp.status();
        let json: Value = resp
            .json()
            .await
            .map_err(|e| format!("openrouter parse: {e}"))?;
        if !status.is_success() {
            return Err(format!("openrouter http {status}: {json}"));
        }
        let msg = json["choices"][0]["message"].clone();
        if msg.is_null() {
            return Err(format!("no message in response: {json}"));
        }

        // Append assistant message verbatim so subsequent loop iterations
        // include the right tool_call_ids.
        messages.push(msg.clone());

        let tool_calls = msg.get("tool_calls").and_then(|v| v.as_array()).cloned();
        if let Some(calls) = tool_calls {
            if calls.is_empty() {
                let content = msg["content"].as_str().unwrap_or("").to_string();
                eprintln!("[SUB-AGENT] ← final after {iter} iterations: {}", preview(&content, 120));
                return Ok(content);
            }
            for tc in calls {
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                let raw_args = tc["function"]["arguments"].as_str().unwrap_or("{}");
                let parsed_args: Value =
                    serde_json::from_str(raw_args).unwrap_or(Value::Null);
                eprintln!(
                    "[SUB-AGENT] tool_call → {name}({})",
                    preview(raw_args, 120)
                );
                let result = match Box::pin(dispatch_tool(state, &name, &parsed_args)).await {
                    Ok(s) => s,
                    Err(e) => format!("Error: {e}"),
                };
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tc["id"],
                    "name": name,
                    "content": result,
                }));
            }
        } else {
            // No tool calls — return the assistant text.
            let content = msg["content"].as_str().unwrap_or("").to_string();
            eprintln!(
                "[SUB-AGENT] ← final after {iter} iterations: {}",
                preview(&content, 120)
            );
            return Ok(content);
        }
    }
    Err(format!(
        "sub-agent exceeded max_iterations ({max_iter}) without producing a final message"
    ))
}

// ============================================================
// Direct game-control tools (Phase 1 of the meta-harness merge).
// Mirror the standalone agent-wars-mcp binary so a single MCP
// registration is enough for the meta-agent to play directly.
// ============================================================

async fn send_via_dispatcher(
    state: &Arc<HarnessState>,
    msg: ClientMsg,
) -> Result<ServerMsg, String> {
    let (tx, rx) = oneshot::channel();
    state
        .game_tx
        .send(GameCmd::Send { msg, ack: tx })
        .await
        .map_err(|_| "writer task dead".to_string())?;
    rx.await.map_err(|_| "ack channel closed".to_string())?
}

fn current_player(state: &Arc<HarnessState>) -> Result<PlayerId, String> {
    state
        .player
        .read()
        .unwrap()
        .ok_or_else(|| "harness not yet matched into a session — call join_queue first".to_string())
}

async fn cached_view(state: &Arc<HarnessState>) -> Result<PlayerView, String> {
    state
        .state
        .lock()
        .await
        .clone()
        .ok_or_else(|| "no state yet".to_string())
}

async fn handle_act(state: &Arc<HarnessState>, args: &Value) -> Result<String, String> {
    let me = current_player(state)?;
    let unit_id = parse_unit_id(args)?;
    let to = parse_coord(args.get("to"))?;
    let target = match args.get("target") {
        Some(Value::Null) | None => None,
        Some(v) => Some(parse_coord(Some(v))?),
    };
    let prev = cached_view(state).await?;
    match send_via_dispatcher(
        state,
        ClientMsg::Move {
            unit_id,
            to,
            attack: target,
        },
    )
    .await?
    {
        ServerMsg::Error { message } => Err(message),
        ServerMsg::State(new) => Ok(format_action_report(
            &prev, &new, unit_id, to, target, me,
        )),
        _ => Err("unexpected server response".into()),
    }
}

async fn handle_play_turn_mcp(
    state: &Arc<HarnessState>,
    args: &Value,
) -> Result<String, String> {
    let me = current_player(state)?;
    let raw = args
        .get("actions")
        .and_then(|v| v.as_array())
        .ok_or("expected `actions` array")?;
    if raw.is_empty() {
        return Err("actions array is empty".into());
    }
    let mut actions: Vec<TurnAction> = Vec::with_capacity(raw.len());
    for (i, v) in raw.iter().enumerate() {
        let t = v
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("action[{i}]: missing `type`"))?;
        match t {
            "move" => {
                let unit_id =
                    parse_uuid_field(v, "unitId").map_err(|e| format!("action[{i}].{e}"))?;
                let to = parse_coord(v.get("to"))
                    .map_err(|e| format!("action[{i}].to: {e}"))?;
                let attack = match v.get("target") {
                    Some(Value::Null) | None => None,
                    Some(c) => {
                        Some(parse_coord(Some(c)).map_err(|e| format!("action[{i}].target: {e}"))?)
                    }
                };
                actions.push(TurnAction::Move { unit_id, to, attack });
            }
            "buyUnit" => {
                let factory_id = parse_uuid_field(v, "factoryId")
                    .map_err(|e| format!("action[{i}].{e}"))?;
                let kind_str = v
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("action[{i}].kind: required"))?;
                let kind =
                    parse_unit_kind(kind_str).map_err(|e| format!("action[{i}].kind: {e}"))?;
                actions.push(TurnAction::BuyUnit { factory_id, kind });
            }
            "endTurn" => actions.push(TurnAction::EndTurn),
            other => return Err(format!("action[{i}]: unknown type '{other}'")),
        }
    }
    match send_via_dispatcher(state, ClientMsg::PlayTurn { actions }).await? {
        ServerMsg::Error { message } => Err(message),
        ServerMsg::State(new) => Ok(format!(
            "play_turn applied. Turn {}, active {:?} (you are {:?}). {}s remaining.\n",
            new.turn_number,
            new.current_turn,
            me,
            new.turn_deadline_secs.saturating_sub(now_secs())
        )),
        _ => Err("unexpected server response".into()),
    }
}

async fn handle_end_turn(state: &Arc<HarnessState>) -> Result<String, String> {
    match send_via_dispatcher(state, ClientMsg::EndTurn).await? {
        ServerMsg::Error { message } => Err(message),
        ServerMsg::State(new) => Ok(format!(
            "Turn ended. Now turn {}, active {:?}.",
            new.turn_number, new.current_turn
        )),
        _ => Err("unexpected server response".into()),
    }
}

async fn handle_surrender_mcp(state: &Arc<HarnessState>) -> Result<String, String> {
    match send_via_dispatcher(state, ClientMsg::Surrender).await? {
        ServerMsg::Error { message } => Err(message),
        ServerMsg::State(new) => Ok(format!(
            "Surrendered. Winner: {:?}.",
            new.winner.unwrap_or(current_player(state)?.other())
        )),
        _ => Err("unexpected server response".into()),
    }
}

async fn handle_buy_unit(state: &Arc<HarnessState>, args: &Value) -> Result<String, String> {
    let me = current_player(state)?;
    let factory_id = parse_uuid_field(args, "factoryId")?;
    let kind_str = args
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or("kind required")?;
    let kind = parse_unit_kind(kind_str)?;
    let prev_funds = cached_view(state)
        .await
        .ok()
        .and_then(|v| v.funds.get(&me).copied())
        .unwrap_or(0);
    match send_via_dispatcher(state, ClientMsg::BuyUnit { factory_id, kind }).await? {
        ServerMsg::Error { message } => Err(message),
        ServerMsg::State(new) => {
            let new_funds = new.funds.get(&me).copied().unwrap_or(0);
            let spent = prev_funds.saturating_sub(new_funds);
            Ok(format!(
                "Bought {} for {}g. Funds remaining: {}g.",
                unit_kind_name(kind),
                spent,
                new_funds
            ))
        }
        _ => Err("unexpected server response".into()),
    }
}

async fn handle_legal_moves(state: &Arc<HarnessState>, args: &Value) -> Result<String, String> {
    let unit_id = parse_unit_id(args)?;
    let view = cached_view(state).await?;
    let synth = synthetic_state(&view);
    let unit = synth
        .units
        .get(&unit_id)
        .ok_or_else(|| format!("unit {unit_id} not visible"))?;
    let me = current_player(state)?;
    if unit.owner != me {
        return Err("not your unit".into());
    }
    let reachable = synth.reachable(unit_id);
    let mut tiles: Vec<((i32, i32), u32)> = reachable.into_iter().collect();
    tiles.sort_by_key(|((x, y), c)| (*c, *x, *y));
    let mut s = format!(
        "Legal destinations for {} at [{},{}] (move points = {}):\n",
        unit_id,
        unit.pos.0,
        unit.pos.1,
        unit.kind.move_points()
    );
    for ((x, y), cost) in tiles {
        s.push_str(&format!("  [{x},{y}] cost {cost}\n"));
    }
    Ok(s)
}

async fn handle_attackable(state: &Arc<HarnessState>, args: &Value) -> Result<String, String> {
    let unit_id = parse_unit_id(args)?;
    let view = cached_view(state).await?;
    let synth = synthetic_state(&view);
    let me = current_player(state)?;
    let unit = synth
        .units
        .get(&unit_id)
        .ok_or_else(|| format!("unit {unit_id} not visible"))?
        .clone();
    if unit.owner != me {
        return Err("not your unit".into());
    }
    let (min_r, max_r) = unit.kind.attack_range();
    let reachable = synth.reachable(unit_id);
    let cheapest = |target_pos: Coord| -> Option<Coord> {
        let mut best: Option<(Coord, u32)> = None;
        for (&pos, &cost) in &reachable {
            let d = (pos.0 - target_pos.0).abs() + (pos.1 - target_pos.1).abs();
            if d >= min_r && d <= max_r {
                best = match best {
                    None => Some((pos, cost)),
                    Some((_, prev)) if cost < prev => Some((pos, cost)),
                    other => other,
                };
            }
        }
        best.map(|(p, _)| p)
    };
    let mut s = format!("Attackable targets for {}:\n", unit_id);
    let mut any = false;
    for u in synth.units.values() {
        if u.owner == unit.owner {
            continue;
        }
        if let Some(from) = cheapest(u.pos) {
            any = true;
            let terrain = view.map.terrain(u.pos).map(terrain_name).unwrap_or("?");
            s.push_str(&format!(
                "  unit {} hp={} at [{},{}] on {} — attack from [{},{}]\n",
                u.id, u.hp, u.pos.0, u.pos.1, terrain, from.0, from.1
            ));
        }
    }
    for rb in &view.buildings {
        if rb.building.owner == Some(unit.owner) {
            continue;
        }
        if !rb.currently_visible {
            continue;
        }
        if let Some(from) = cheapest(rb.building.pos) {
            any = true;
            let terrain = view.map.terrain(rb.building.pos).map(terrain_name).unwrap_or("?");
            s.push_str(&format!(
                "  building {:?} hp={}/{} at [{},{}] on {} — attack from [{},{}]\n",
                rb.building.kind,
                rb.building.hp,
                rb.building.kind.max_hp(),
                rb.building.pos.0,
                rb.building.pos.1,
                terrain,
                from.0,
                from.1
            ));
        }
    }
    if !any {
        return Ok(format!(
            "Unit {} cannot reach any visible enemy this turn.\n",
            unit_id
        ));
    }
    Ok(s)
}

async fn handle_simulate_attack(
    state: &Arc<HarnessState>,
    args: &Value,
) -> Result<String, String> {
    let unit_id = parse_unit_id(args)?;
    let target_pos = parse_coord(args.get("target"))?;
    let view = cached_view(state).await?;
    let synth = synthetic_state(&view);
    let me = current_player(state)?;
    let attacker = synth
        .units
        .get(&unit_id)
        .ok_or_else(|| format!("unit {unit_id} not visible"))?
        .clone();
    if attacker.owner != me {
        return Err("not your unit".into());
    }
    let from = match args.get("from") {
        Some(Value::Null) | None => attacker.pos,
        Some(v) => parse_coord(Some(v))?,
    };
    let manhattan = (from.0 - target_pos.0).abs() + (from.1 - target_pos.1).abs();
    let (min_r, max_r) = attacker.kind.attack_range();
    if manhattan < min_r || manhattan > max_r {
        return Err(format!(
            "target out of range from {:?} (need {}-{}, got {})",
            from, min_r, max_r, manhattan
        ));
    }
    let mut hypo = attacker.clone();
    hypo.pos = from;
    if let Some(target_unit) = synth.unit_at(target_pos).cloned() {
        if target_unit.owner == attacker.owner {
            return Err("target is friendly".into());
        }
        let def_stars = synth.defense_stars_for_unit(&target_unit);
        let dmg = agent_wars::game::compute_damage(&hypo, def_stars, &target_unit);
        let new_hp = target_unit.hp.saturating_sub(dmg);
        let mut s = format!(
            "Forecast: defender takes {} HP (now {}/{}). def_stars={}. ",
            dmg,
            new_hp,
            UnitKind::max_hp(target_unit.kind),
            def_stars
        );
        if new_hp == 0 {
            s.push_str("Defender DIES — no counterattack.\n");
        } else {
            let mut counter_def = target_unit.clone();
            counter_def.hp = new_hp;
            let atk_after = hypo.clone();
            let mut atk_synth = synth.clone();
            atk_synth.units.insert(hypo.id, atk_after.clone());
            let atk_stars = atk_synth.defense_stars_for_unit(&atk_after);
            let counter =
                agent_wars::game::compute_damage(&counter_def, atk_stars, &atk_after);
            let atk_new_hp = atk_after.hp.saturating_sub(counter);
            s.push_str(&format!(
                "Counter: attacker takes {} HP (now {}/{}){}.\n",
                counter,
                atk_new_hp,
                UnitKind::max_hp(atk_after.kind),
                if atk_new_hp == 0 { " (DESTROYED)" } else { "" }
            ));
        }
        Ok(s)
    } else if let Some(rb) = view.buildings.iter().find(|rb| rb.building.pos == target_pos) {
        use agent_wars::game::BuildingKind;
        if rb.building.owner == Some(me) {
            return Err("target is your own building".into());
        }
        if !matches!(rb.building.kind, BuildingKind::Hq) {
            return Err("only HQs are attackable; cities/factories are captured".into());
        }
        let def_stars = synth.defense_stars_for_building(&rb.building);
        let dmg = agent_wars::game::compute_damage_vs_building(&hypo, def_stars, &rb.building);
        let new_hp = rb.building.hp.saturating_sub(dmg);
        Ok(format!(
            "Forecast vs HQ at [{},{}]: takes {} HP (now {}/{}){}. HQs do not counter.\n",
            target_pos.0,
            target_pos.1,
            dmg,
            new_hp,
            rb.building.kind.max_hp(),
            if new_hp == 0 { " — DESTROYED" } else { "" }
        ))
    } else {
        Err(format!(
            "no attackable target at [{},{}]",
            target_pos.0, target_pos.1
        ))
    }
}

fn handle_unit_stats() -> String {
    use agent_wars::game::UnitKind::*;
    let kinds = [Infantry, Scout, HeavyInfantry];
    let mut s = String::from("Unit roster:\n");
    for &k in &kinds {
        s.push_str(&format!(
            "  {:<14} cost={:>4}g  move={}  vision={}  hp_max={}  range={:?}\n",
            unit_kind_name(k),
            k.cost(),
            k.move_points(),
            k.vision(),
            k.max_hp(),
            k.attack_range(),
        ));
    }
    s.push_str("\nBase damage matrix (% at full HP, before terrain):\n");
    s.push_str("                    vs Inf  vs Scout  vs Heavy\n");
    for &atk in &kinds {
        let row: Vec<String> = kinds
            .iter()
            .map(|d| {
                atk.base_damage(*d)
                    .map(|n| format!("{:>5}%", n))
                    .unwrap_or_else(|| "   --".into())
            })
            .collect();
        s.push_str(&format!(
            "  {:<14}  {}\n",
            unit_kind_name(atk),
            row.join("   ")
        ));
    }
    s.push_str("\nVs HQ:\n");
    for &atk in &kinds {
        s.push_str(&format!(
            "  {:<14} {:>3}%\n",
            unit_kind_name(atk),
            atk.base_damage_vs_building(agent_wars::game::BuildingKind::Hq),
        ));
    }
    s
}

// =================== Helpers (duplicated from mcp.rs) ===================

fn parse_unit_id(args: &Value) -> Result<Uuid, String> {
    let s = args
        .get("unitId")
        .and_then(|v| v.as_str())
        .ok_or("unitId required")?;
    s.parse().map_err(|e: uuid::Error| format!("bad unitId: {e}"))
}

fn parse_uuid_field(v: &Value, field: &str) -> Result<Uuid, String> {
    let s = v
        .get(field)
        .and_then(|s| s.as_str())
        .ok_or_else(|| format!("{field}: required"))?;
    s.parse().map_err(|e: uuid::Error| format!("{field}: {e}"))
}

fn parse_coord(v: Option<&Value>) -> Result<Coord, String> {
    let arr = v.and_then(|v| v.as_array()).ok_or("expected [x, y]")?;
    if arr.len() != 2 {
        return Err("coord must be [x, y]".into());
    }
    let x = arr[0].as_i64().ok_or("x must be integer")? as i32;
    let y = arr[1].as_i64().ok_or("y must be integer")? as i32;
    Ok((x, y))
}

fn parse_unit_kind(s: &str) -> Result<UnitKind, String> {
    match s.to_lowercase().replace('-', "_").as_str() {
        "infantry" => Ok(UnitKind::Infantry),
        "scout" => Ok(UnitKind::Scout),
        "heavy_infantry" | "heavyinfantry" | "heavy" => Ok(UnitKind::HeavyInfantry),
        other => Err(format!(
            "unknown unit kind '{other}' (use infantry, scout, or heavy_infantry)"
        )),
    }
}

fn synthetic_state(view: &PlayerView) -> GameState {
    let units: HashMap<Uuid, Unit> = view.units.iter().map(|u| (u.id, u.clone())).collect();
    let buildings: HashMap<Uuid, Building> = view
        .buildings
        .iter()
        .map(|rb| (rb.building.id, rb.building.clone()))
        .collect();
    GameState {
        map: view.map.clone(),
        units,
        buildings,
        current_turn: view.current_turn,
        turn_number: view.turn_number,
        winner: view.winner,
        last_action: view.last_action.clone(),
        seen_buildings: HashMap::new(),
        hq_owners: std::collections::HashSet::new(),
        funds: HashMap::new(),
        factories_used: std::collections::HashSet::new(),
        map_seed: view.map_seed,
        actions_this_turn: 0,
        idle_streak: HashMap::new(),
        is_draw: view.is_draw,
    }
}

fn terrain_name(t: Terrain) -> &'static str {
    match t {
        Terrain::Plains => "plains",
        Terrain::Forest => "forest",
        Terrain::Mountain => "mountain",
        Terrain::Sea => "sea",
    }
}

fn terrain_glyph(t: Terrain) -> char {
    match t {
        Terrain::Plains => '.',
        Terrain::Forest => '^',
        Terrain::Mountain => 'M',
        Terrain::Sea => '~',
    }
}

fn unit_kind_name(k: UnitKind) -> &'static str {
    match k {
        UnitKind::Infantry => "infantry",
        UnitKind::Scout => "scout",
        UnitKind::HeavyInfantry => "heavy_infantry",
    }
}

fn building_glyph(rb: &RememberedBuilding, me: PlayerId) -> char {
    use agent_wars::game::BuildingKind;
    let mine = rb.building.owner == Some(me);
    let neutral = rb.building.owner.is_none();
    match rb.building.kind {
        BuildingKind::Hq => if mine { 'H' } else { 'X' },
        BuildingKind::Factory => if mine { 'F' } else if neutral { 'f' } else { 'Y' },
        BuildingKind::City => if mine { 'C' } else if neutral { 'c' } else { 'K' },
    }
}

fn format_state(view: &PlayerView, me: PlayerId) -> String {
    let mut s = String::new();
    s.push_str(
        "RULES (read carefully): WIN by destroying the enemy HQ (10 HP) or by their surrender — \
         losing all units does NOT lose the game. Each turn has a 10s wallclock budget; the server \
         auto-ends an idle turn for you. 5 consecutive idle turns auto-surrenders on turn 6; if BOTH \
         players idle 5 turns, the match ends in a DRAW. Cities/factories are CAPTURED by ending \
         turn standing on them, not attacked. Per-unit movement: scout=7, infantry=3, heavy=2; \
         forest costs 1 for scouts but 2 for infantry/heavy; mountain is 2 with a HARD CAP of one \
         mountain entry per turn. Damage = base × (atk_hp/10) × (1 − def_stars × def_hp_ratio × 0.1). \
         Defense stars: terrain (plains 1, forest 2, mountain 4) + building bonus when occupying \
         (city/factory: infantry +3, others +2; HQ +4). Income at start of your turn: HQ 100g, \
         factory 1000g, city 250g per owned. Surrender allowed from turn 4.\n\n",
    );
    s.push_str(&format!(
        "Map seed: {}. Turn {}; active player: {:?}; you are {:?}.\n",
        view.map_seed, view.turn_number, view.current_turn, me
    ));
    let remaining = view
        .turn_deadline_secs
        .saturating_sub(now_secs());
    if view.current_turn == me {
        s.push_str(&format!(
            "Turn clock: {remaining}s remaining (server auto-ends at 0s).\n"
        ));
    } else {
        s.push_str(&format!(
            "Turn clock: {remaining}s on opponent's turn.\n"
        ));
    }
    if let Some(w) = view.winner {
        let outcome = if w == me { "YOU WIN" } else { "YOU LOSE" };
        s.push_str(&format!("GAME OVER — Winner: {:?} ({outcome}).\n", w));
    }
    if view.is_draw {
        s.push_str("GAME OVER — DRAW (both players idled too long).\n");
    }
    s.push_str(&format!(
        "Map: {} x {}. Visible tiles: {}/{}. Funds: {}g.\n\n",
        view.map.width,
        view.map.height,
        view.visible_tiles.len(),
        view.map.width * view.map.height,
        view.funds.get(&me).copied().unwrap_or(0),
    ));

    let mine_pos: HashMap<Coord, &Unit> = view
        .units
        .iter()
        .filter(|u| u.owner == me)
        .map(|u| (u.pos, u))
        .collect();
    let theirs_pos: HashMap<Coord, &Unit> = view
        .units
        .iter()
        .filter(|u| u.owner != me)
        .map(|u| (u.pos, u))
        .collect();
    let visible: std::collections::HashSet<Coord> = view.visible_tiles.iter().copied().collect();
    let buildings_by_pos: HashMap<Coord, &RememberedBuilding> = view
        .buildings
        .iter()
        .map(|rb| (rb.building.pos, rb))
        .collect();

    s.push_str("   ");
    for x in 0..view.map.width {
        s.push_str(&format!("{:>2} ", x));
    }
    s.push('\n');
    for y in 0..view.map.height {
        s.push_str(&format!("{:>2} ", y));
        for x in 0..view.map.width {
            let pos = (x, y);
            let glyph = if let Some(u) = mine_pos.get(&pos) {
                if buildings_by_pos.contains_key(&pos) {
                    building_glyph(buildings_by_pos[&pos], me)
                } else if u.has_moved {
                    'u'
                } else {
                    'U'
                }
            } else if let Some(rb) = buildings_by_pos.get(&pos) {
                building_glyph(rb, me)
            } else if !visible.contains(&pos) {
                '?'
            } else if theirs_pos.contains_key(&pos) {
                'E'
            } else {
                view.map.terrain(pos).map(terrain_glyph).unwrap_or('?')
            };
            s.push_str(&format!(" {} ", glyph));
        }
        s.push('\n');
    }
    s.push_str(
        "\nLegend: U=your unit READY, u=acted, E=enemy unit, H=your HQ, X=enemy HQ, F=your factory, \
         Y=enemy factory, f=neutral factory, C=your city, K=enemy city, c=neutral city, \
         .=plains, ^=forest, M=mountain, ~=sea, ?=fogged.\n"
    );

    let mut mine: Vec<&Unit> = view.units.iter().filter(|u| u.owner == me).collect();
    let mut theirs: Vec<&Unit> = view.units.iter().filter(|u| u.owner != me).collect();
    mine.sort_by_key(|u| (u.pos.1, u.pos.0));
    theirs.sort_by_key(|u| (u.pos.1, u.pos.0));
    let ready = mine.iter().filter(|u| !u.has_moved).count();
    let acted = mine.len() - ready;
    s.push_str(&format!(
        "\nYour units ({}, {} READY / {} acted):\n",
        mine.len(),
        ready,
        acted
    ));
    for u in &mine {
        let terrain = view.map.terrain(u.pos).map(terrain_name).unwrap_or("?");
        s.push_str(&format!(
            "  {} {} hp={}/{} pos=[{},{}] on {}  mv={} vis={}  [{}]\n",
            u.id,
            unit_kind_name(u.kind),
            u.hp,
            UnitKind::max_hp(u.kind),
            u.pos.0,
            u.pos.1,
            terrain,
            u.kind.move_points(),
            u.kind.vision(),
            if u.has_moved { "ACTED" } else { "READY" },
        ));
    }
    s.push_str(&format!("\nVisible enemy units ({}):\n", theirs.len()));
    for u in &theirs {
        let terrain = view.map.terrain(u.pos).map(terrain_name).unwrap_or("?");
        s.push_str(&format!(
            "  {} {} hp={}/{} pos=[{},{}] on {}  mv={} vis={}\n",
            u.id,
            unit_kind_name(u.kind),
            u.hp,
            UnitKind::max_hp(u.kind),
            u.pos.0,
            u.pos.1,
            terrain,
            u.kind.move_points(),
            u.kind.vision(),
        ));
    }

    let mut mine_b = Vec::new();
    let mut enemy_b = Vec::new();
    let mut neutral_b = Vec::new();
    for rb in &view.buildings {
        match rb.building.owner {
            Some(o) if o == me => mine_b.push(rb),
            Some(_) => enemy_b.push(rb),
            None => neutral_b.push(rb),
        }
    }
    let kind_label = |k: agent_wars::game::BuildingKind| match k {
        agent_wars::game::BuildingKind::Hq => "HQ",
        agent_wars::game::BuildingKind::Factory => "Factory",
        agent_wars::game::BuildingKind::City => "City",
    };
    if !mine_b.is_empty() {
        s.push_str("\nYour buildings:\n");
        for rb in &mine_b {
            let extra = if rb.building.kind == agent_wars::game::BuildingKind::Hq {
                format!(" hp={}/{}", rb.building.hp, rb.building.kind.max_hp())
            } else if rb.building.kind == agent_wars::game::BuildingKind::Factory {
                format!(" id={}", rb.building.id)
            } else {
                String::new()
            };
            s.push_str(&format!(
                "  {}{} at [{},{}]\n",
                kind_label(rb.building.kind),
                extra,
                rb.building.pos.0,
                rb.building.pos.1
            ));
        }
    }
    if !enemy_b.is_empty() {
        s.push_str("\nKnown enemy buildings:\n");
        for rb in &enemy_b {
            let tag = if rb.currently_visible {
                "live"
            } else {
                "ghost"
            };
            let extra = if rb.building.kind == agent_wars::game::BuildingKind::Hq {
                format!(" hp={}/{}", rb.building.hp, rb.building.kind.max_hp())
            } else {
                String::new()
            };
            s.push_str(&format!(
                "  {}{} at [{},{}] ({}, last seen turn {})\n",
                kind_label(rb.building.kind),
                extra,
                rb.building.pos.0,
                rb.building.pos.1,
                tag,
                rb.last_seen_turn,
            ));
        }
    }
    if !neutral_b.is_empty() {
        s.push_str("\nKnown neutral buildings (capturable):\n");
        for rb in &neutral_b {
            let tag = if rb.currently_visible {
                "live"
            } else {
                "ghost"
            };
            s.push_str(&format!(
                "  {} at [{},{}] ({}, last seen turn {})\n",
                kind_label(rb.building.kind),
                rb.building.pos.0,
                rb.building.pos.1,
                tag,
                rb.last_seen_turn,
            ));
        }
    }
    s
}

fn format_action_report(
    prev: &PlayerView,
    new: &PlayerView,
    unit_id: Uuid,
    to: Coord,
    target: Option<Coord>,
    me: PlayerId,
) -> String {
    let mut s = String::new();
    let prev_unit = prev.units.iter().find(|u| u.id == unit_id).cloned();
    let new_unit = new.units.iter().find(|u| u.id == unit_id).cloned();
    if let Some(prev_u) = &prev_unit {
        if prev_u.pos == to {
            s.push_str(&format!(
                "Unit {} stayed at [{},{}].\n",
                unit_id, to.0, to.1
            ));
        } else {
            s.push_str(&format!(
                "Unit {} moved [{},{}] -> [{},{}].\n",
                unit_id, prev_u.pos.0, prev_u.pos.1, to.0, to.1
            ));
        }
    }
    if let Some(target_pos) = target {
        let prev_unit_target = prev.units.iter().find(|u| u.pos == target_pos).cloned();
        let prev_building_target = prev
            .buildings
            .iter()
            .find(|rb| rb.building.pos == target_pos)
            .cloned();
        if let Some(prev_t) = prev_unit_target {
            let new_target = new.units.iter().find(|u| u.id == prev_t.id);
            match new_target {
                None => s.push_str(&format!(
                    "Defender {} at [{},{}] destroyed!\n",
                    prev_t.id, target_pos.0, target_pos.1
                )),
                Some(nt) => {
                    let dmg = prev_t.hp.saturating_sub(nt.hp);
                    s.push_str(&format!(
                        "Dealt {} HP to {}; defender now {} HP.\n",
                        dmg, prev_t.id, nt.hp
                    ));
                }
            }
        } else if let Some(prev_b) = prev_building_target {
            let new_b = new
                .buildings
                .iter()
                .find(|rb| rb.building.id == prev_b.building.id);
            let kind_label = match prev_b.building.kind {
                agent_wars::game::BuildingKind::Hq => "HQ",
                agent_wars::game::BuildingKind::Factory => "factory",
                agent_wars::game::BuildingKind::City => "city",
            };
            match new_b {
                None => s.push_str(&format!(
                    "Enemy {} at [{},{}] DESTROYED!\n",
                    kind_label, target_pos.0, target_pos.1
                )),
                Some(nb) => {
                    let dmg = prev_b.building.hp.saturating_sub(nb.building.hp);
                    s.push_str(&format!(
                        "Dealt {} HP to enemy {}; now {} HP.\n",
                        dmg, kind_label, nb.building.hp
                    ));
                }
            }
        }
        match (prev_unit.as_ref(), new_unit.as_ref()) {
            (Some(p), Some(n)) => {
                let counter = p.hp.saturating_sub(n.hp);
                if counter > 0 {
                    s.push_str(&format!(
                        "Counter: took {} HP; attacker now {} HP.\n",
                        counter, n.hp
                    ));
                }
            }
            (Some(_), None) => {
                s.push_str(&format!("Counter killed your unit {}!\n", unit_id));
            }
            _ => {}
        }
    }
    if let Some(w) = new.winner {
        let outcome = if w == me { "YOU WIN" } else { "YOU LOSE" };
        s.push_str(&format!("\nGAME OVER — Winner: {:?} ({outcome}).\n", w));
    }
    if new.is_draw {
        s.push_str("\nGAME OVER — DRAW (both players idled too long).\n");
    }
    s
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
