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
  endTurn: document.getElementById("endTurn"),
  surrender: document.getElementById("surrender"),
  leave: document.getElementById("leave"),
  status: document.getElementById("status"),
  sessionId: document.getElementById("sessionId"),
  turnNumber: document.getElementById("turnNumber"),
  currentTurn: document.getElementById("currentTurn"),
  funds: document.getElementById("funds"),
  mapSeed: document.getElementById("mapSeed"),
  winner: document.getElementById("winner"),
  canvas: document.getElementById("board"),
  buyPanel: document.getElementById("buyPanel"),
  // Lobby:
  lobby: document.getElementById("lobby"),
  boardArea: document.getElementById("board-area"),
  usernameInput: document.getElementById("usernameInput"),
  queueBtn: document.getElementById("queueBtn"),
  browseBtn: document.getElementById("browseBtn"),
  lobbyStatus: document.getElementById("lobbyStatus"),
};
const ctx = els.canvas.getContext("2d");

let ws = null;
let myView = null;       // "spectator" | { player: "p1"|"p2" }
let mySessionId = null;
let state = null;
let selected = null;
let reachable = null;
let pendingMove = null;
let attackTargets = null;
let lastError = "";

// Restore last-used username so reconnect-by-username is one click.
els.usernameInput.value = localStorage.getItem("agent-wars-username") || "";

// Active move animation: { unitId, path: [[x,y],...], started: ms }
let animation = null;

// ---------------- Lobby flow ----------------

function lobbyStatus(text, cls = "") {
  els.lobbyStatus.textContent = text;
  els.lobbyStatus.className = "lobby-status " + cls;
}

async function refreshSessions() {
  const grid = document.getElementById("sessionsGrid");
  if (!grid) return;
  try {
    const resp = await fetch("/api/sessions");
    const sessions = await resp.json();
    if (!sessions.length) {
      grid.innerHTML = '<p class="empty">No live games right now. Pick a username and Queue to start one.</p>';
      return;
    }
    grid.innerHTML = sessions
      .map(
        (s) => `
        <div class="session-card">
          <canvas data-preview="${s.id}" width="200" height="140"></canvas>
          <div class="meta">
            <span><code>${shortId(s.id)}</code> · ${s.mapWidth}×${s.mapHeight}</span>
            <span>turn ${s.turnNumber}</span>
          </div>
          <div class="meta">
            <span>${labelPlayer(s.currentTurn)}'s turn</span>
            <span>units P1 ${s.p1Units} · P2 ${s.p2Units}</span>
          </div>
          ${s.hasWinner ? `<div class="meta"><span class="winner">${labelPlayer(s.winner)} won</span></div>` : ""}
          <button data-watch="${s.id}">Watch live</button>
        </div>`,
      )
      .join("");
    // Render the per-card preview canvases.
    for (const s of sessions) {
      const c = grid.querySelector(`canvas[data-preview="${s.id}"]`);
      if (c) renderPreview(c, s);
    }
    grid.querySelectorAll("button[data-watch]").forEach((b) => {
      b.addEventListener("click", () => {
        const username = currentUsername();
        if (!username) return;
        watchSession(b.dataset.watch, username);
      });
    });
  } catch (e) {
    grid.innerHTML = `<p class="empty">Failed to load sessions: ${e}</p>`;
  }
}

// Render a small terrain + units snapshot onto a session card's canvas.
// Tiny tile size — drop the per-tile decorations and just paint the colors,
// then dot in units and building positions in owner colors.
function renderPreview(canvas, s) {
  const map = s.map;
  if (!map) return;
  const pctx = canvas.getContext("2d");
  const dpr = window.devicePixelRatio || 1;
  const cssW = canvas.clientWidth || 200;
  const cssH = canvas.clientHeight || 140;
  // Fit map into the card area while preserving aspect.
  const tile = Math.max(2, Math.floor(Math.min(cssW / map.width, cssH / map.height)));
  const w = tile * map.width;
  const h = tile * map.height;
  canvas.width = w * dpr;
  canvas.height = h * dpr;
  canvas.style.width = w + "px";
  canvas.style.height = h + "px";
  pctx.scale(dpr, dpr);

  // Background.
  pctx.fillStyle = "#0a0c0f";
  pctx.fillRect(0, 0, w, h);
  // Tiles (colors only, no decoration — too small).
  for (let y = 0; y < map.height; y++) {
    for (let x = 0; x < map.width; x++) {
      const t = map.tiles[y * map.width + x];
      pctx.fillStyle = TERRAIN_COLORS[t] || "#222";
      pctx.fillRect(x * tile, y * tile, tile, tile);
    }
  }
  // Buildings as squares in owner color.
  for (const b of s.buildings || []) {
    const ownerColor = b.owner ? PLAYER_COLORS[b.owner] : NEUTRAL_COLOR;
    pctx.fillStyle = ownerColor;
    const inset = Math.max(0.5, tile * 0.2);
    pctx.fillRect(
      b.pos[0] * tile + inset,
      b.pos[1] * tile + inset,
      tile - inset * 2,
      tile - inset * 2,
    );
    if (b.kind === "hq") {
      // White dot in the center for HQs so they stand out.
      pctx.fillStyle = "#fff";
      const dot = Math.max(1, Math.floor(tile / 4));
      pctx.fillRect(
        b.pos[0] * tile + tile / 2 - dot / 2,
        b.pos[1] * tile + tile / 2 - dot / 2,
        dot,
        dot,
      );
    }
  }
  // Units as filled dots.
  for (const u of s.units || []) {
    pctx.fillStyle = PLAYER_COLORS[u.owner] || "#fff";
    pctx.beginPath();
    pctx.arc(
      u.pos[0] * tile + tile / 2,
      u.pos[1] * tile + tile / 2,
      Math.max(1.5, tile * 0.32),
      0,
      Math.PI * 2,
    );
    pctx.fill();
  }
}

function shortId(id) { return id.slice(0, 8); }

function currentUsername() {
  const u = (els.usernameInput.value || "").trim();
  if (!/^[A-Za-z0-9_-]{1,32}$/.test(u)) {
    lobbyStatus("Username must be 1–32 chars, letters/numbers/_/-", "error");
    return null;
  }
  localStorage.setItem("agent-wars-username", u);
  return u;
}

els.queueBtn.addEventListener("click", () => {
  const username = currentUsername();
  if (!username) return;
  connect(username, { kind: "play" });
});

els.browseBtn.addEventListener("click", () => {
  refreshSessions();
});

els.endTurn.addEventListener("click", () => send({ type: "endTurn" }));
els.surrender.addEventListener("click", () => {
  if (confirm("Surrender? The other player will win immediately.")) {
    send({ type: "surrender" });
  }
});
els.leave.addEventListener("click", () => {
  if (!confirm("Leave the session? Player connections will get auto-reconnected on next Hello.")) return;
  send({ type: "leave" });
  if (ws) ws.close();
  showLobby();
  refreshSessions();
});

function watchSession(sessionId, username) {
  connect(username, { kind: "watch", sessionId });
}

function showLobby() {
  els.lobby.style.display = "";
  els.boardArea.hidden = true;
  state = null;
  myView = null;
  mySessionId = null;
  setStatus("disconnected");
  els.endTurn.disabled = true;
  els.surrender.disabled = true;
  els.leave.disabled = true;
  els.role.textContent = "";
}

function showGame() {
  els.lobby.style.display = "none";
  els.boardArea.hidden = false;
}

// Periodically refresh the session list while in lobby.
setInterval(() => {
  if (els.lobby.style.display !== "none" && !ws) refreshSessions();
}, 3000);
refreshSessions();

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

function connect(username, intent) {
  const url = `ws://${location.host}/ws`;
  setStatus("connecting…");
  lobbyStatus("Connecting…");
  ws = new WebSocket(url);
  ws.addEventListener("open", () => {
    setStatus("connected", "connected");
    ws.send(JSON.stringify({ type: "hello", username, intent }));
  });
  ws.addEventListener("close", () => {
    setStatus("disconnected");
    if (!state) {
      // We never made it into a session — hop back to the lobby.
      showLobby();
    }
  });
  ws.addEventListener("error", () => setStatus("error", "error"));
  ws.addEventListener("message", (ev) => {
    let msg;
    try { msg = JSON.parse(ev.data); } catch { return; }
    switch (msg.type) {
      case "hello":
        lobbyStatus(`connected to server v${msg.serverVersion}`);
        break;
      case "queued":
        lobbyStatus(`Queued — position ${msg.position}. Waiting for an opponent…`);
        break;
      case "matched":
        myView = { player: msg.role };
        mySessionId = msg.sessionId;
        lobbyStatus(`Matched as ${msg.role}!`);
        showGame();
        break;
      case "reconnected":
        myView = { player: msg.role };
        mySessionId = msg.sessionId;
        lobbyStatus(`Reconnected as ${msg.role}.`);
        showGame();
        break;
      case "spectating":
        myView = "spectator";
        mySessionId = msg.sessionId;
        lobbyStatus(`Spectating ${shortId(msg.sessionId)}.`);
        showGame();
        break;
      case "state":
        handleState(msg);
        break;
      case "error":
        lastError = msg.message;
        console.warn("server error:", msg.message);
        if (!state) lobbyStatus(`Error: ${msg.message}`, "error");
        updateHud();
        break;
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
  if (state.you) {
    els.funds.textContent = `${state.funds?.[state.you] ?? 0}g`;
  } else {
    // Spectator sees both players' funds in real time.
    const p1 = state.funds?.p1 ?? 0;
    const p2 = state.funds?.p2 ?? 0;
    els.funds.textContent = `P1 ${p1}g · P2 ${p2}g`;
  }
  els.mapSeed.textContent = state.mapSeed ?? "–";
  els.sessionId.textContent = state.sessionId ? state.sessionId.slice(0, 8) : "–";
  els.role.textContent = state.you
    ? labelPlayer(state.you)
    : "Spectator";
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
  els.leave.disabled = !ws || ws.readyState !== 1;
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

// Mirror of server-side movement rules: per-unit terrain costs and the
// max-1-mountain-per-turn cap. Used purely for the visual highlight; the
// server is authoritative for legality.
const MAX_MOUNTAIN_CROSSINGS_PER_TURN = 1;

function moveCostFor(terrain, kind) {
  if (terrain === "sea") return null;
  if (terrain === "plains") return 1;
  if (terrain === "forest") return kind === "scout" ? 1 : 2;
  if (terrain === "mountain") return 2;
  return null;
}

function movePointsFor(kind) {
  if (kind === "scout") return 7;
  if (kind === "heavy_infantry") return 2;
  return 3; // infantry
}

function computeReachable(unit) {
  const map = state.map;
  const mp = movePointsFor(unit.kind);
  const blocked = new Set();
  const friendSet = new Set();
  for (const u of state.units) {
    const k = `${u.pos[0]},${u.pos[1]}`;
    if (u.owner !== unit.owner) blocked.add(k);
    else if (u.id !== unit.id) friendSet.add(k);
  }
  for (const b of state.buildings || []) {
    if (b.kind === "hq") blocked.add(`${b.pos[0]},${b.pos[1]}`);
  }

  // State key encodes mountains crossed: "x,y,m".
  const startKey = `${unit.pos[0]},${unit.pos[1]},0`;
  const best = new Map();
  best.set(startKey, 0);
  const heap = [[0, unit.pos[0], unit.pos[1], 0]];
  while (heap.length) {
    heap.sort((a, b) => a[0] - b[0]);
    const [c, x, y, m] = heap.shift();
    if (c > (best.get(`${x},${y},${m}`) ?? Infinity)) continue;
    for (const [nx, ny] of [[x+1,y],[x-1,y],[x,y+1],[x,y-1]]) {
      if (nx < 0 || ny < 0 || nx >= map.width || ny >= map.height) continue;
      const t = map.tiles[ny * map.width + nx];
      const step = moveCostFor(t, unit.kind);
      if (step == null) continue;
      if (blocked.has(`${nx},${ny}`)) continue;
      const newM = t === "mountain" ? m + 1 : m;
      if (newM > MAX_MOUNTAIN_CROSSINGS_PER_TURN) continue;
      const nc = c + step;
      if (nc > mp) continue;
      const key = `${nx},${ny},${newM}`;
      if (nc < (best.get(key) ?? Infinity)) {
        best.set(key, nc);
        heap.push([nc, nx, ny, newM]);
      }
    }
  }
  // Aggregate (x, y, m) → cheapest reachable coord.
  const out = new Set();
  for (const k of best.keys()) {
    const [x, y] = k.split(",");
    out.add(`${x},${y}`);
  }
  for (const k of friendSet) out.delete(k);
  out.add(`${unit.pos[0]},${unit.pos[1]}`);
  return out;
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
      drawTile(x, y, map.tiles[y * map.width + x]);
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

// Pre-baked deterministic positions for the in-tile decorations so a forest
// always has trees in the same spots and the map doesn't shimmer.
function drawTile(x, y, terrain) {
  const px = x * TILE;
  const py = y * TILE;

  ctx.fillStyle = TERRAIN_COLORS[terrain] || "#222";
  ctx.fillRect(px, py, TILE, TILE);

  if (terrain === "plains") drawPlainsDetail(px, py, x, y);
  else if (terrain === "forest") drawForestDetail(px, py, x, y);
  else if (terrain === "mountain") drawMountainDetail(px, py);
  else if (terrain === "sea") drawSeaDetail(px, py, x, y);

  ctx.strokeStyle = "#0006";
  ctx.lineWidth = 1;
  ctx.strokeRect(px + 0.5, py + 0.5, TILE - 1, TILE - 1);
}

// Cheap deterministic hash so each (x, y) pulls a stable variant.
function tileHash(x, y) {
  let h = (x * 73856093) ^ (y * 19349663);
  h = (h ^ (h >>> 13)) >>> 0;
  return h;
}

function drawPlainsDetail(px, py, x, y) {
  const h = tileHash(x, y);
  // Three faint grass tufts at deterministic offsets so the field looks alive.
  ctx.fillStyle = "#3f6b2c";
  for (let i = 0; i < 3; i++) {
    const ox = ((h >>> (i * 4)) & 0xf) * 2 + 6;
    const oy = ((h >>> (i * 4 + 8)) & 0xf) * 2 + 6;
    ctx.fillRect(px + ox, py + oy, 2, 1);
  }
}

function drawForestDetail(px, py, x, y) {
  const h = tileHash(x, y);
  // Two or three small triangle "trees" with subtle position jitter.
  const seeds = [
    [12 + ((h >>> 0) & 3), 26 + ((h >>> 2) & 3)],
    [28 + ((h >>> 4) & 3), 16 + ((h >>> 6) & 3)],
    [34 + ((h >>> 8) & 3), 32 + ((h >>> 10) & 3)],
  ];
  for (const [tx, ty] of seeds) {
    drawTree(px + tx, py + ty);
  }
}

function drawTree(cx, cy) {
  // Trunk
  ctx.fillStyle = "#3a2a18";
  ctx.fillRect(cx - 1, cy + 1, 2, 4);
  // Canopy: dark triangle stack
  ctx.fillStyle = "#1c3a13";
  ctx.beginPath();
  ctx.moveTo(cx, cy - 9);
  ctx.lineTo(cx - 6, cy + 1);
  ctx.lineTo(cx + 6, cy + 1);
  ctx.closePath();
  ctx.fill();
  // Highlight
  ctx.fillStyle = "#2c5e22";
  ctx.beginPath();
  ctx.moveTo(cx, cy - 9);
  ctx.lineTo(cx - 4, cy - 1);
  ctx.lineTo(cx + 1, cy - 1);
  ctx.closePath();
  ctx.fill();
}

function drawMountainDetail(px, py) {
  // Rock silhouette: two overlapping triangles + snow caps.
  ctx.fillStyle = "#5c4d36";
  ctx.beginPath();
  ctx.moveTo(px + TILE * 0.5, py + 8);
  ctx.lineTo(px + 6, py + TILE - 6);
  ctx.lineTo(px + TILE - 6, py + TILE - 6);
  ctx.closePath();
  ctx.fill();
  // Smaller foreground peak
  ctx.fillStyle = "#6e5c41";
  ctx.beginPath();
  ctx.moveTo(px + TILE * 0.7, py + 18);
  ctx.lineTo(px + TILE * 0.3, py + TILE - 6);
  ctx.lineTo(px + TILE - 6, py + TILE - 6);
  ctx.closePath();
  ctx.fill();
  // Snow caps
  ctx.fillStyle = "#f0f0f0";
  ctx.beginPath();
  ctx.moveTo(px + TILE * 0.5, py + 8);
  ctx.lineTo(px + TILE * 0.5 - 5, py + 16);
  ctx.lineTo(px + TILE * 0.5 + 5, py + 16);
  ctx.closePath();
  ctx.fill();
}

function drawSeaDetail(px, py, x, y) {
  const h = tileHash(x, y);
  // Two short wave lines whose y-offset varies by tile.
  ctx.strokeStyle = "rgba(180, 220, 240, 0.7)";
  ctx.lineWidth = 1.4;
  for (let i = 0; i < 2; i++) {
    const off = 18 + i * 16 + ((h >>> (i * 3)) & 3);
    ctx.beginPath();
    ctx.moveTo(px + 8, py + off);
    ctx.bezierCurveTo(
      px + 18, py + off - 3,
      px + 30, py + off + 3,
      px + TILE - 8, py + off,
    );
    ctx.stroke();
  }
}

function drawBuilding(b, ghost) {
  const [x, y] = b.pos;
  const px = x * TILE;
  const py = y * TILE;
  const ownerColor = b.owner ? (PLAYER_COLORS[b.owner] || "#aaa") : NEUTRAL_COLOR;
  const ownerDark = b.owner === "p1" ? "#992233" : b.owner === "p2" ? "#26609a" : "#5a5a5a";

  ctx.save();
  if (ghost) ctx.globalAlpha = 0.55;

  if (b.kind === "hq") drawHqGlyph(px, py, ownerColor, ownerDark, ghost);
  else if (b.kind === "factory") drawFactoryGlyph(px, py, ownerColor, ownerDark, ghost);
  else if (b.kind === "city") drawCityGlyph(px, py, ownerColor, ownerDark, ghost);

  if (ghost) {
    // Dashed outline so the player remembers it's stale.
    ctx.strokeStyle = "#fff8";
    ctx.lineWidth = 1.2;
    ctx.setLineDash([3, 3]);
    ctx.strokeRect(px + 3, py + 3, TILE - 6, TILE - 6);
    ctx.setLineDash([]);
  }
  if (b.kind === "hq") drawHpBadge(x, y, b.hp, 10);
  ctx.restore();
}

function drawHqGlyph(px, py, color, dark, ghost) {
  // Castle keep: crenellated rectangle + flagpole.
  const baseY = py + 18;
  const w = TILE - 14;
  const h = TILE - 22;
  // Walls
  ctx.fillStyle = dark;
  ctx.fillRect(px + 7, baseY, w, h);
  ctx.fillStyle = color;
  ctx.fillRect(px + 7, baseY, w, h - 4);
  // Crenellations
  for (let i = 0; i < 4; i++) {
    ctx.fillStyle = i % 2 === 0 ? color : dark;
    ctx.fillRect(px + 7 + i * (w / 4), baseY - 4, w / 4 - 1, 5);
  }
  // Door
  ctx.fillStyle = "#1a1a1a";
  ctx.fillRect(px + TILE / 2 - 4, py + TILE - 16, 8, 10);
  // Flagpole
  ctx.fillStyle = "#fff";
  ctx.fillRect(px + TILE / 2 - 1, py + 4, 2, 16);
  // Flag
  ctx.fillStyle = ghost ? "#aaa" : color;
  ctx.beginPath();
  ctx.moveTo(px + TILE / 2 + 1, py + 5);
  ctx.lineTo(px + TILE / 2 + 11, py + 8);
  ctx.lineTo(px + TILE / 2 + 1, py + 11);
  ctx.closePath();
  ctx.fill();
  // Outline
  ctx.strokeStyle = "#fff";
  ctx.lineWidth = 1.5;
  ctx.strokeRect(px + 7, baseY - 4, w, h + 4);
}

function drawFactoryGlyph(px, py, color, dark, _ghost) {
  // Sawtooth roof with chimney + smoke puff.
  const baseY = py + 18;
  const w = TILE - 12;
  const h = TILE - 22;
  // Body
  ctx.fillStyle = color;
  ctx.fillRect(px + 6, baseY, w, h);
  ctx.strokeStyle = "#fff";
  ctx.lineWidth = 1.5;
  ctx.strokeRect(px + 6, baseY, w, h);
  // Sawtooth roof
  const teeth = 4;
  ctx.fillStyle = dark;
  ctx.beginPath();
  ctx.moveTo(px + 6, baseY);
  for (let i = 0; i < teeth; i++) {
    const x0 = px + 6 + i * (w / teeth);
    const x1 = x0 + w / teeth;
    ctx.lineTo(x0 + w / teeth / 2, baseY - 7);
    ctx.lineTo(x1, baseY);
  }
  ctx.closePath();
  ctx.fill();
  ctx.stroke();
  // Chimney
  ctx.fillStyle = dark;
  ctx.fillRect(px + TILE - 14, py + 4, 5, 14);
  ctx.strokeRect(px + TILE - 14, py + 4, 5, 14);
  // Smoke puff
  ctx.fillStyle = "rgba(220,220,220,0.6)";
  ctx.beginPath();
  ctx.arc(px + TILE - 9, py + 6, 4, 0, Math.PI * 2);
  ctx.arc(px + TILE - 4, py + 4, 3, 0, Math.PI * 2);
  ctx.fill();
  // Door
  ctx.fillStyle = "#1a1a1a";
  ctx.fillRect(px + TILE / 2 - 3, py + TILE - 14, 6, 8);
}

function drawCityGlyph(px, py, color, dark, _ghost) {
  // Three towers of varying heights with windows.
  const groundY = py + TILE - 6;
  const towers = [
    { x: px + 8,  w: 9,  h: 18 },
    { x: px + 19, w: 11, h: 26 },
    { x: px + 32, w: 9,  h: 22 },
  ];
  for (const t of towers) {
    ctx.fillStyle = color;
    ctx.fillRect(t.x, groundY - t.h, t.w, t.h);
    ctx.strokeStyle = "#fff";
    ctx.lineWidth = 1.2;
    ctx.strokeRect(t.x, groundY - t.h, t.w, t.h);
  }
  // Windows: lit yellow rectangles
  ctx.fillStyle = "#f6c34a";
  for (const t of towers) {
    const cols = t.w >= 10 ? 2 : 1;
    const colW = (t.w - (cols + 1) * 2) / cols;
    for (let row = 0; row < Math.floor(t.h / 6); row++) {
      const wy = groundY - 5 - row * 6;
      if (wy < py + 6) break;
      for (let c = 0; c < cols; c++) {
        const wx = t.x + 2 + c * (colW + 2);
        ctx.fillRect(wx, wy, colW, 2);
      }
    }
  }
  // Banner stripe under the tallest building so you can spot ownership.
  ctx.fillStyle = dark;
  ctx.fillRect(px + 4, groundY, TILE - 8, 4);
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
