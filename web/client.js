// agent-wars browser client
//
// Connects to ws://<host>/ws, picks a vantage, renders the fog-filtered state,
// and animates units along their pathfinding path when they move.

const TILE = 48;
const STEP_MS = 140; // milliseconds per tile during animation

const TERRAIN_COLORS = {
  plains:   "#4f7a3a",
  forest:   "#2d4a23",
  mountain: "#7a6b50",
  sea:      "#1e4a78",
};
const PLAYER_COLORS = { p1: "#dd4455", p2: "#3388cc" };
const NEUTRAL_COLOR = "#888";

const UNIT_COSTS = { scout: 1000, infantry: 2000, heavy_infantry: 3000 };
const UNIT_KIND_LABELS = {
  infantry: "Infantry",
  scout: "Scout",
  heavy_infantry: "Heavy Infantry",
};

const els = {
  role: document.getElementById("role"),
  connect: document.getElementById("connect"),
  endTurn: document.getElementById("endTurn"),
  surrender: document.getElementById("surrender"),
  reset: document.getElementById("reset"),
  status: document.getElementById("status"),
  turnNumber: document.getElementById("turnNumber"),
  currentTurn: document.getElementById("currentTurn"),
  funds: document.getElementById("funds"),
  mapSeed: document.getElementById("mapSeed"),
  winner: document.getElementById("winner"),
  canvas: document.getElementById("board"),
  buyPanel: document.getElementById("buyPanel"),
};
const ctx = els.canvas.getContext("2d");

let ws = null;
let view = null;
let state = null;
let selected = null;
let reachable = null;
let pendingMove = null;
let attackTargets = null;
let lastError = "";

// Active move animation: { unitId, path: [[x,y],...], started: ms }
let animation = null;

els.connect.addEventListener("click", () => {
  if (ws) ws.close();
  const role = els.role.value;
  view = role === "spectator"
    ? "spectator"
    : { type: "player", value: role === "player1" ? "p1" : "p2" };
  connect(view);
});
els.endTurn.addEventListener("click", () => send({ type: "endTurn" }));
els.surrender.addEventListener("click", () => {
  if (confirm("Surrender? The other player will win immediately.")) {
    send({ type: "surrender" });
  }
});
els.reset.addEventListener("click", () => {
  const input = prompt(
    "Reset the lobby. Leave blank for a random map, or enter a seed (u64) to reproduce a specific map.",
    "",
  );
  if (input === null) return;
  const trimmed = input.trim();
  if (trimmed === "") {
    send({ type: "reset" });
  } else {
    const seed = Number(trimmed);
    if (!Number.isFinite(seed) || seed < 0) {
      alert("Seed must be a non-negative integer.");
      return;
    }
    send({ type: "reset", seed });
  }
});

els.canvas.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  clearSelection();
  render();
});

els.canvas.addEventListener("click", (e) => {
  if (!state || animation) return;
  const rect = els.canvas.getBoundingClientRect();
  const x = Math.floor((e.clientX - rect.left) / TILE);
  const y = Math.floor((e.clientY - rect.top) / TILE);
  if (x < 0 || y < 0 || x >= state.map.width || y >= state.map.height) return;

  const me = state.you;
  const isMyTurn = me && state.currentTurn === me && !state.winner;

  // Attack-pick mode (provisional move chosen).
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

  // Unit-selected mode.
  if (selected) {
    const enemyTargetUnit = unitAt(x, y);
    const enemyTargetBuilding = buildingAt(x, y);

    // Click an enemy unit OR enemy HQ within reach -> auto-resolve.
    // Cities and factories are captured by ending turn on them, not attacked.
    const enemyTarget =
      (enemyTargetUnit && enemyTargetUnit.owner !== me) ? enemyTargetUnit :
      (enemyTargetBuilding && enemyTargetBuilding.kind === "hq" && enemyTargetBuilding.owner !== me && enemyTargetBuilding.currentlyVisible) ? enemyTargetBuilding : null;
    if (enemyTarget) {
      const standTile = findAttackPosition(selected, [x, y]);
      if (standTile) {
        send({ type: "move", unitId: selected.id, to: standTile, attack: [x, y] });
        clearSelection(); render(); return;
      }
    }

    // Click another own unit -> switch.
    if (enemyTargetUnit && enemyTargetUnit.owner === me && enemyTargetUnit.id !== selected.id && !enemyTargetUnit.hasMoved) {
      selected = enemyTargetUnit;
      reachable = computeReachable(selected);
      render(); return;
    }

    // Click a reachable destination tile -> move (or enter attack-pick).
    // Factories/cities are passable destinations (you stand on them to capture
    // or to spawn from); HQs block.
    const dstBlocked = enemyTargetBuilding && enemyTargetBuilding.kind === "hq";
    if (reachable && reachable.has(`${x},${y}`) && !enemyTargetUnit && !dstBlocked) {
      const targets = adjacentEnemies([x, y]);
      if (targets.size > 0) {
        pendingMove = [x, y];
        attackTargets = targets;
      } else {
        send({ type: "move", unitId: selected.id, to: [x, y] });
        clearSelection();
      }
      render(); return;
    }

    clearSelection(); render(); return;
  }

  // Idle: clicking a friendly unit selects it.
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
      handleState(msg);
    } else if (msg.type === "joined") {
      // ack
    } else if (msg.type === "error") {
      lastError = msg.message;
      console.warn("server error:", msg.message);
      updateHud();
    }
  });
}

function handleState(newState) {
  // Start an animation if the action included a meaningful path.
  if (
    newState.lastAction &&
    newState.lastAction.path &&
    newState.lastAction.path.length > 1
  ) {
    animation = {
      unitId: newState.lastAction.unitId,
      path: newState.lastAction.path,
      started: performance.now(),
    };
    requestAnimationFrame(tick);
  }

  state = newState;
  if (selected) {
    const fresh = state.units.find((u) => u.id === selected.id);
    if (!fresh || fresh.hasMoved) clearSelection();
    else { selected = fresh; reachable = computeReachable(fresh); }
  }
  lastError = "";
  updateHud();
  render();
}

function tick(now) {
  if (!animation) return;
  const elapsed = now - animation.started;
  const totalMs = STEP_MS * (animation.path.length - 1);
  if (elapsed >= totalMs) {
    animation = null;
    render();
    return;
  }
  render();
  requestAnimationFrame(tick);
}

function animatedPosition() {
  if (!animation) return null;
  const elapsed = performance.now() - animation.started;
  const idx = Math.min(
    animation.path.length - 1,
    Math.floor(elapsed / STEP_MS),
  );
  const t = (elapsed - idx * STEP_MS) / STEP_MS;
  const a = animation.path[idx];
  const b = animation.path[Math.min(animation.path.length - 1, idx + 1)];
  return [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
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
  const myFunds = state.you ? (state.funds?.[state.you] ?? 0) : "—";
  els.funds.textContent = myFunds === "—" ? "—" : `${myFunds}g`;
  els.mapSeed.textContent = state.mapSeed ?? "–";
  if (state.winner) {
    const outcome = state.you === state.winner ? "you win!" : state.you ? "you lose." : "match over.";
    els.winner.textContent = `${labelPlayer(state.winner)} wins — ${outcome}`;
  } else if (lastError) {
    els.winner.textContent = `⚠ ${lastError}`;
  } else {
    els.winner.textContent = "";
  }
  const isMyTurn = state.you && state.currentTurn === state.you && !state.winner;
  els.endTurn.disabled = !isMyTurn;
  els.surrender.disabled = !state.you || state.winner || state.turnNumber < 4;
  els.reset.disabled = !ws || ws.readyState !== 1;
  renderBuyPanel(isMyTurn);
}

function renderBuyPanel(isMyTurn) {
  if (!state || !state.you) {
    els.buyPanel.innerHTML = '<p class="hint">Connect as a player to buy units.</p>';
    return;
  }
  const myFactories = (state.buildings || []).filter(
    (rb) => rb.kind === "factory" && rb.owner === state.you,
  );
  if (myFactories.length === 0) {
    els.buyPanel.innerHTML = '<p class="hint">You don\'t own any factories.</p>';
    return;
  }
  const used = new Set(state.factoriesUsed || []);
  const myFunds = state.funds?.[state.you] ?? 0;

  const rows = myFactories.map((f) => {
    const isUsed = used.has(f.id);
    const occupied = (state.units || []).some(
      (u) => u.pos[0] === f.pos[0] && u.pos[1] === f.pos[1],
    );
    let status = "";
    if (!isMyTurn) status = " (not your turn)";
    else if (isUsed) status = " (already produced)";
    else if (occupied) status = " (tile occupied)";

    const buttons = Object.entries(UNIT_COSTS)
      .map(([kind, cost]) => {
        const disabled = !isMyTurn || isUsed || occupied || myFunds < cost;
        return `<button data-factory="${f.id}" data-kind="${kind}" ${disabled ? "disabled" : ""}>${UNIT_KIND_LABELS[kind]} (${cost}g)</button>`;
      })
      .join("");
    return `<div class="factory-row"><span class="factory-label">Factory at [${f.pos[0]},${f.pos[1]}]${status}</span>${buttons}</div>`;
  });
  els.buyPanel.innerHTML = rows.join("");

  // Bind buy buttons.
  els.buyPanel.querySelectorAll("button[data-factory]").forEach((btn) => {
    btn.addEventListener("click", () => {
      send({
        type: "buyUnit",
        factoryId: btn.dataset.factory,
        kind: btn.dataset.kind,
      });
    });
  });
}

function labelPlayer(p) {
  return p === "p1" ? "Player 1" : p === "p2" ? "Player 2" : p;
}

function unitAt(x, y) { return state.units.find((u) => u.pos[0] === x && u.pos[1] === y); }
function buildingAt(x, y) {
  return (state.buildings || []).find((b) => b.pos[0] === x && b.pos[1] === y);
}

function adjacentEnemies([x, y]) {
  const out = new Set();
  if (!state.you) return out;
  for (const u of state.units) {
    if (u.owner === state.you) continue;
    const d = Math.abs(u.pos[0] - x) + Math.abs(u.pos[1] - y);
    if (d === 1) out.add(`${u.pos[0]},${u.pos[1]}`);
  }
  // Only enemy HQs are attackable; factories/cities are captured by standing on them.
  for (const b of state.buildings || []) {
    if (b.kind !== "hq") continue;
    if (b.owner === state.you) continue;
    if (!b.currentlyVisible) continue;
    const d = Math.abs(b.pos[0] - x) + Math.abs(b.pos[1] - y);
    if (d === 1) out.add(`${b.pos[0]},${b.pos[1]}`);
  }
  return out;
}

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

function attackableEnemiesForSelected() {
  if (!selected || !reachable) return new Set();
  const out = new Set();
  const candidates = [
    ...state.units
      .filter((u) => u.owner !== state.you)
      .map((u) => ({ pos: u.pos })),
    ...(state.buildings || [])
      .filter((b) => b.kind === "hq" && b.owner !== state.you && b.currentlyVisible)
      .map((b) => ({ pos: b.pos })),
  ];
  for (const c of candidates) {
    for (const k of reachable) {
      const [rx, ry] = k.split(",").map(Number);
      const d = Math.abs(rx - c.pos[0]) + Math.abs(ry - c.pos[1]);
      if (d === 1) {
        out.add(`${c.pos[0]},${c.pos[1]}`);
        break;
      }
    }
  }
  return out;
}

function computeReachable(unit) {
  const map = state.map;
  const mp = 3;
  const cost = { plains: 1, forest: 1, mountain: 2, sea: null };
  const blocked = new Set();
  const friendSet = new Set();
  for (const u of state.units) {
    const k = `${u.pos[0]},${u.pos[1]}`;
    if (u.owner !== unit.owner) blocked.add(k);
    else if (u.id !== unit.id) friendSet.add(k);
  }
  // Only HQs block movement; factories and cities are passable so units
  // can stand on them to capture or to spawn from.
  for (const b of state.buildings || []) {
    if (b.kind === "hq") blocked.add(`${b.pos[0]},${b.pos[1]}`);
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
      if (blocked.has(`${nx},${ny}`)) continue;
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

  if (reachable && !pendingMove) {
    ctx.fillStyle = "rgba(255, 230, 120, 0.28)";
    for (const k of reachable) {
      const [x, y] = k.split(",").map(Number);
      ctx.fillRect(x * TILE, y * TILE, TILE, TILE);
    }
  }

  if (pendingMove) {
    const [x, y] = pendingMove;
    ctx.fillStyle = "rgba(255, 230, 120, 0.45)";
    ctx.fillRect(x * TILE, y * TILE, TILE, TILE);
    ctx.strokeStyle = "#ffd84a";
    ctx.lineWidth = 2;
    ctx.strokeRect(x * TILE + 1, y * TILE + 1, TILE - 2, TILE - 2);
  }

  const targetsToRing = attackTargets ?? attackableEnemiesForSelected();
  if (targetsToRing.size > 0) {
    ctx.strokeStyle = "#ff5050";
    ctx.lineWidth = 3;
    for (const k of targetsToRing) {
      const [x, y] = k.split(",").map(Number);
      const cx = x * TILE + TILE / 2;
      const cy = y * TILE + TILE / 2;
      ctx.strokeRect(x * TILE + 4, y * TILE + 4, TILE - 8, TILE - 8);
      ctx.beginPath();
      ctx.arc(cx, cy, TILE * 0.42, 0, Math.PI * 2);
      ctx.stroke();
    }
  }

  // Currently-visible buildings (full opacity).
  for (const b of state.buildings || []) {
    if (!b.currentlyVisible) continue;
    drawBuilding(b, false);
  }

  // Units (with optional animation override for the moving unit).
  const animPos = animatedPosition();
  for (const u of state.units) {
    let renderPos = u.pos;
    if (animation && u.id === animation.unitId && animPos) {
      renderPos = animPos;
    }
    drawUnit(u, renderPos, u.id === selected?.id);
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

  // Ghost buildings on top of fog so the player still sees what they remember.
  for (const b of state.buildings || []) {
    if (b.currentlyVisible) continue;
    drawBuilding(b, true);
  }
}

function drawBuilding(b, ghost) {
  const [x, y] = b.pos;
  const cx = x * TILE + TILE / 2;
  const cy = y * TILE + TILE / 2;
  const px = x * TILE + 6;
  const py = y * TILE + 6;
  const size = TILE - 12;

  const ownerColor = b.owner ? (PLAYER_COLORS[b.owner] || "#aaa") : NEUTRAL_COLOR;

  ctx.save();
  if (ghost) ctx.globalAlpha = 0.5;
  ctx.fillStyle = ownerColor;
  ctx.lineWidth = 2;
  ctx.strokeStyle = ghost ? "#fff7" : "#fff";
  ctx.setLineDash(ghost ? [4, 3] : []);

  if (b.kind === "hq") {
    roundedRect(px, py, size, size, 4);
    ctx.fill();
    ctx.stroke();
    drawStar(cx, cy - 2, 5, TILE * 0.18, TILE * 0.08);
    drawHpBadge(x, y, b.hp, 10);
  } else if (b.kind === "factory") {
    // Squat industrial silhouette: trapezoidal base + small chimney.
    ctx.beginPath();
    ctx.moveTo(px, py + size);
    ctx.lineTo(px, py + size * 0.45);
    ctx.lineTo(px + size * 0.6, py + size * 0.2);
    ctx.lineTo(px + size, py + size * 0.45);
    ctx.lineTo(px + size, py + size);
    ctx.closePath();
    ctx.fill();
    ctx.stroke();
    // Chimney.
    ctx.fillStyle = ownerColor;
    ctx.fillRect(px + size * 0.65, py + size * 0.05, size * 0.15, size * 0.25);
    ctx.strokeRect(px + size * 0.65, py + size * 0.05, size * 0.15, size * 0.25);
  } else if (b.kind === "city") {
    // Stack of small windows.
    ctx.beginPath();
    ctx.arc(cx, cy, size * 0.45, 0, Math.PI * 2);
    ctx.fill();
    ctx.stroke();
    // Windows grid.
    ctx.setLineDash([]);
    ctx.fillStyle = "#0a0a0a";
    const w = size * 0.18;
    for (const dy of [-w * 1.1, w * 0.1]) {
      for (const dx of [-w * 0.7, w * 0.3]) {
        ctx.fillRect(cx + dx, cy + dy, w * 0.5, w * 0.6);
      }
    }
  }
  ctx.restore();
}

function drawHpBadge(x, y, hp, max) {
  ctx.fillStyle = "#000a";
  ctx.fillRect(x * TILE + TILE - 18, y * TILE + TILE - 14, 16, 12);
  ctx.fillStyle = "#fff";
  ctx.font = "bold 10px monospace";
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  ctx.fillText(`${hp}/${max}`, x * TILE + TILE - 10, y * TILE + TILE - 8);
}

function drawStar(cx, cy, points, outer, inner) {
  ctx.beginPath();
  for (let i = 0; i < points * 2; i++) {
    const r = i % 2 === 0 ? outer : inner;
    const a = (Math.PI / points) * i - Math.PI / 2;
    const px = cx + Math.cos(a) * r;
    const py = cy + Math.sin(a) * r;
    if (i === 0) ctx.moveTo(px, py); else ctx.lineTo(px, py);
  }
  ctx.closePath();
  ctx.fillStyle = "#fff";
  ctx.fill();
}

function roundedRect(x, y, w, h, r) {
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.lineTo(x + w - r, y);
  ctx.quadraticCurveTo(x + w, y, x + w, y + r);
  ctx.lineTo(x + w, y + h - r);
  ctx.quadraticCurveTo(x + w, y + h, x + w - r, y + h);
  ctx.lineTo(x + r, y + h);
  ctx.quadraticCurveTo(x, y + h, x, y + h - r);
  ctx.lineTo(x, y + r);
  ctx.quadraticCurveTo(x, y, x + r, y);
  ctx.closePath();
}

function drawUnit(u, pos, isSelected) {
  const cx = pos[0] * TILE + TILE / 2;
  const cy = pos[1] * TILE + TILE / 2;

  ctx.fillStyle = PLAYER_COLORS[u.owner] || "#aaa";
  ctx.lineWidth = 2;
  ctx.strokeStyle = u.hasMoved ? "#0008" : "#fff";

  // Each unit kind gets a distinct silhouette so you can read the board at a glance.
  let badgeR;
  if (u.kind === "scout") {
    // Forward-leaning chevron — small, fast.
    const r = TILE * 0.28;
    ctx.beginPath();
    ctx.moveTo(cx, cy - r);                // tip up
    ctx.lineTo(cx + r * 0.95, cy + r * 0.55);
    ctx.lineTo(cx, cy + r * 0.25);
    ctx.lineTo(cx - r * 0.95, cy + r * 0.55);
    ctx.closePath();
    ctx.fill();
    ctx.stroke();
    badgeR = r;
  } else if (u.kind === "heavy_infantry") {
    // Chunky rounded square — slow, tough.
    const r = TILE * 0.34;
    roundedRect(cx - r, cy - r, r * 2, r * 2, 6);
    ctx.fill();
    ctx.stroke();
    // Inner stripe for the "armor" feel.
    ctx.save();
    ctx.fillStyle = "#0006";
    ctx.fillRect(cx - r, cy - 2, r * 2, 4);
    ctx.restore();
    badgeR = r;
  } else {
    // Default: infantry circle.
    const r = TILE * 0.30;
    ctx.beginPath();
    ctx.arc(cx, cy, r, 0, Math.PI * 2);
    ctx.fill();
    ctx.stroke();
    badgeR = r;
  }

  // Single-letter label so the kind is unambiguous even without legend lookup.
  ctx.fillStyle = "#fff";
  ctx.font = "bold 12px system-ui, sans-serif";
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  const letter = u.kind === "scout" ? "S" : u.kind === "heavy_infantry" ? "H" : "I";
  ctx.fillText(letter, cx, cy + 1);

  if (u.hp < 10) {
    const w = 16, h = 12;
    ctx.fillStyle = "#000a";
    ctx.fillRect(cx + badgeR - w * 0.5, cy + badgeR - h * 0.4, w, h);
    ctx.fillStyle = "#fff";
    ctx.font = "bold 10px monospace";
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    ctx.fillText(String(u.hp), cx + badgeR, cy + badgeR + 1);
  }

  if (isSelected) {
    ctx.beginPath();
    ctx.arc(cx, cy, badgeR + 5, 0, Math.PI * 2);
    ctx.strokeStyle = "#ffd84a";
    ctx.lineWidth = 2;
    ctx.stroke();
  }
}

render();
