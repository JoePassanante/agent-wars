// agent-wars browser client
//
// Connects to ws://<host>/ws, picks a vantage, renders fog-filtered state.
//
// Click flow:
//   1. Click a friendly infantry that hasn't acted -> highlight reachable tiles.
//   2. Click a reachable tile -> if any adjacent enemies, enter "pick attack
//      target"; otherwise send the move immediately.
//   3. In attack mode, click an adjacent enemy to attack, or click the
//      pending destination tile again to wait (move only). Click anywhere
//      else to cancel.

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
  reset: document.getElementById("reset"),
  status: document.getElementById("status"),
  turnNumber: document.getElementById("turnNumber"),
  currentTurn: document.getElementById("currentTurn"),
  winner: document.getElementById("winner"),
  canvas: document.getElementById("board"),
};
const ctx = els.canvas.getContext("2d");

let ws = null;
let view = null;
let state = null;       // last PlayerView received
let selected = null;    // selected unit (object)
let reachable = null;   // Set<"x,y"> while picking destination
let pendingMove = null; // [x, y] of provisional destination
let attackTargets = null; // Set<"x,y"> of enemy units we could attack from pendingMove
let lastError = "";

els.connect.addEventListener("click", () => {
  if (ws) ws.close();
  const role = els.role.value;
  view = role === "spectator"
    ? "spectator"
    : { type: "player", value: role === "player1" ? "p1" : "p2" };
  connect(view);
});
els.endTurn.addEventListener("click", () => send({ type: "endTurn" }));
els.reset.addEventListener("click", () => {
  if (confirm("Reset the lobby? Everyone connected will see a fresh match.")) {
    send({ type: "reset" });
  }
});

els.canvas.addEventListener("contextmenu", (e) => {
  // Right-click cancels current selection.
  e.preventDefault();
  clearSelection();
  render();
});

els.canvas.addEventListener("click", (e) => {
  if (!state) return;
  const rect = els.canvas.getBoundingClientRect();
  const x = Math.floor((e.clientX - rect.left) / TILE);
  const y = Math.floor((e.clientY - rect.top) / TILE);
  if (x < 0 || y < 0 || x >= state.map.width || y >= state.map.height) return;

  const me = state.you;
  const isMyTurn = me && state.currentTurn === me && !state.winner;

  // Phase 3: provisional destination chosen — pick attack target or wait.
  if (pendingMove) {
    const key = `${x},${y}`;
    if (attackTargets && attackTargets.has(key)) {
      send({ type: "move", unitId: selected.id, to: pendingMove, attack: [x, y] });
      clearSelection(); render(); return;
    }
    if (pendingMove[0] === x && pendingMove[1] === y) {
      send({ type: "move", unitId: selected.id, to: pendingMove });
      clearSelection(); render(); return;
    }
    clearSelection(); render(); return;
  }

  // Phase 2: a unit is selected.
  if (selected) {
    const clicked = unitAt(x, y);

    // Click an enemy that we can attack from somewhere reachable: auto-resolve.
    if (clicked && clicked.owner !== me) {
      const standTile = findAttackPosition(selected, [x, y]);
      if (standTile) {
        send({ type: "move", unitId: selected.id, to: standTile, attack: [x, y] });
        clearSelection(); render(); return;
      }
      // Out of range — fall through to cancel.
    }

    // Click a different friendly unit: switch selection.
    if (clicked && clicked.owner === me && clicked.id !== selected.id && !clicked.hasMoved) {
      selected = clicked;
      reachable = computeReachable(clicked);
      render(); return;
    }

    // Click a reachable empty tile: move (or enter attack-pick if enemies adjacent).
    if (reachable && reachable.has(`${x},${y}`) && !clicked) {
      const targets = enemiesAdjacentTo([x, y]);
      if (targets.size > 0) {
        pendingMove = [x, y];
        attackTargets = targets;
      } else {
        send({ type: "move", unitId: selected.id, to: [x, y] });
        clearSelection();
      }
      render(); return;
    }

    // Otherwise: clicked nothing useful — cancel selection.
    clearSelection(); render(); return;
  }

  // Phase 1: nothing selected.
  if (!isMyTurn) return;
  const u = unitAt(x, y);
  if (u && u.owner === me && !u.hasMoved) {
    selected = u;
    reachable = computeReachable(u);
  }
  render();
});

function clearSelection() {
  selected = null;
  reachable = null;
  pendingMove = null;
  attackTargets = null;
}

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
      // If our selected unit is gone or already acted, drop the selection.
      if (selected) {
        const fresh = state.units.find((u) => u.id === selected.id);
        if (!fresh || fresh.hasMoved) {
          clearSelection();
        } else {
          selected = fresh;
          reachable = computeReachable(fresh);
        }
      }
      lastError = "";
      updateHud();
      render();
    } else if (msg.type === "joined") {
      // ack
    } else if (msg.type === "error") {
      lastError = msg.message;
      console.warn("server error:", msg.message);
      updateHud();
    }
  });
}

function serializeView(v) {
  if (v === "spectator") return "spectator";
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
    els.winner.textContent = `${labelPlayer(state.winner)} wins!`;
  } else if (lastError) {
    els.winner.textContent = `⚠ ${lastError}`;
  } else {
    els.winner.textContent = "";
  }
  const isMyTurn = state.you && state.currentTurn === state.you && !state.winner;
  els.endTurn.disabled = !isMyTurn;
  els.reset.disabled = !ws || ws.readyState !== 1;
}

function labelPlayer(p) {
  return p === "p1" ? "Player 1" : p === "p2" ? "Player 2" : p;
}

function unitAt(x, y) {
  return state.units.find((u) => u.pos[0] === x && u.pos[1] === y);
}

function enemiesAdjacentTo([x, y]) {
  if (!state.you) return new Set();
  const out = new Set();
  for (const u of state.units) {
    if (u.owner === state.you) continue;
    const d = Math.abs(u.pos[0] - x) + Math.abs(u.pos[1] - y);
    if (d >= 1 && d <= 1) out.add(`${u.pos[0]},${u.pos[1]}`);
  }
  return out;
}

// Find a reachable tile from which `unit` can attack `enemyPos` (melee Manhattan-1).
// Prefers staying put if already adjacent; otherwise the cheapest reachable adjacent.
function findAttackPosition(unit, enemyPos) {
  const r = reachable || computeReachable(unit);
  const cur = `${unit.pos[0]},${unit.pos[1]}`;
  const adj = [
    [enemyPos[0] + 1, enemyPos[1]],
    [enemyPos[0] - 1, enemyPos[1]],
    [enemyPos[0], enemyPos[1] + 1],
    [enemyPos[0], enemyPos[1] - 1],
  ];
  if (adj.some(([ax, ay]) => `${ax},${ay}` === cur)) {
    return [unit.pos[0], unit.pos[1]];
  }
  for (const [ax, ay] of adj) {
    if (r.has(`${ax},${ay}`)) return [ax, ay];
  }
  return null;
}

// Enemies the selected unit could hit this turn from any reachable tile.
function attackableEnemiesForSelected() {
  if (!selected || !reachable) return new Set();
  const out = new Set();
  for (const u of state.units) {
    if (u.owner === state.you) continue;
    for (const k of reachable) {
      const [rx, ry] = k.split(",").map(Number);
      const d = Math.abs(rx - u.pos[0]) + Math.abs(ry - u.pos[1]);
      if (d === 1) {
        out.add(`${u.pos[0]},${u.pos[1]}`);
        break;
      }
    }
  }
  return out;
}

function computeReachable(unit) {
  const map = state.map;
  const mp = 3; // infantry move points
  const cost = { plains: 1, forest: 1, mountain: 2, sea: null };
  const enemySet = new Set();
  const friendSet = new Set();
  for (const u of state.units) {
    const k = `${u.pos[0]},${u.pos[1]}`;
    if (u.owner !== unit.owner) enemySet.add(k);
    else if (u.id !== unit.id) friendSet.add(k);
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
  for (const k of friendSet) best.delete(k);
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

  // Tiles + grid.
  for (let y = 0; y < map.height; y++) {
    for (let x = 0; x < map.width; x++) {
      const t = map.tiles[y * map.width + x];
      ctx.fillStyle = TERRAIN_COLORS[t] || "#222";
      ctx.fillRect(x * TILE, y * TILE, TILE, TILE);
      ctx.strokeStyle = "#0006";
      ctx.lineWidth = 1;
      ctx.strokeRect(x * TILE + 0.5, y * TILE + 0.5, TILE - 1, TILE - 1);
    }
  }

  // Reachable highlight (only when no pending move).
  if (reachable && !pendingMove) {
    ctx.fillStyle = "rgba(255, 230, 120, 0.28)";
    for (const k of reachable) {
      const [x, y] = k.split(",").map(Number);
      ctx.fillRect(x * TILE, y * TILE, TILE, TILE);
    }
  }

  // Pending move tile.
  if (pendingMove) {
    const [x, y] = pendingMove;
    ctx.fillStyle = "rgba(255, 230, 120, 0.45)";
    ctx.fillRect(x * TILE, y * TILE, TILE, TILE);
    ctx.strokeStyle = "#ffd84a";
    ctx.lineWidth = 2;
    ctx.strokeRect(x * TILE + 1, y * TILE + 1, TILE - 2, TILE - 2);
  }

  // Attack target rings: explicit attackTargets in attack-pick mode, OR
  // pre-show all attackable enemies whenever a unit is selected.
  const targetsToRing = attackTargets ?? attackableEnemiesForSelected();
  if (targetsToRing.size > 0) {
    ctx.strokeStyle = "#ff5050";
    ctx.lineWidth = 3;
    for (const k of targetsToRing) {
      const [x, y] = k.split(",").map(Number);
      const cx = x * TILE + TILE / 2;
      const cy = y * TILE + TILE / 2;
      ctx.beginPath();
      ctx.arc(cx, cy, TILE * 0.42, 0, Math.PI * 2);
      ctx.stroke();
    }
  }

  // Units.
  for (const u of state.units) drawUnit(u, u.id === selected?.id);

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

  if (u.hp < 10) {
    const w = 16, h = 12;
    ctx.fillStyle = "#000a";
    ctx.fillRect(cx + r - w * 0.5, cy + r - h * 0.4, w, h);
    ctx.fillStyle = "#fff";
    ctx.font = "bold 10px monospace";
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    ctx.fillText(String(u.hp), cx + r, cy + r + 1);
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
