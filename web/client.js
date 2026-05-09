// agent-wars browser client
//
// Connects to ws://<host>/ws, sends a Join with the chosen view, then renders
// every State it receives. For player views, fog-of-war tiles are drawn dim
// and only visible-or-friendly units are rendered. Spectator sees everything.

const TILE = 48;

const TERRAIN_COLORS = {
  plains:   "#4f7a3a",
  forest:   "#2d4a23",
  mountain: "#7a6b50",
  sea:      "#1e4a78",
};
const PLAYER_COLORS = { p1: "#dd4455", p2: "#3388cc" };

const els = {
  role: document.getElementById("role"),
  connect: document.getElementById("connect"),
  endTurn: document.getElementById("endTurn"),
  status: document.getElementById("status"),
  turnNumber: document.getElementById("turnNumber"),
  currentTurn: document.getElementById("currentTurn"),
  winner: document.getElementById("winner"),
  canvas: document.getElementById("board"),
};
const ctx = els.canvas.getContext("2d");

let ws = null;
let view = null;       // {type: "player", value: "p1"|"p2"} | "spectator"
let state = null;      // last PlayerView received
let selected = null;   // currently selected unit
let reachable = null;  // Map<"x,y", true> for currently selected unit, computed locally

els.connect.addEventListener("click", () => {
  if (ws) ws.close();
  const role = els.role.value;
  view = role === "spectator"
    ? "spectator"
    : { type: "player", value: role === "player1" ? "p1" : "p2" };
  connect(view);
});
els.endTurn.addEventListener("click", () => {
  send({ type: "endTurn" });
});

els.canvas.addEventListener("click", (e) => {
  if (!state) return;
  const rect = els.canvas.getBoundingClientRect();
  const x = Math.floor((e.clientX - rect.left) / TILE);
  const y = Math.floor((e.clientY - rect.top) / TILE);
  if (x < 0 || y < 0 || x >= state.map.width || y >= state.map.height) return;

  const me = state.you;
  const isMyTurn = me && state.currentTurn === me;

  if (selected) {
    if (reachable && reachable.has(`${x},${y}`)) {
      send({ type: "move", unitId: selected.id, to: [x, y] });
    }
    selected = null;
    reachable = null;
    render();
    return;
  }

  if (!isMyTurn) return;
  const u = unitAt(x, y);
  if (u && u.owner === me && !u.hasMoved) {
    selected = u;
    reachable = computeReachable(u);
  }
  render();
});

function connect(viewValue) {
  const url = `ws://${location.host}/ws`;
  setStatus("connecting…");
  ws = new WebSocket(url);
  ws.addEventListener("open", () => {
    setStatus("connected", "connected");
    send({ type: "join", view: serializeView(viewValue) });
  });
  ws.addEventListener("close", () => setStatus("disconnected"));
  ws.addEventListener("error", () => setStatus("error", "error"));
  ws.addEventListener("message", (ev) => {
    let msg;
    try { msg = JSON.parse(ev.data); } catch { return; }
    if (msg.type === "state") {
      state = msg;
      // If a previously selected unit no longer exists or has moved, clear selection.
      if (selected) {
        const fresh = state.units.find((u) => u.id === selected.id);
        if (!fresh || fresh.hasMoved) {
          selected = null;
          reachable = null;
        } else {
          selected = fresh;
          reachable = computeReachable(fresh);
        }
      }
      updateHud();
      render();
    } else if (msg.type === "joined") {
      // ack
    } else if (msg.type === "error") {
      console.warn("server error:", msg.message);
    }
  });
}

function serializeView(v) {
  if (v === "spectator") return "spectator";
  // Player(p) is serialized as {"player": "p1"} by serde tagged enums.
  return { player: v.value };
}

function send(msg) {
  if (!ws || ws.readyState !== 1) return;
  ws.send(JSON.stringify(msg));
}

function setStatus(text, cls = "") {
  els.status.textContent = text;
  els.status.className = "status " + cls;
}

function updateHud() {
  els.turnNumber.textContent = state.turnNumber;
  els.currentTurn.textContent = labelPlayer(state.currentTurn);
  if (state.winner) {
    els.winner.textContent = `${labelPlayer(state.winner)} wins`;
  } else {
    els.winner.textContent = "";
  }
  const isMyTurn = state.you && state.currentTurn === state.you && !state.winner;
  els.endTurn.disabled = !isMyTurn;
}

function labelPlayer(p) {
  return p === "p1" ? "Player 1" : p === "p2" ? "Player 2" : p;
}

function unitAt(x, y) {
  return state.units.find((u) => u.pos[0] === x && u.pos[1] === y);
}

// Compute reachable tiles for a unit using the same rules as the server.
// (Client-side only for the highlight; the server validates the actual move.)
function computeReachable(unit) {
  const map = state.map;
  const mp = 3; // infantry move points
  const cost = {
    plains: 1, forest: 1, mountain: 2, sea: null,
  };
  const enemySet = new Set();
  for (const u of state.units) {
    if (u.owner !== unit.owner) enemySet.add(`${u.pos[0]},${u.pos[1]}`);
  }
  const friendSet = new Set();
  for (const u of state.units) {
    if (u.owner === unit.owner && u.id !== unit.id) friendSet.add(`${u.pos[0]},${u.pos[1]}`);
  }

  const best = new Map();
  best.set(`${unit.pos[0]},${unit.pos[1]}`, 0);
  const heap = [[0, unit.pos[0], unit.pos[1]]];
  while (heap.length) {
    heap.sort((a, b) => a[0] - b[0]);
    const [c, x, y] = heap.shift();
    if (c > (best.get(`${x},${y}`) ?? Infinity)) continue;
    for (const [nx, ny] of [[x+1,y],[x-1,y],[x,y+1],[x,y-1]]) {
      if (nx < 0 || ny < 0 || nx >= map.width || ny >= map.height) continue;
      const t = map.tiles[ny * map.width + nx];
      const step = cost[t];
      if (step == null) continue;
      if (enemySet.has(`${nx},${ny}`)) continue;
      const nc = c + step;
      if (nc > mp) continue;
      const key = `${nx},${ny}`;
      if (nc < (best.get(key) ?? Infinity)) {
        best.set(key, nc);
        heap.push([nc, nx, ny]);
      }
    }
  }
  // Can't stop on a tile occupied by friendly unit.
  for (const k of friendSet) best.delete(k);
  // Can stay put.
  best.set(`${unit.pos[0]},${unit.pos[1]}`, 0);
  return new Set(best.keys());
}

function render() {
  if (!state) {
    ctx.fillStyle = "#0d0f12";
    ctx.fillRect(0, 0, els.canvas.width, els.canvas.height);
    return;
  }
  const { map } = state;
  els.canvas.width = map.width * TILE;
  els.canvas.height = map.height * TILE;

  const visible = new Set(state.visibleTiles.map(([x, y]) => `${x},${y}`));

  // Tiles
  for (let y = 0; y < map.height; y++) {
    for (let x = 0; x < map.width; x++) {
      const t = map.tiles[y * map.width + x];
      ctx.fillStyle = TERRAIN_COLORS[t] || "#222";
      ctx.fillRect(x * TILE, y * TILE, TILE, TILE);
      // Grid line.
      ctx.strokeStyle = "#0006";
      ctx.lineWidth = 1;
      ctx.strokeRect(x * TILE + 0.5, y * TILE + 0.5, TILE - 1, TILE - 1);
    }
  }

  // Reachable highlight under units.
  if (reachable) {
    ctx.fillStyle = "rgba(255, 230, 120, 0.28)";
    for (const k of reachable) {
      const [x, y] = k.split(",").map(Number);
      ctx.fillRect(x * TILE, y * TILE, TILE, TILE);
    }
  }

  // Units
  for (const u of state.units) {
    drawUnit(u, u.id === selected?.id);
  }

  // Fog overlay (player view only).
  if (state.you) {
    ctx.fillStyle = "rgba(0, 0, 0, 0.55)";
    for (let y = 0; y < map.height; y++) {
      for (let x = 0; x < map.width; x++) {
        if (!visible.has(`${x},${y}`)) {
          ctx.fillRect(x * TILE, y * TILE, TILE, TILE);
        }
      }
    }
  }
}

function drawUnit(u, isSelected) {
  const [x, y] = u.pos;
  const cx = x * TILE + TILE / 2;
  const cy = y * TILE + TILE / 2;
  const r = TILE * 0.32;

  ctx.beginPath();
  ctx.arc(cx, cy, r, 0, Math.PI * 2);
  ctx.fillStyle = PLAYER_COLORS[u.owner] || "#aaa";
  ctx.fill();
  ctx.lineWidth = 2;
  ctx.strokeStyle = u.hasMoved ? "#0008" : "#fff";
  ctx.stroke();

  // HP badge if not full.
  if (u.hp < 10) {
    ctx.fillStyle = "#000a";
    ctx.fillRect(cx + r - 8, cy + r - 10, 14, 12);
    ctx.fillStyle = "#fff";
    ctx.font = "bold 10px monospace";
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    ctx.fillText(String(u.hp), cx + r - 1, cy + r - 4);
  }

  if (isSelected) {
    ctx.beginPath();
    ctx.arc(cx, cy, r + 4, 0, Math.PI * 2);
    ctx.strokeStyle = "#ffd84a";
    ctx.lineWidth = 2;
    ctx.stroke();
  }
}

render();
