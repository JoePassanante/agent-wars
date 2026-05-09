//! agent-wars MCP server.
//!
//! Connects to a running agent-wars WebSocket server as a specific player
//! (p1 or p2) and exposes a small tool surface over JSON-RPC on stdio so an
//! LLM agent can read the board, plan, and act.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agent_wars::game::{Coord, GameState, PlayerId, PlayerView, Terrain, Unit, UnitKind, View};
use agent_wars::proto::{ClientMsg, ServerMsg};
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
    /// Which player this agent controls (p1 or p2).
    #[arg(long, value_parser = parse_player)]
    player: PlayerId,
    /// agent-wars WebSocket URL.
    #[arg(long, default_value = "ws://127.0.0.1:8080/ws")]
    url: String,
}

fn parse_player(s: &str) -> Result<PlayerId, String> {
    match s.to_lowercase().as_str() {
        "p1" | "player1" | "1" => Ok(PlayerId::P1),
        "p2" | "player2" | "2" => Ok(PlayerId::P2),
        _ => Err(format!("expected p1 or p2, got '{s}'")),
    }
}

struct McpClient {
    player: PlayerId,
    state: Mutex<Option<PlayerView>>,
    cmd_tx: mpsc::Sender<ClientMsg>,
    events: broadcast::Sender<ServerMsg>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    eprintln!(
        "agent-wars MCP starting as {:?}, connecting to {}",
        args.player, args.url
    );

    let (ws_stream, _) = connect_async(&args.url).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ClientMsg>(32);
    let (events_tx, _) = broadcast::channel::<ServerMsg>(64);

    let client = Arc::new(McpClient {
        player: args.player,
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

    // Subscribe before joining so we don't miss the initial State push.
    let mut bootstrap = client.events.subscribe();
    cmd_tx
        .send(ClientMsg::Join {
            view: View::Player(args.player),
        })
        .await?;

    // Wait for the initial state (or fail fast).
    let mut got_state = false;
    while !got_state {
        match timeout(Duration::from_secs(5), bootstrap.recv()).await {
            Ok(Ok(ServerMsg::State(_))) => got_state = true,
            Ok(Ok(ServerMsg::Joined { .. })) => {}
            Ok(Ok(ServerMsg::Error { message })) => {
                eprintln!("server error during join: {message}");
            }
            _ => break,
        }
    }
    if !got_state {
        return Err("never received initial state".into());
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
            "description": "Get the current game state from your player's perspective: turn number, whose turn it is, your units (id, position, hp, hasMoved), visible enemy units, terrain map (rendered ASCII + raw tiles), and fog-of-war coverage.",
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
            "description": "Move a unit and optionally attack. `to` is the destination [x, y] (use the unit's current position for a stationary attack). If `target` is set, the unit attacks the enemy at that coord (must be at Manhattan distance 1 from `to` for infantry). Server rules: defender counterattacks if it survives and is in range; sets has_moved on the attacker so the unit cannot act again this turn.",
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
            "description": "End your turn. The other player will then be active. The server resets has_moved flags for the player whose turn becomes active.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "surrender",
            "description": "Concede the match — the other player wins immediately. The server rejects surrender until at least 3 full turns have elapsed (turn_number >= 4) so a player can't bail out instantly.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "reset_lobby",
            "description": "Reset the entire match to its starting state. All connected clients (browser, agents) re-sync.",
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
        "end_turn" => handle_end_turn(client).await,
        "surrender" => handle_surrender(client).await,
        "reset_lobby" => handle_reset(client).await,
        other => Err(format!("unknown tool: {other}")),
    }
}

async fn handle_get_state(client: &Arc<McpClient>) -> Result<String, String> {
    let view = client
        .state
        .lock()
        .await
        .clone()
        .ok_or("no state yet")?;
    Ok(format_state(&view, client.player))
}

async fn handle_legal_moves(client: &Arc<McpClient>, args: &Value) -> Result<String, String> {
    let unit_id = parse_unit_id(args)?;
    let view = client.state.lock().await.clone().ok_or("no state yet")?;
    let synth = synthetic_state(&view);
    let unit = synth
        .units
        .get(&unit_id)
        .ok_or_else(|| format!("unit {unit_id} not visible"))?;
    if unit.owner != client.player {
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
    if unit.owner != client.player {
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
        if rb.building.owner == unit.owner {
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
            client.player,
        )),
        _ => Err("unexpected server response".into()),
    }
}

async fn handle_end_turn(client: &Arc<McpClient>) -> Result<String, String> {
    let mut events = client.events.subscribe();
    client
        .cmd_tx
        .send(ClientMsg::EndTurn)
        .await
        .map_err(|_| "writer task dead".to_string())?;
    match wait_for_response(&mut events).await? {
        ServerMsg::Error { message } => Err(message),
        ServerMsg::State(new) => Ok(format!(
            "Turn ended. Now turn {}, active player: {:?}.\n",
            new.turn_number, new.current_turn
        )),
        _ => Err("unexpected server response".into()),
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
            new.winner.unwrap_or(client.player.other())
        )),
        _ => Err("unexpected server response".into()),
    }
}

async fn handle_reset(client: &Arc<McpClient>) -> Result<String, String> {
    let mut events = client.events.subscribe();
    client
        .cmd_tx
        .send(ClientMsg::Reset)
        .await
        .map_err(|_| "writer task dead".to_string())?;
    match wait_for_response(&mut events).await? {
        ServerMsg::Error { message } => Err(message),
        ServerMsg::State(_) => Ok("Lobby reset to fresh state.\n".into()),
        _ => Err("unexpected server response".into()),
    }
}

async fn wait_for_response(
    events: &mut broadcast::Receiver<ServerMsg>,
) -> Result<ServerMsg, String> {
    loop {
        match timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Ok(ServerMsg::Joined { .. })) => continue,
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
        Terrain::Forest => 'F',
        Terrain::Mountain => 'M',
        Terrain::Sea => '~',
    }
}

fn format_state(view: &PlayerView, me: PlayerId) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Turn {}; active player: {:?}; you are {:?}.\n",
        view.turn_number, view.current_turn, me
    ));
    if let Some(w) = view.winner {
        s.push_str(&format!("GAME OVER — winner: {:?}.\n", w));
    }
    s.push_str(&format!(
        "Map: {} x {}. Visible tiles: {}/{}.\n\n",
        view.map.width,
        view.map.height,
        view.visible_tiles.len(),
        view.map.width * view.map.height
    ));

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
            let glyph = if let Some(rb) = buildings_by_pos.get(&pos) {
                // HQs are remembered through fog. Distinguish "live sighting" (uppercase)
                // from "ghost / last known" (lowercase).
                let live = rb.currently_visible;
                let mine = rb.building.owner == me;
                match (mine, live) {
                    (true, _) => 'H', // your own HQ is always current
                    (false, true) => 'X',
                    (false, false) => 'x',
                }
            } else if !visible.contains(&pos) {
                '?'
            } else if let Some(u) = mine_pos.get(&pos) {
                if u.has_moved { 'u' } else { 'U' }
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
    s.push_str("\nLegend: U=your unit (can act), u=acted, E=enemy unit, H=your HQ, X=enemy HQ (visible), x=enemy HQ (last seen / ghost), .=plains, F=forest, M=mountain, ~=sea, ?=fogged.\n");

    let mut mine: Vec<&Unit> = view.units.iter().filter(|u| u.owner == me).collect();
    let mut theirs: Vec<&Unit> = view.units.iter().filter(|u| u.owner != me).collect();
    mine.sort_by_key(|u| (u.pos.1, u.pos.0));
    theirs.sort_by_key(|u| (u.pos.1, u.pos.0));

    s.push_str(&format!("\nYour units ({}):\n", mine.len()));
    for u in &mine {
        let terrain = view.map.terrain(u.pos).map(terrain_name).unwrap_or("?");
        s.push_str(&format!(
            "  {} {} hp={}/{} pos=[{},{}] on {} {}\n",
            u.id,
            unit_kind_name(u.kind),
            u.hp,
            UnitKind::max_hp(u.kind),
            u.pos.0,
            u.pos.1,
            terrain,
            if u.has_moved { "(acted)" } else { "(can act)" },
        ));
    }
    s.push_str(&format!("\nVisible enemy units ({}):\n", theirs.len()));
    for u in &theirs {
        let terrain = view.map.terrain(u.pos).map(terrain_name).unwrap_or("?");
        s.push_str(&format!(
            "  {} {} hp={}/{} pos=[{},{}] on {}\n",
            u.id,
            unit_kind_name(u.kind),
            u.hp,
            UnitKind::max_hp(u.kind),
            u.pos.0,
            u.pos.1,
            terrain,
        ));
    }

    // Buildings — own HQs are always current; enemy HQs may be ghosts.
    let mine_hq: Vec<_> = view
        .buildings
        .iter()
        .filter(|rb| rb.building.owner == me)
        .collect();
    let enemy_hq: Vec<_> = view
        .buildings
        .iter()
        .filter(|rb| rb.building.owner != me)
        .collect();
    if !mine_hq.is_empty() {
        s.push_str("\nYour buildings:\n");
        for rb in &mine_hq {
            s.push_str(&format!(
                "  HQ hp={}/{} at [{},{}]\n",
                rb.building.hp,
                rb.building.kind.max_hp(),
                rb.building.pos.0,
                rb.building.pos.1
            ));
        }
    }
    if !enemy_hq.is_empty() {
        s.push_str("\nKnown enemy buildings:\n");
        for rb in &enemy_hq {
            let tag = if rb.currently_visible {
                "live"
            } else {
                "ghost / last seen"
            };
            s.push_str(&format!(
                "  HQ hp={}/{} at [{},{}] ({}, last seen turn {})\n",
                rb.building.hp,
                rb.building.kind.max_hp(),
                rb.building.pos.0,
                rb.building.pos.1,
                tag,
                rb.last_seen_turn,
            ));
        }
    }
    s
}

fn unit_kind_name(k: UnitKind) -> &'static str {
    match k {
        UnitKind::Infantry => "infantry",
    }
}

fn format_action_report(
    prev: &PlayerView,
    new: &PlayerView,
    unit_id: Uuid,
    to: Coord,
    target: Option<Coord>,
    _me: PlayerId,
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
        // Locate the defender via their previous-state position.
        let prev_target = prev.units.iter().find(|u| u.pos == target_pos).cloned();
        if let Some(prev_t) = prev_target {
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

    if let Some(w) = new.winner {
        s.push_str(&format!("\nGAME OVER — winner: {:?}.\n", w));
    }
    s
}
