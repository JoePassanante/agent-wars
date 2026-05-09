//! agent-wars MCP server.
//!
//! Connects to a running agent-wars WebSocket server as a specific player
//! (p1 or p2) and exposes a small tool surface over JSON-RPC on stdio so an
//! LLM agent can read the board, plan, and act.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_wars::game::{Coord, GameState, PlayerId, PlayerView, Terrain, Unit, UnitKind};
use agent_wars::proto::{ClientIntent, ClientMsg, ServerMsg};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(name = "agent-wars-mcp", about = "MCP server for agent-wars")]
struct Args {
    /// Username this agent uses to queue / reconnect. Required.
    /// Aliased as --player for backward compat with old configs.
    #[arg(long, alias = "player")]
    username: String,
    /// agent-wars WebSocket URL.
    #[arg(long, default_value = "ws://127.0.0.1:8080/ws")]
    url: String,
}

struct McpClient {
    /// Set once when the server confirms a Matched / Reconnected attachment.
    /// `OnceLock` so reads are sync and the value can't be reassigned.
    player: std::sync::OnceLock<PlayerId>,
    session_id: std::sync::OnceLock<uuid::Uuid>,
    #[allow(dead_code)]
    username: String,
    state: Mutex<Option<PlayerView>>,
    cmd_tx: mpsc::Sender<ClientMsg>,
    events: broadcast::Sender<ServerMsg>,
}

impl McpClient {
    fn player(&self) -> PlayerId {
        *self
            .player
            .get()
            .expect("MCP tools shouldn't run before matchmaking completes")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    eprintln!(
        "agent-wars MCP starting as username={}, connecting to {}",
        args.username, args.url
    );

    let (ws_stream, _) = connect_async(&args.url).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ClientMsg>(32);
    let (events_tx, _) = broadcast::channel::<ServerMsg>(64);

    let client = Arc::new(McpClient {
        player: std::sync::OnceLock::new(),
        session_id: std::sync::OnceLock::new(),
        username: args.username.clone(),
        state: Mutex::new(None),
        cmd_tx: cmd_tx.clone(),
        events: events_tx.clone(),
    });

    let reader_client = Arc::clone(&client);
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
                *reader_client.state.lock().await = Some(view.clone());
            }
            let _ = reader_client.events.send(server_msg);
        }
        eprintln!("ws reader exiting");
    });

    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            let s = match serde_json::to_string(&cmd) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if ws_tx.send(WsMessage::Text(s)).await.is_err() {
                break;
            }
        }
    });

    // Subscribe before saying Hello so we don't miss the bootstrap messages.
    let mut bootstrap = client.events.subscribe();
    cmd_tx
        .send(ClientMsg::Hello {
            username: args.username.clone(),
            intent: ClientIntent::Play,
        })
        .await?;

    // Walk the bootstrap state machine: Hello → Queued/Reconnected → Matched
    // → State. Note Queued can sit for a while (until another player joins),
    // so we don't bail on Queued; we wait without a deadline once it lands.
    let mut got_state = false;
    let mut hard_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !got_state {
        // Loose 10s budget for the pre-queue phase. Once Queued lands the
        // deadline gets refreshed each tick to "no timeout".
        let now = tokio::time::Instant::now();
        let remaining = if now >= hard_deadline {
            Duration::from_secs(0)
        } else {
            hard_deadline - now
        };
        match timeout(remaining, bootstrap.recv()).await {
            Ok(Ok(ServerMsg::Hello { server_version, .. })) => {
                eprintln!("connected to server v{server_version}");
            }
            Ok(Ok(ServerMsg::Queued { position })) => {
                eprintln!("queued at position {position}; waiting for opponent…");
                hard_deadline = tokio::time::Instant::now() + Duration::from_secs(3600);
            }
            Ok(Ok(ServerMsg::Matched { session_id, role })) => {
                eprintln!("matched into session {session_id} as {role:?}");
                let _ = client.player.set(role);
                let _ = client.session_id.set(session_id);
                hard_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            }
            Ok(Ok(ServerMsg::Reconnected { session_id, role })) => {
                eprintln!("reconnected to session {session_id} as {role:?}");
                let _ = client.player.set(role);
                let _ = client.session_id.set(session_id);
            }
            Ok(Ok(ServerMsg::Spectating { session_id })) => {
                eprintln!("spectating session {session_id} (MCP runs as player only — exiting)");
                return Err("MCP only supports playing, not spectating".into());
            }
            Ok(Ok(ServerMsg::State(_))) => got_state = true,
            Ok(Ok(ServerMsg::Error { message })) => {
                eprintln!("server error: {message}");
                return Err(format!("server error: {message}").into());
            }
            Ok(Err(_)) => return Err("event channel closed during handshake".into()),
            Err(_) => return Err("timed out waiting for matchmaking".into()),
        }
    }

    eprintln!("agent-wars MCP ready");
    json_rpc_loop(client).await?;
    Ok(())
}

// ----------------------------- JSON-RPC layer -----------------------------

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

async fn json_rpc_loop(client: Arc<McpClient>) -> std::io::Result<()> {
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

        // Notifications (no id) get no response.
        let is_notification = req.id.is_none();
        let id = req.id.clone().unwrap_or(Value::Null);

        match req.method.as_str() {
            "initialize" => {
                let result = json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "agent-wars-mcp", "version": "0.1.0" }
                });
                send_result(&mut stdout, id, result).await?;
            }
            "notifications/initialized" => { /* no response */ }
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
                match dispatch_tool(&client, &name, &args).await {
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

// --------------------------------- Tools ---------------------------------

fn list_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "get_state",
            "description": "Get the current game state from your player's perspective. The response includes a rules summary, turn info, your units, visible enemies, building list (yours / enemy with ghost-or-live tag / neutral), funds, and an ASCII map. WIN ONLY by destroying the enemy HQ (10 HP) or by their surrender — losing all your units does NOT end the game.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "legal_moves",
            "description": "List tiles a unit can move to this turn, with movement-point cost. The unit's current tile is included with cost 0 (stay put — legal as a stationary attack base).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "unitId": { "type": "string", "description": "UUID from get_state" }
                },
                "required": ["unitId"]
            }
        }),
        json!({
            "name": "attackable_targets",
            "description": "List enemies a unit could attack this turn, including the tile it would need to stand on to make the attack.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "unitId": { "type": "string" }
                },
                "required": ["unitId"]
            }
        }),
        json!({
            "name": "act",
            "description": "Move a unit and optionally attack. `to` is the destination [x, y] (use the unit's current position to attack from where it stands). If `target` is set, the unit attacks the enemy AT that coord (must be at Manhattan distance 1 from `to`). Only enemy UNITS and HQs can be attacked — cities and factories are captured by ending your turn on them, not damaged. Defender counterattacks if it survives and is in range. Sets has_moved=true so the unit cannot act again this turn. Move a unit ONTO an enemy/neutral city or factory and then end_turn to capture it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "unitId": { "type": "string" },
                    "to": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "minItems": 2, "maxItems": 2,
                        "description": "[x, y] destination"
                    },
                    "target": {
                        "type": ["array", "null"],
                        "items": { "type": "integer" },
                        "minItems": 2, "maxItems": 2,
                        "description": "Optional [x, y] of enemy to attack from `to`"
                    }
                },
                "required": ["unitId", "to"]
            }
        }),
        json!({
            "name": "end_turn",
            "description": "End your turn. Before handing off, the server flips ownership of any capturable building (city/factory) one of your infantry-class units is standing on. Then the other player becomes active, their has_moved flags reset, and they collect income (1000g per HQ/factory/city they own). Each factory becomes available for production again at the start of its owner's turn.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "unit_stats",
            "description": "Return the full unit roster: cost, move points, vision, max HP, attack range, and the damage matrix (% base damage attacker→defender at full HP, before terrain reduction). Useful for planning matchups before committing to an attack.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "simulate_attack",
            "description": "Predict the outcome of one of your units attacking a target without actually committing. Returns expected damage to the defender, defender HP after, and counterattack damage to your unit (if defender survives and is in range). Honors current HP, terrain defense (terrain stars + building bonus), and per-unit defense modifiers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "unitId": { "type": "string", "description": "Your attacking unit's UUID." },
                    "from": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "minItems": 2, "maxItems": 2,
                        "description": "Tile [x, y] the unit would attack from. Must be in attack range of `target`. Defaults to the unit's current position if omitted."
                    },
                    "target": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "minItems": 2, "maxItems": 2,
                        "description": "[x, y] of the enemy unit OR enemy HQ to attack."
                    }
                },
                "required": ["unitId", "target"]
            }
        }),
        json!({
            "name": "buy_unit",
            "description": "Spend funds to spawn a unit at one of your factories. Constraints: (1) factory.owner == you, (2) factory hasn't already produced this turn, (3) the factory tile is empty (no unit standing on it — move it off first), (4) you have enough funds. Newly bought units have has_moved=true and CANNOT act this turn. Costs: scout=1000g, infantry=2000g, heavy_infantry=3000g. Each factory can produce up to ONE unit per turn — separate from the per-unit movement allowance.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "factoryId": { "type": "string" },
                    "kind": {
                        "type": "string",
                        "enum": ["infantry", "scout", "heavy_infantry"]
                    }
                },
                "required": ["factoryId", "kind"]
            }
        }),
        json!({
            "name": "wait_for_turn",
            "description": "Block until it's your turn or the game ends, then return. If your turn doesn't arrive within `timeoutSeconds` (default 50), returns a 'still waiting' status so you can call again. Idle CPU — server pushes you the moment state changes. Use this between rounds instead of polling get_state.",
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
        json!({
            "name": "surrender",
            "description": "Concede the match — the other player wins immediately. The server rejects surrender until at least 3 full turns have elapsed (turn_number >= 4) so a player can't bail out instantly.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "list_sessions",
            "description": "List currently active sessions (lobby browser). Returns each session's id, map size, turn number, active player, and unit counts. Useful for understanding the lobby state but not normally needed for play — your assigned session was given to you in the matchmaking response.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
    ]
}

async fn dispatch_tool(
    client: &Arc<McpClient>,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    match name {
        "get_state" => handle_get_state(client).await,
        "legal_moves" => handle_legal_moves(client, args).await,
        "attackable_targets" => handle_attackable(client, args).await,
        "act" => handle_act(client, args).await,
        "buy_unit" => handle_buy_unit(client, args).await,
        "unit_stats" => Ok(format_unit_stats()),
        "simulate_attack" => handle_simulate_attack(client, args).await,
        "list_sessions" => handle_list_sessions().await,
        "end_turn" => handle_end_turn(client).await,
        "wait_for_turn" => handle_wait_for_turn(client, args).await,
        "surrender" => handle_surrender(client).await,
        other => Err(format!("unknown tool: {other}")),
    }
}

async fn handle_list_sessions() -> Result<String, String> {
    // Fetch the lobby's active sessions from the HTTP endpoint. Cheap; this
    // is a stateless GET so we don't keep a long-running HTTP client.
    let url = "http://127.0.0.1:8080/api/sessions";
    let body = match tokio_tungstenite_fetch_text(url).await {
        Ok(b) => b,
        Err(e) => return Err(format!("failed to fetch session list: {e}")),
    };
    let sessions: Vec<serde_json::Value> =
        serde_json::from_str(&body).map_err(|e| format!("bad session list: {e}"))?;
    if sessions.is_empty() {
        return Ok("No active sessions.\n".into());
    }
    let mut s = String::from("Active sessions:\n");
    for sess in &sessions {
        s.push_str(&format!(
            "  {} — {}x{} map, turn {}, {:?}'s turn{}{}\n",
            sess["id"],
            sess["mapWidth"],
            sess["mapHeight"],
            sess["turnNumber"],
            sess["currentTurn"],
            if sess["hasWinner"].as_bool().unwrap_or(false) {
                " [GAME OVER]"
            } else {
                ""
            },
            if let (Some(p1), Some(p2)) = (sess["p1Units"].as_u64(), sess["p2Units"].as_u64()) {
                format!(" — units P1:{p1} P2:{p2}")
            } else {
                String::new()
            },
        ));
    }
    Ok(s)
}

/// Tiny HTTP GET helper using a raw TCP connection so we don't pull in
/// `reqwest`. Suitable for local same-machine fetches only.
async fn tokio_tungstenite_fetch_text(url: &str) -> std::io::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let parsed = url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::other("only http:// supported"))?;
    let (host_port, path) = match parsed.find('/') {
        Some(i) => (&parsed[..i], &parsed[i..]),
        None => (parsed, "/"),
    };
    let mut stream = TcpStream::connect(host_port).await?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    let mut buf = String::new();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await?;
    let raw = String::from_utf8_lossy(&bytes).to_string();
    if let Some(idx) = raw.find("\r\n\r\n") {
        buf.push_str(&raw[idx + 4..]);
    } else {
        buf.push_str(&raw);
    }
    Ok(buf)
}

async fn handle_get_state(client: &Arc<McpClient>) -> Result<String, String> {
    let view = client
        .state
        .lock()
        .await
        .clone()
        .ok_or("no state yet")?;
    Ok(format_state(&view, client.player()))
}

async fn handle_legal_moves(client: &Arc<McpClient>, args: &Value) -> Result<String, String> {
    let unit_id = parse_unit_id(args)?;
    let view = client.state.lock().await.clone().ok_or("no state yet")?;
    let synth = synthetic_state(&view);
    let unit = synth
        .units
        .get(&unit_id)
        .ok_or_else(|| format!("unit {unit_id} not visible"))?;
    if unit.owner != client.player() {
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

async fn handle_attackable(client: &Arc<McpClient>, args: &Value) -> Result<String, String> {
    let unit_id = parse_unit_id(args)?;
    let view = client.state.lock().await.clone().ok_or("no state yet")?;
    let synth = synthetic_state(&view);
    let unit = synth
        .units
        .get(&unit_id)
        .ok_or_else(|| format!("unit {unit_id} not visible"))?
        .clone();
    if unit.owner != client.player() {
        return Err("not your unit".into());
    }
    let (min_r, max_r) = unit.kind.attack_range();
    let reachable = synth.reachable(unit_id);

    enum TargetEntry {
        Unit(Unit),
        Building(agent_wars::game::Building),
    }
    let mut targets: Vec<(TargetEntry, Coord)> = Vec::new();

    let cheapest_attack_pos = |target_pos: Coord| -> Option<Coord> {
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

    for u in synth.units.values() {
        if u.owner == unit.owner {
            continue;
        }
        if let Some(from) = cheapest_attack_pos(u.pos) {
            targets.push((TargetEntry::Unit(u.clone()), from));
        }
    }
    // Visible enemy buildings (ghost buildings excluded — can't attack what's behind fog).
    for rb in view.buildings.iter() {
        if rb.building.owner == Some(unit.owner) {
            continue;
        }
        if !rb.currently_visible {
            continue;
        }
        if let Some(from) = cheapest_attack_pos(rb.building.pos) {
            targets.push((TargetEntry::Building(rb.building.clone()), from));
        }
    }

    if targets.is_empty() {
        return Ok(format!(
            "Unit {} cannot reach any visible enemy this turn.\n",
            unit_id
        ));
    }
    let mut s = format!("Attackable targets for {}:\n", unit_id);
    for (target, from) in targets {
        match target {
            TargetEntry::Unit(u) => {
                let terrain = view.map.terrain(u.pos).map(terrain_name).unwrap_or("?");
                s.push_str(&format!(
                    "  enemy unit {} hp={} at [{},{}] on {} — attack from [{},{}]\n",
                    u.id, u.hp, u.pos.0, u.pos.1, terrain, from.0, from.1
                ));
            }
            TargetEntry::Building(b) => {
                let terrain = view.map.terrain(b.pos).map(terrain_name).unwrap_or("?");
                s.push_str(&format!(
                    "  enemy HQ {} hp={}/{} at [{},{}] on {} — attack from [{},{}]\n",
                    b.id,
                    b.hp,
                    b.kind.max_hp(),
                    b.pos.0,
                    b.pos.1,
                    terrain,
                    from.0,
                    from.1
                ));
            }
        }
    }
    Ok(s)
}

async fn handle_act(client: &Arc<McpClient>, args: &Value) -> Result<String, String> {
    let unit_id = parse_unit_id(args)?;
    let to = parse_coord(args.get("to"))?;
    let target = match args.get("target") {
        Some(Value::Null) | None => None,
        Some(v) => Some(parse_coord(Some(v))?),
    };

    let prev = client.state.lock().await.clone().ok_or("no state yet")?;
    let mut events = client.events.subscribe();

    client
        .cmd_tx
        .send(ClientMsg::Move {
            unit_id,
            to,
            attack: target,
        })
        .await
        .map_err(|_| "writer task dead".to_string())?;

    match wait_for_response(&mut events).await? {
        ServerMsg::Error { message } => Err(message),
        ServerMsg::State(new_state) => Ok(format_action_report(
            &prev,
            &new_state,
            unit_id,
            to,
            target,
            client.player(),
        )),
        _ => Err("unexpected server response".into()),
    }
}

fn format_unit_stats() -> String {
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
    s.push_str("\nBase damage matrix (attacker → defender, % at full HP, before terrain):\n");
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
    s.push_str("\nVs HQ base damage:\n");
    for &atk in &kinds {
        s.push_str(&format!(
            "  {:<14} {:>3}%\n",
            unit_kind_name(atk),
            atk.base_damage_vs_building(agent_wars::game::BuildingKind::Hq),
        ));
    }
    s.push_str(
        "\nDamage formula: base × (atk_hp/10) × (1 − def_stars × def_hp_ratio × 0.1).\n\
         Defense stars stack: terrain (plains 1 / forest 2 / mountain 4) + building \
         (city/factory: infantry +3, scout/heavy +2; HQ: +4 to its occupant). Cap at 90% reduction.\n",
    );
    s
}

async fn handle_simulate_attack(
    client: &Arc<McpClient>,
    args: &Value,
) -> Result<String, String> {
    let unit_id = parse_unit_id(args)?;
    let target_pos = parse_coord(args.get("target"))?;
    let view = client.state.lock().await.clone().ok_or("no state yet")?;
    let synth = synthetic_state(&view);

    let attacker = synth
        .units
        .get(&unit_id)
        .ok_or_else(|| format!("unit {unit_id} not visible"))?
        .clone();
    if attacker.owner != client.player() {
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

    // Hypothetical attacker — same stats but at `from`.
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
            "Attack {} ({}, hp {}/{}) → {} ({}, hp {}/{}).\n  \
             Defense stars: {} (terrain + building bonus).\n  \
             Forecast: defender takes {} HP (now {} HP).\n",
            unit_kind_name(hypo.kind),
            hypo.id,
            hypo.hp,
            UnitKind::max_hp(hypo.kind),
            unit_kind_name(target_unit.kind),
            target_unit.id,
            target_unit.hp,
            UnitKind::max_hp(target_unit.kind),
            def_stars,
            dmg,
            new_hp,
        );
        if new_hp == 0 {
            s.push_str("  Defender DIES — no counterattack.\n");
        } else {
            // Counter from new_hp.
            let mut counter_def = target_unit.clone();
            counter_def.hp = new_hp;
            let atk_after = hypo.clone();
            // Defender attacks attacker on attacker's tile.
            // Build a synthetic with attacker moved to `from` for stars.
            let mut atk_synth = synth.clone();
            atk_synth.units.insert(hypo.id, atk_after.clone());
            let atk_stars = atk_synth.defense_stars_for_unit(&atk_after);
            let counter = agent_wars::game::compute_damage(&counter_def, atk_stars, &atk_after);
            let atk_new_hp = atk_after.hp.saturating_sub(counter);
            s.push_str(&format!(
                "  Counter: defender deals {} HP back; attacker now {} HP{}.\n",
                counter,
                atk_new_hp,
                if atk_new_hp == 0 {
                    " (DESTROYED)"
                } else {
                    ""
                }
            ));
        }
        Ok(s)
    } else if let Some(rb) = view.buildings.iter().find(|rb| rb.building.pos == target_pos) {
        use agent_wars::game::BuildingKind;
        if rb.building.owner == Some(client.player()) {
            return Err("target is your own building".into());
        }
        if !matches!(rb.building.kind, BuildingKind::Hq) {
            return Err("only HQs are attackable; cities/factories are captured".into());
        }
        let def_stars = synth.defense_stars_for_building(&rb.building);
        let dmg = agent_wars::game::compute_damage_vs_building(&hypo, def_stars, &rb.building);
        let new_hp = rb.building.hp.saturating_sub(dmg);
        Ok(format!(
            "Attack {} ({}, hp {}/{}) → enemy HQ at [{},{}] (hp {}/{}).\n  \
             Defense stars: {}.\n  \
             Forecast: HQ takes {} HP (now {} HP){}.\n  \
             HQs do not counterattack.\n",
            unit_kind_name(hypo.kind),
            hypo.id,
            hypo.hp,
            UnitKind::max_hp(hypo.kind),
            target_pos.0,
            target_pos.1,
            rb.building.hp,
            rb.building.kind.max_hp(),
            def_stars,
            dmg,
            new_hp,
            if new_hp == 0 { " — DESTROYED" } else { "" },
        ))
    } else {
        Err(format!(
            "no attackable target at [{},{}]",
            target_pos.0, target_pos.1
        ))
    }
}

async fn handle_buy_unit(client: &Arc<McpClient>, args: &Value) -> Result<String, String> {
    let factory_id_str = args
        .get("factoryId")
        .and_then(|v| v.as_str())
        .ok_or("factoryId required")?;
    let factory_id: Uuid = factory_id_str
        .parse()
        .map_err(|e: uuid::Error| format!("bad factoryId: {e}"))?;
    let kind_str = args
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or("kind required")?;
    let kind = parse_unit_kind(kind_str)?;

    let prev_funds = client
        .state
        .lock()
        .await
        .as_ref()
        .and_then(|v| v.funds.get(&client.player()).copied())
        .unwrap_or(0);

    let mut events = client.events.subscribe();
    client
        .cmd_tx
        .send(ClientMsg::BuyUnit { factory_id, kind })
        .await
        .map_err(|_| "writer task dead".to_string())?;
    match wait_for_response(&mut events).await? {
        ServerMsg::Error { message } => Err(message),
        ServerMsg::State(new) => {
            let new_funds = new.funds.get(&client.player()).copied().unwrap_or(0);
            let spent = prev_funds.saturating_sub(new_funds);
            Ok(format!(
                "Bought {} for {}g. Funds remaining: {}g.\n",
                unit_kind_name(kind),
                spent,
                new_funds
            ))
        }
        _ => Err("unexpected server response".into()),
    }
}

async fn handle_end_turn(client: &Arc<McpClient>) -> Result<String, String> {
    // Snapshot building ownership and our funds before the turn flips so we
    // can call out captures and the income tick to the agent explicitly —
    // diffing state in their head is where the hallucinations come from.
    let prev = client.state.lock().await.clone();
    let me = client.player();

    let mut events = client.events.subscribe();
    client
        .cmd_tx
        .send(ClientMsg::EndTurn)
        .await
        .map_err(|_| "writer task dead".to_string())?;
    match wait_for_response(&mut events).await? {
        ServerMsg::Error { message } => Err(message),
        ServerMsg::State(new) => {
            let mut s = format!(
                "Turn ended. Now turn {}, active player: {:?}.",
                new.turn_number, new.current_turn
            );
            if let Some(prev_view) = prev {
                // Capture report — every building whose owner changed.
                use agent_wars::game::BuildingKind;
                let kind_label = |k: BuildingKind| match k {
                    BuildingKind::Hq => "HQ",
                    BuildingKind::Factory => "Factory",
                    BuildingKind::City => "City",
                };
                let owner_label = |o: Option<PlayerId>| -> String {
                    match o {
                        None => "neutral".into(),
                        Some(p) if p == me => "you".into(),
                        Some(p) => format!("{:?}", p),
                    }
                };
                for nb in &new.buildings {
                    if let Some(pb) = prev_view
                        .buildings
                        .iter()
                        .find(|p| p.building.id == nb.building.id)
                    {
                        if pb.building.owner != nb.building.owner {
                            s.push_str(&format!(
                                "\nCapture: {} at [{},{}] flipped {} -> {}.",
                                kind_label(nb.building.kind),
                                nb.building.pos.0,
                                nb.building.pos.1,
                                owner_label(pb.building.owner),
                                owner_label(nb.building.owner),
                            ));
                        }
                    }
                }
                // Income tick (fund delta on the incoming player only).
                let active = new.current_turn;
                let before = prev_view.funds.get(&active).copied();
                let after = new.funds.get(&active).copied();
                if let (Some(b), Some(a)) = (before, after) {
                    if a > b {
                        s.push_str(&format!(
                            "\nIncome: {:?} collected {}g (now {}g).",
                            active,
                            a - b,
                            a
                        ));
                    }
                }
            }
            s.push('\n');
            Ok(s)
        }
        _ => Err("unexpected server response".into()),
    }
}

async fn handle_wait_for_turn(
    client: &Arc<McpClient>,
    args: &Value,
) -> Result<String, String> {
    let timeout_secs = args
        .get("timeoutSeconds")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .clamp(1, 120);
    let me = client.player();

    // Subscribe BEFORE the first state read so we never miss a transition.
    let mut events = client.events.subscribe();

    let snapshot = client.state.lock().await.clone();
    if let Some(view) = snapshot {
        if let Some(w) = view.winner {
            let outcome = if w == me { "YOU WIN" } else { "YOU LOSE" };
            return Ok(format!(
                "Game is over. Winner: {:?} ({outcome}). (You: {:?}.)\n",
                w, me
            ));
        }
        if view.current_turn == me {
            return Ok(format!(
                "It's your turn now (turn {}). Call get_state for details.\n",
                view.turn_number
            ));
        }
    }

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let now = Instant::now();
        if now >= deadline {
            let view = client.state.lock().await.clone();
            return Ok(match view {
                Some(v) => format!(
                    "Still {:?}'s turn (turn {}) after {}s. Call wait_for_turn again to keep waiting.\n",
                    v.current_turn, v.turn_number, timeout_secs
                ),
                None => "No state yet — server may not be running.".into(),
            });
        }
        match timeout(deadline - now, events.recv()).await {
            Ok(Ok(_)) => {
                // State changed — re-check.
                let view = client.state.lock().await.clone();
                if let Some(v) = view {
                    if let Some(w) = v.winner {
                        let outcome = if w == me { "YOU WIN" } else { "YOU LOSE" };
                        return Ok(format!(
                            "Game is over. Winner: {:?} ({outcome}). (You: {:?}.)\n",
                            w, me
                        ));
                    }
                    if v.current_turn == me {
                        return Ok(format!(
                            "It's your turn now (turn {}). Call get_state for details.\n",
                            v.turn_number
                        ));
                    }
                }
                // Otherwise keep waiting.
            }
            Ok(Err(_)) => return Err("event channel closed".into()),
            Err(_) => {} // timeout iter — loop and check deadline
        }
    }
}

async fn handle_surrender(client: &Arc<McpClient>) -> Result<String, String> {
    let mut events = client.events.subscribe();
    client
        .cmd_tx
        .send(ClientMsg::Surrender)
        .await
        .map_err(|_| "writer task dead".to_string())?;
    match wait_for_response(&mut events).await? {
        ServerMsg::Error { message } => Err(message),
        ServerMsg::State(new) => Ok(format!(
            "Surrendered. Winner: {:?}.\n",
            new.winner.unwrap_or(client.player().other())
        )),
        _ => Err("unexpected server response".into()),
    }
}

async fn wait_for_response(
    events: &mut broadcast::Receiver<ServerMsg>,
) -> Result<ServerMsg, String> {
    loop {
        match timeout(Duration::from_secs(5), events.recv()).await {
            // Lobby/handshake messages can re-arrive on long-running sessions
            // (e.g., reconnection acks); they aren't responses to commands.
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

// --------------------------- Helpers + formatting ---------------------------

fn parse_unit_id(args: &Value) -> Result<Uuid, String> {
    let s = args
        .get("unitId")
        .and_then(|v| v.as_str())
        .ok_or("unitId required")?;
    s.parse().map_err(|e: uuid::Error| format!("bad unitId: {e}"))
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

fn synthetic_state(view: &PlayerView) -> GameState {
    let units: HashMap<Uuid, Unit> = view.units.iter().map(|u| (u.id, u.clone())).collect();
    let buildings: HashMap<Uuid, agent_wars::game::Building> = view
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

fn format_state(view: &PlayerView, me: PlayerId) -> String {
    let mut s = String::new();
    s.push_str(
        "RULES (read carefully — these are easy to misinfer from AW intuition):\n\n\
         WIN: Only by destroying the enemy HQ (down to 0 HP, total 10 damage) or by their surrender. \
         Wiping out every enemy unit does NOT win — they can rebuild at their factory and you have \
         to crack the HQ. Surrender is allowed from turn 4 onward.\n\n\
         TURN FLOW: Each of your units gets exactly ONE action per turn (move OR move+attack OR \
         stay+attack). You may act with ALL of them in any order — there is NO per-turn limit on how \
         many units you act with. After you've issued every action you want, call end_turn.\n\n\
         MOVEMENT: Each unit has a fixed move-points budget — scout=7, infantry=3, heavy=2. Each \
         tile you ENTER costs:\n  \
         • plains   = 1\n  \
         • forest   = 1 for scouts, 2 for infantry/heavy\n  \
         • mountain = 2 (and a HARD CAP: a unit may enter AT MOST ONE mountain tile per turn, even \
         if it has unspent move points)\n  \
         • sea      = impassable\n\
         Your starting tile is free. The total cost of entered tiles must not exceed your move \
         points. Tiles occupied by other units, by friendly units you can't pass through (no \
         pass-through of friendlies if you'd stop on them), or by HQs are not legal stops/passes.\n\n\
         BUYING UNITS (at factories): scout=1000g, infantry=2000g, heavy_infantry=3000g. Each \
         factory you own can produce ONCE per turn, and only if its tile is empty (move any unit \
         off the factory first). New units have has_moved=true, so they CANNOT act the turn \
         they're built. Production is independent of unit actions.\n\n\
         CAPTURE: Move one of your infantry-class units onto a non-friendly city or factory, then \
         call end_turn. Capture is INSTANT (one full turn standing). HQs are NOT captured — only \
         destroyed by direct attack.\n\n\
         COMBAT: damage = base × (atk_hp / 10) × (1 − def_stars × def_hp_ratio × 0.1). Defenders \
         counterattack at post-hit HP if they survive and remain in range; HQs do not counter. \
         Cities/factories cannot be attacked — only captured. See unit_stats for the damage matrix.\n\n\
         DEFENSE STARS: terrain (plains 1, forest 2, mountain 4) plus a building bonus to whoever \
         occupies it (city/factory infantry +3, scout/heavy +2; HQ to its occupant +4). Stars stack; \
         the reduction caps at 90%.\n\n\
         INCOME (paid at the start of your turn): HQ 1000g, factory 1000g, city 250g — for each \
         that YOU own.\n\n\
         RECOMMENDED LOOP: wait_for_turn → get_state → for each ready unit { legal_moves and/or \
         attackable_targets → optional simulate_attack → act } → for each idle factory { buy_unit } \
         → end_turn. Use unit_stats once if you need the damage matrix.\n\n",
    );
    s.push_str(&format!(
        "Map seed: {}. Turn {}; active player: {:?}; you are {:?}.\n",
        view.map_seed, view.turn_number, view.current_turn, me
    ));
    if let Some(w) = view.winner {
        let outcome = if w == me { "YOU WIN" } else { "YOU LOSE" };
        s.push_str(&format!("GAME OVER — Winner: {:?} ({outcome}).\n", w));
    }
    s.push_str(&format!(
        "Map: {} x {}. Visible tiles: {}/{}.\n",
        view.map.width,
        view.map.height,
        view.visible_tiles.len(),
        view.map.width * view.map.height
    ));
    if let Some(my_funds) = view.funds.get(&me).copied() {
        s.push_str(&format!("Funds: {}g.\n", my_funds));
    }
    s.push('\n');

    // ASCII map.
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

    let buildings_by_pos: HashMap<Coord, &agent_wars::game::RememberedBuilding> = view
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
            // Buildings render on top of units in the ASCII map. If a friendly
            // unit happens to stand on a city/factory, we still draw the building
            // glyph (lowercase) — agents can cross-reference the unit list.
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
                view.map
                    .terrain(pos)
                    .map(terrain_glyph)
                    .unwrap_or('?')
            };
            s.push_str(&format!(" {} ", glyph));
        }
        s.push('\n');
    }
    s.push_str(
        "\nLegend: U=your unit (can act), u=acted, E=enemy unit, \
         H=your HQ, X=enemy HQ, F=your factory, Y=enemy factory, f=neutral factory, \
         C=your city, K=enemy city, c=neutral city, \
         .=plains, ^=forest, M=mountain, ~=sea, ?=fogged. \
         The building list below shows ghost/live status and HP for each building.\n"
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

    // Buildings — partition into your buildings, enemy buildings (with
    // ghost/live status), and neutral buildings (cities you've discovered).
    let mut mine = Vec::new();
    let mut enemy = Vec::new();
    let mut neutral = Vec::new();
    for rb in &view.buildings {
        match rb.building.owner {
            Some(o) if o == me => mine.push(rb),
            Some(_) => enemy.push(rb),
            None => neutral.push(rb),
        }
    }
    let kind_label = |k: agent_wars::game::BuildingKind| match k {
        agent_wars::game::BuildingKind::Hq => "HQ",
        agent_wars::game::BuildingKind::Factory => "Factory",
        agent_wars::game::BuildingKind::City => "City",
    };
    if !mine.is_empty() {
        s.push_str("\nYour buildings:\n");
        for rb in &mine {
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
                rb.building.pos.1,
            ));
        }
    }
    if !enemy.is_empty() {
        s.push_str("\nKnown enemy buildings:\n");
        for rb in &enemy {
            let tag = if rb.currently_visible {
                "live"
            } else {
                "ghost / last seen"
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
    if !neutral.is_empty() {
        s.push_str("\nKnown neutral buildings (capturable):\n");
        for rb in &neutral {
            let tag = if rb.currently_visible {
                "live"
            } else {
                "ghost / last seen"
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

fn building_glyph(rb: &agent_wars::game::RememberedBuilding, me: PlayerId) -> char {
    use agent_wars::game::BuildingKind;
    let mine = rb.building.owner == Some(me);
    let neutral = rb.building.owner.is_none();
    match rb.building.kind {
        BuildingKind::Hq => if mine { 'H' } else { 'X' },
        BuildingKind::Factory => if mine { 'F' } else if neutral { 'f' } else { 'Y' },
        BuildingKind::City => if mine { 'C' } else if neutral { 'c' } else { 'K' },
    }
}

fn unit_kind_name(k: UnitKind) -> &'static str {
    match k {
        UnitKind::Infantry => "infantry",
        UnitKind::Scout => "scout",
        UnitKind::HeavyInfantry => "heavy_infantry",
    }
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
        // Look for the target as either a unit OR a building from the prior state.
        let prev_unit_target = prev.units.iter().find(|u| u.pos == target_pos).cloned();
        let prev_building_target = prev
            .buildings
            .iter()
            .find(|rb| rb.building.pos == target_pos)
            .cloned();

        if let Some(prev_t) = prev_unit_target {
            let new_target = new.units.iter().find(|u| u.id == prev_t.id);
            match new_target {
                None => {
                    s.push_str(&format!(
                        "Defender {} at [{},{}] destroyed!\n",
                        prev_t.id, target_pos.0, target_pos.1
                    ));
                }
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
                None => {
                    s.push_str(&format!(
                        "Enemy {} at [{},{}] DESTROYED!\n",
                        kind_label, target_pos.0, target_pos.1
                    ));
                }
                Some(nb) => {
                    let dmg = prev_b.building.hp.saturating_sub(nb.building.hp);
                    s.push_str(&format!(
                        "Dealt {} HP to enemy {}; now {} HP.\n",
                        dmg, kind_label, nb.building.hp
                    ));
                }
            }
        } else {
            s.push_str(&format!(
                "(no defender found at [{},{}] in prior state)\n",
                target_pos.0, target_pos.1
            ));
        }
        // Counterattack effect on attacker.
        match (prev_unit.as_ref(), new_unit.as_ref()) {
            (Some(p), Some(n)) => {
                let counter = p.hp.saturating_sub(n.hp);
                if counter > 0 {
                    s.push_str(&format!(
                        "Counterattack: took {} HP; attacker now {} HP.\n",
                        counter, n.hp
                    ));
                } else {
                    s.push_str("No counterattack damage taken.\n");
                }
            }
            (Some(_), None) => {
                s.push_str(&format!(
                    "Counterattack killed your unit {}!\n",
                    unit_id
                ));
            }
            _ => {}
        }
    }

    // If the acting unit ended its move on a capturable building someone
    // else (or no one) owns, hint that ending turn here will flip it.
    if let Some(unit_now) = new.units.iter().find(|u| u.id == unit_id) {
        if let Some(rb) = new.buildings.iter().find(|rb| rb.building.pos == unit_now.pos) {
            use agent_wars::game::BuildingKind;
            let mine = rb.building.owner == Some(me);
            if !mine && matches!(rb.building.kind, BuildingKind::City | BuildingKind::Factory) {
                let kind = match rb.building.kind {
                    BuildingKind::City => "city",
                    BuildingKind::Factory => "factory",
                    BuildingKind::Hq => "hq",
                };
                let owner = match rb.building.owner {
                    None => "neutral".to_string(),
                    Some(o) => format!("{:?}", o),
                };
                s.push_str(&format!(
                    "Standing on {} {} at [{},{}] — end your turn here to capture it (instant). \
                     This building is NOT the enemy HQ; you damage HQs by attacking them.\n",
                    owner, kind, unit_now.pos.0, unit_now.pos.1
                ));
            }
        }
    }

    if let Some(w) = new.winner {
        let outcome = if w == me { "YOU WIN" } else { "YOU LOSE" };
        s.push_str(&format!("\nGAME OVER — Winner: {:?} ({outcome}).\n", w));
        s.push_str(&summarize_final_state(new, me));
    }
    s
}

/// At end of game, print a clean rundown of who has what so the agent can
/// understand the result instead of guessing from a fog-filtered last view.
fn summarize_final_state(view: &PlayerView, me: PlayerId) -> String {
    let mut s = String::from("Final state:\n");
    for owner in [PlayerId::P1, PlayerId::P2] {
        let units: Vec<&Unit> = view.units.iter().filter(|u| u.owner == owner).collect();
        let bldgs: Vec<&agent_wars::game::RememberedBuilding> = view
            .buildings
            .iter()
            .filter(|rb| rb.building.owner == Some(owner))
            .collect();
        let label = if owner == me { "you" } else { "opponent" };
        s.push_str(&format!(
            "  {:?} ({}): units={}, buildings={}\n",
            owner,
            label,
            units.len(),
            bldgs.len(),
        ));
    }
    s
}
