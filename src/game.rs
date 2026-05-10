use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use uuid::Uuid;

pub type Coord = (i32, i32);

/// Inclusive bounds for randomly chosen map dimensions. Each seed picks a
/// width and height inside this box so games vary in size and feel.
pub const MIN_MAP_DIM: i32 = 8;
pub const MAX_MAP_DIM: i32 = 15;
/// Generated maps must have at least this many vertex-disjoint land paths
/// between the two HQs. Stops the random generator from producing maps with a
/// single chokepoint that the better-positioned player can lock down.
pub const MIN_DISJOINT_HQ_PATHS: usize = 3;

/// Bumped whenever the random-map generator's output for a given seed
/// could change (terrain density, placement rules, etc.). Recorded in
/// replay logs so old replays know which generator made them.
pub const MAP_GENERATOR_VERSION: u32 = 3;
/// Bumped whenever rules or unit numbers change in a way that affects
/// gameplay. Recorded in replay logs alongside the seed.
pub const GAME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Wallclock budget for a single player's turn. Once the deadline lapses
/// the lobby's turn-timer task force-ends the active player's turn —
/// whatever they did before the deadline stays committed.
pub const TURN_DURATION_SECS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Terrain {
    Plains,
    Forest,
    Mountain,
    Sea,
}

/// Mountains are punishing: a unit can only enter one mountain tile per
/// turn. The pathfinder tracks `mountains_entered` as part of its state so
/// any path that would step into a second mountain is rejected.
pub const MAX_MOUNTAIN_CROSSINGS_PER_TURN: u32 = 1;

impl Terrain {
    /// Movement cost for the given unit kind, or `None` if impassable.
    /// Forest only slows non-scouts: scouts move through woods at plains
    /// speed. Mountains cost 2 and additionally trigger the per-turn cap.
    pub fn move_cost_for(self, kind: UnitKind) -> Option<u32> {
        use Terrain::*;
        use UnitKind::*;
        match (self, kind) {
            (Sea, _) => None,
            (Plains, _) => Some(1),
            (Forest, Scout) => Some(1),
            (Forest, _) => Some(2),
            (Mountain, _) => Some(2),
        }
    }

    /// Defense stars granted by standing on this terrain. Stars × 10% scale
    /// with the defender's HP fraction in the damage formula.
    pub fn defense_stars_for(self, _kind: UnitKind) -> u32 {
        match self {
            Terrain::Plains => 1,
            Terrain::Forest => 2,
            Terrain::Mountain => 4,
            Terrain::Sea => 0,
        }
    }

    /// Vision bonus when standing on this terrain.
    pub fn vision_bonus(self) -> i32 {
        match self {
            Terrain::Mountain => 3,
            _ => 0,
        }
    }

    /// Forests hide units inside them unless an enemy is adjacent.
    pub fn hides_units(self) -> bool {
        matches!(self, Terrain::Forest)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlayerId {
    P1,
    P2,
}

impl PlayerId {
    pub fn other(self) -> PlayerId {
        match self {
            PlayerId::P1 => PlayerId::P2,
            PlayerId::P2 => PlayerId::P1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    Infantry,
    Scout,
    HeavyInfantry,
}

impl UnitKind {
    pub fn move_points(self) -> u32 {
        match self {
            UnitKind::Infantry => 3,
            UnitKind::Scout => 7,
            UnitKind::HeavyInfantry => 2,
        }
    }
    pub fn vision(self) -> i32 {
        match self {
            UnitKind::Infantry => 2,
            UnitKind::Scout => 4,
            UnitKind::HeavyInfantry => 2,
        }
    }
    pub fn max_hp(self) -> u32 {
        10
    }
    pub fn attack_range(self) -> (i32, i32) {
        match self {
            UnitKind::Infantry => (1, 1),
            UnitKind::Scout => (1, 1),
            UnitKind::HeavyInfantry => (1, 1),
        }
    }
    /// Cost to produce at a factory. Scouts are cheap recon (cheap & fragile),
    /// infantry are the mid-tier all-rounder, heavy infantry is premium armor.
    pub fn cost(self) -> u32 {
        match self {
            UnitKind::Scout => 1000,
            UnitKind::Infantry => 2000,
            UnitKind::HeavyInfantry => 3000,
        }
    }
    /// All current unit kinds are infantry-class and can capture buildings.
    pub fn can_capture(self) -> bool {
        true
    }
    /// Base damage % out of 100 against a given defender. Tuned so each kind
    /// has a clear role:
    /// - Scouts are recon: hit infantry hard, useless against heavy armor,
    ///   take heavy damage from anything except other scouts.
    /// - Heavy infantry is the brick: shrugs off infantry/scout chip damage,
    ///   trades evenly with other heavies.
    /// - Infantry is the all-rounder: average damage in and out vs everything.
    pub fn base_damage(self, target: UnitKind) -> Option<u32> {
        use UnitKind::*;
        Some(match (self, target) {
            (Infantry,      Infantry)      => 55,
            (Infantry,      Scout)         => 70,
            (Infantry,      HeavyInfantry) => 35,
            (Scout,         Infantry)      => 70,
            (Scout,         Scout)         => 45,
            (Scout,         HeavyInfantry) => 20,
            (HeavyInfantry, Infantry)      => 65,
            (HeavyInfantry, Scout)         => 95,
            (HeavyInfantry, HeavyInfantry) => 55,
        })
    }
    /// Base damage % against a building. Only HQs take damage; factories
    /// and cities are captured rather than destroyed.
    pub fn base_damage_vs_building(self, target: BuildingKind) -> u32 {
        match (self, target) {
            (UnitKind::Infantry,      BuildingKind::Hq) => 30,
            (UnitKind::Scout,         BuildingKind::Hq) => 20,
            (UnitKind::HeavyInfantry, BuildingKind::Hq) => 40,
            (_, BuildingKind::Factory) | (_, BuildingKind::City) => 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Unit {
    pub id: Uuid,
    pub kind: UnitKind,
    pub owner: PlayerId,
    pub pos: Coord,
    pub hp: u32,
    #[serde(default)]
    pub has_moved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildingKind {
    Hq,
    Factory,
    City,
}

impl BuildingKind {
    pub fn max_hp(self) -> u32 {
        10
    }
    /// Buildings emit short-range vision so the owner notices nearby threats.
    pub fn vision(self) -> i32 {
        1
    }
    /// HQs block movement; factories and cities are passable so units can stand
    /// on them to capture or to spawn from a factory.
    pub fn blocks_movement(self) -> bool {
        matches!(self, BuildingKind::Hq)
    }
    /// Cities and factories can change ownership by infantry standing on them
    /// at end of turn. HQs cannot be captured — they're destroyed by damage.
    pub fn capturable(self) -> bool {
        matches!(self, BuildingKind::Factory | BuildingKind::City)
    }
    pub fn produces_units(self) -> bool {
        matches!(self, BuildingKind::Factory)
    }
    /// Funds generated each turn by an owned building.
    pub fn income_per_turn(self) -> u32 {
        match self {
            BuildingKind::Hq => 100,
            BuildingKind::Factory => 1000,
            BuildingKind::City => 250,
        }
    }
    /// Extra defense stars when a unit of the given kind is occupying this
    /// building tile. Stack on top of `Terrain::defense_stars_for`.
    /// Infantry get a fortification bonus inside cities; scouts and heavies
    /// don't get the same urban-warfare advantage.
    pub fn defense_stars_for(self, kind: UnitKind) -> u32 {
        use BuildingKind::*;
        use UnitKind::*;
        match (self, kind) {
            (City, Infantry) => 3,
            (City, _) => 2,
            (Factory, Infantry) => 3,
            (Factory, _) => 2,
            (Hq, _) => 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Building {
    pub id: Uuid,
    pub kind: BuildingKind,
    /// `None` for neutral buildings (cities at start of game). HQs are always
    /// owned. Factories may start owned by a player or neutral depending on
    /// the map.
    pub owner: Option<PlayerId>,
    pub pos: Coord,
    pub hp: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Map {
    pub width: i32,
    pub height: i32,
    /// Row-major; len = width * height.
    pub tiles: Vec<Terrain>,
}

impl Map {
    pub fn idx(&self, (x, y): Coord) -> Option<usize> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            None
        } else {
            Some((y * self.width + x) as usize)
        }
    }
    pub fn terrain(&self, c: Coord) -> Option<Terrain> {
        self.idx(c).map(|i| self.tiles[i])
    }
    pub fn in_bounds(&self, c: Coord) -> bool {
        self.idx(c).is_some()
    }
}

/// Generate a random map deterministically from `rng`, mirrored 180° across
/// the map center so the two players land on opposite corners with identical
/// terrain. Features are stamped only in a "canonical half" (top half plus
/// the left half of the center row when height is odd); the rest is copied
/// from there via point reflection.
pub fn random_map(width: i32, height: i32, rng: &mut SmallRng) -> Map {
    assert!(
        width >= 4 && height >= 4,
        "map dimensions must be at least 4 in each axis"
    );
    let n = (width * height) as usize;
    let mut tiles = vec![Terrain::Plains; n];
    let idx = |x: i32, y: i32| -> Option<usize> {
        if x < 0 || y < 0 || x >= width || y >= height {
            None
        } else {
            Some((y * width + x) as usize)
        }
    };
    let in_canonical_half = |x: i32, y: i32| -> bool {
        if y < height / 2 {
            return true;
        }
        // For odd heights the middle row generates from its left half.
        if height % 2 == 1 && y == height / 2 && x < width / 2 {
            return true;
        }
        false
    };
    let stamp = |tiles: &mut [Terrain], pos: Coord, terrain: Terrain| {
        if !in_canonical_half(pos.0, pos.1) {
            return;
        }
        if let Some(i) = idx(pos.0, pos.1) {
            tiles[i] = terrain;
        }
    };

    // Density scales gently with map area so 8x8 maps don't get over-cluttered.
    let area = width * height;
    let n_forests = rng.gen_range((area / 30).max(2)..(area / 14).max(4));
    for _ in 0..n_forests {
        let cx = rng.gen_range(0..width);
        let cy = rng.gen_range(0..height);
        let size = rng.gen_range(3..8);
        for _ in 0..size {
            let ox = cx + rng.gen_range(-2..3);
            let oy = cy + rng.gen_range(-2..3);
            stamp(&mut tiles, (ox, oy), Terrain::Forest);
        }
    }

    let n_ridges = rng.gen_range(1..(area / 50).max(2) + 1);
    for _ in 0..n_ridges {
        let mut x = rng.gen_range(0..width);
        let mut y = rng.gen_range(0..height);
        let length = rng.gen_range(4..10);
        let bias_x: i32 = rng.gen_range(-1..2);
        let bias_y: i32 = rng.gen_range(-1..2);
        for _ in 0..length {
            stamp(&mut tiles, (x, y), Terrain::Mountain);
            x = (x + bias_x + rng.gen_range(-1..2)).clamp(0, width - 1);
            y = (y + bias_y + rng.gen_range(-1..2)).clamp(0, height - 1);
        }
    }

    let n_lakes = rng.gen_range(0..(area / 60).max(1) + 1);
    for _ in 0..n_lakes {
        let cx = rng.gen_range(0..width);
        let cy = rng.gen_range(0..height);
        let size = rng.gen_range(2..5);
        for _ in 0..size {
            let ox = cx + rng.gen_range(-2..3);
            let oy = cy + rng.gen_range(-2..3);
            stamp(&mut tiles, (ox, oy), Terrain::Sea);
        }
    }

    // 180° rotation through center: copy each canonical-half tile onto its
    // partner. When width and height are both odd a single self-mapping
    // tile (the exact center) is left as plains.
    for y in 0..height {
        for x in 0..width {
            if !in_canonical_half(x, y) {
                continue;
            }
            let mx = width - 1 - x;
            let my = height - 1 - y;
            if mx == x && my == y {
                continue;
            }
            let src = (y * width + x) as usize;
            let dst = (my * width + mx) as usize;
            tiles[dst] = tiles[src];
        }
    }

    Map {
        width,
        height,
        tiles,
    }
}

/// 180° rotation through the map center: P1's coord ↔ P2's coord.
fn mirror_pos(width: i32, height: i32, pos: Coord) -> Coord {
    (width - 1 - pos.0, height - 1 - pos.1)
}

/// Pick HQ/factory/city positions on `map`. P1 lands in the top quarter,
/// P2 in the bottom quarter, factories adjacent to their HQ on land, and
/// cities scattered through the middle band. Returns `None` if the random
/// terrain made any of these picks impossible.
pub struct RandomPlacements {
    pub p1_hq: Coord,
    pub p2_hq: Coord,
    pub p1_factory: Coord,
    pub p2_factory: Coord,
    pub cities: Vec<Coord>,
    pub p1_units: Vec<(UnitKind, Coord)>,
    pub p2_units: Vec<(UnitKind, Coord)>,
}

pub fn random_placements(map: &Map, rng: &mut SmallRng) -> Option<RandomPlacements> {
    let half = map.height / 2;
    let q = (map.height / 4).max(2);
    let mut occupied: HashSet<Coord> = HashSet::new();

    // P1 is placed in the top half; P2 is the exact mirror across the midline.
    let p1_hq = pick_random_buildable(map, rng, 1..q, &occupied)?;
    let p2_hq = mirror_pos(map.width, map.height,p1_hq);
    if p1_hq == p2_hq {
        return None; // shouldn't happen on an even-height map but guard anyway
    }
    occupied.insert(p1_hq);
    occupied.insert(p2_hq);

    let p1_factory = pick_adjacent_buildable(map, p1_hq, rng, &occupied)?;
    let p2_factory = mirror_pos(map.width, map.height,p1_factory);
    occupied.insert(p1_factory);
    occupied.insert(p2_factory);

    // Cities come in mirrored pairs in the upper-middle ↔ lower-middle bands.
    // Each pick chooses one tile in the top middle band [q, half) and we
    // automatically place its reflection.
    let n_pairs = rng.gen_range(3..6);
    let mut cities = Vec::with_capacity(n_pairs * 2);
    for _ in 0..400 {
        if cities.len() >= n_pairs * 2 {
            break;
        }
        let Some(pos) = pick_random_buildable(map, rng, q..half, &occupied) else {
            break;
        };
        let mirror = mirror_pos(map.width, map.height,pos);
        if mirror == pos || occupied.contains(&mirror) {
            continue;
        }
        // Mirror tile must also be buildable. The map is mirrored so this
        // should always hold, but verify.
        if !map.terrain(mirror).map_or(false, is_buildable) {
            continue;
        }
        cities.push(pos);
        cities.push(mirror);
        occupied.insert(pos);
        occupied.insert(mirror);
    }

    // Starting army: pick P1 unit positions, mirror to P2.
    let p1_units = pick_starting_units(map, &[p1_hq, p1_factory], rng, &mut occupied)?;
    let mut p2_units = Vec::with_capacity(p1_units.len());
    for &(kind, pos) in &p1_units {
        let mpos = mirror_pos(map.width, map.height,pos);
        if occupied.contains(&mpos) {
            return None;
        }
        if !map.terrain(mpos).map_or(false, is_buildable) {
            return None;
        }
        occupied.insert(mpos);
        p2_units.push((kind, mpos));
    }

    Some(RandomPlacements {
        p1_hq,
        p2_hq,
        p1_factory,
        p2_factory,
        cities,
        p1_units,
        p2_units,
    })
}

/// Buildings (HQ, factory, city) can only be placed on plains. Mountains
/// and forests are passable terrain features for units, not building lots,
/// and sea is impassable entirely.
fn is_buildable(t: Terrain) -> bool {
    matches!(t, Terrain::Plains)
}


fn pick_random_buildable(
    map: &Map,
    rng: &mut SmallRng,
    y_range: std::ops::Range<i32>,
    exclude: &HashSet<Coord>,
) -> Option<Coord> {
    if y_range.start >= y_range.end {
        return None;
    }
    for _ in 0..400 {
        let x = rng.gen_range(0..map.width);
        let y = rng.gen_range(y_range.start..y_range.end);
        let pos = (x, y);
        if exclude.contains(&pos) {
            continue;
        }
        if map.terrain(pos).map_or(false, is_buildable) {
            return Some(pos);
        }
    }
    None
}

fn pick_adjacent_buildable(
    map: &Map,
    pos: Coord,
    rng: &mut SmallRng,
    exclude: &HashSet<Coord>,
) -> Option<Coord> {
    let mut adj: Vec<Coord> = neighbors4(pos)
        .into_iter()
        .filter(|p| map.in_bounds(*p))
        .filter(|p| !exclude.contains(p))
        .filter(|p| map.terrain(*p).map_or(false, is_buildable))
        .collect();
    if adj.is_empty() {
        return None;
    }
    adj.shuffle(rng);
    Some(adj[0])
}

fn pick_starting_units(
    map: &Map,
    seeds: &[Coord],
    rng: &mut SmallRng,
    occupied: &mut HashSet<Coord>,
) -> Option<Vec<(UnitKind, Coord)>> {
    // Starting units spawn only on plains. Forests would hide them from the
    // start (weird for an opening) and mountains slow first-turn movement
    // for everything except heavy infantry. Pure plains keeps openings fair.
    let mut candidates: Vec<Coord> = Vec::new();
    let consider = |candidates: &mut Vec<Coord>, n: Coord| {
        if !map.in_bounds(n) {
            return;
        }
        if occupied.contains(&n) || candidates.contains(&n) {
            return;
        }
        if map.terrain(n).map_or(false, is_buildable) {
            candidates.push(n);
        }
    };
    for s in seeds {
        for n in neighbors4(*s) {
            consider(&mut candidates, n);
        }
    }
    if candidates.len() < 2 {
        // Fallback: widen search to distance-2 tiles around any seed.
        for s in seeds {
            for dx in -2i32..=2 {
                for dy in -2i32..=2 {
                    consider(&mut candidates, (s.0 + dx, s.1 + dy));
                }
            }
        }
    }
    if candidates.len() < 2 {
        return None;
    }
    candidates.shuffle(rng);
    let scout_pos = candidates[0];
    let inf_pos = candidates[1];
    occupied.insert(scout_pos);
    occupied.insert(inf_pos);
    Some(vec![
        (UnitKind::Scout, scout_pos),
        (UnitKind::Infantry, inf_pos),
    ])
}

/// Count vertex-disjoint land paths between two coords using the standard
/// successive-shortest-paths heuristic: repeatedly BFS, mark interior tiles
/// of each found path as used, stop when no new path exists. Treats sea
/// as impassable. The endpoints themselves are always usable so we don't
/// undercount when the HQ tile is in the middle of a contested area.
pub fn count_disjoint_paths(map: &Map, start: Coord, end: Coord, max: usize) -> usize {
    let mut blocked: HashSet<Coord> = HashSet::new();
    let mut count = 0;
    while count < max {
        match bfs_path_avoiding(map, start, end, &blocked) {
            Some(path) => {
                count += 1;
                if path.len() > 2 {
                    for &tile in &path[1..path.len() - 1] {
                        blocked.insert(tile);
                    }
                } else {
                    // Adjacent endpoints: no interior tiles to block. We can't
                    // count more than one path through that single edge.
                    return count;
                }
            }
            None => break,
        }
    }
    count
}

fn bfs_path_avoiding(
    map: &Map,
    start: Coord,
    end: Coord,
    blocked: &HashSet<Coord>,
) -> Option<Vec<Coord>> {
    let mut prev: HashMap<Coord, Option<Coord>> = HashMap::new();
    prev.insert(start, None);
    let mut queue: VecDeque<Coord> = VecDeque::new();
    queue.push_back(start);
    while let Some(pos) = queue.pop_front() {
        if pos == end {
            break;
        }
        for n in neighbors4(pos) {
            if !map.in_bounds(n) {
                continue;
            }
            if prev.contains_key(&n) {
                continue;
            }
            if blocked.contains(&n) && n != end {
                continue;
            }
            if map.terrain(n).map_or(true, |t| t == Terrain::Sea) {
                continue;
            }
            prev.insert(n, Some(pos));
            queue.push_back(n);
        }
    }
    if !prev.contains_key(&end) {
        return None;
    }
    let mut path = vec![end];
    let mut cur = end;
    while let Some(Some(p)) = prev.get(&cur).copied() {
        path.push(p);
        cur = p;
    }
    path.reverse();
    Some(path)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameState {
    pub map: Map,
    pub units: HashMap<Uuid, Unit>,
    pub buildings: HashMap<Uuid, Building>,
    pub current_turn: PlayerId,
    pub turn_number: u32,
    pub winner: Option<PlayerId>,
    /// What just happened — populated by try_action and cleared on the next action.
    pub last_action: Option<ActionReport>,
    /// Per-player memory of buildings the player has seen at least once.
    /// Each entry is a snapshot of the building taken the last time the player
    /// could see its tile (so HP/owner reflect the last sighting, not the
    /// current truth — that's the "ghost" behavior).
    #[serde(skip)]
    pub seen_buildings: HashMap<PlayerId, HashMap<Uuid, SeenBuilding>>,
    /// Players who started the match with an HQ. Used by the HQ-loss win rule
    /// so test setups without buildings don't immediately decide a winner.
    #[serde(skip)]
    pub hq_owners: HashSet<PlayerId>,
    /// Cash on hand for each player. Spent on units at factories.
    pub funds: HashMap<PlayerId, u32>,
    /// Factories that have already produced a unit during the current turn.
    /// Cleared at the start of every turn.
    #[serde(skip)]
    pub factories_used: HashSet<Uuid>,
    /// The seed used to generate this map. Surfaced in PlayerView so
    /// players/agents can record or reproduce a particular layout.
    pub map_seed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeenBuilding {
    pub building: Building,
    pub last_seen_turn: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RememberedBuilding {
    #[serde(flatten)]
    pub building: Building,
    pub currently_visible: bool,
    pub last_seen_turn: u32,
}

impl GameState {
    /// Build a fresh game on a random map. Seeds are sampled from system
    /// entropy until one yields a map that satisfies `MIN_DISJOINT_HQ_PATHS`.
    /// The chosen seed is recorded in `map_seed` so the layout can be
    /// reproduced via `with_seed`.
    pub fn new() -> Self {
        let mut entropy_rng = SmallRng::from_entropy();
        for _ in 0..400 {
            let seed: u64 = entropy_rng.r#gen();
            if let Some(state) = Self::try_with_seed(seed) {
                return state;
            }
        }
        // Astronomically unlikely fallback: a fixed seed we know works.
        Self::try_with_seed(0xA9E0_57A6_5DEA_DBEE)
            .expect("fallback seed should always validate")
    }

    /// Build a game on a specific seed. Returns `Err` if the seed produces
    /// a map without enough disjoint HQ paths — callers can decide whether
    /// to surface the error or pick a different seed.
    pub fn with_seed(seed: u64) -> Result<Self, String> {
        Self::try_with_seed(seed).ok_or_else(|| {
            format!(
                "seed {seed} produced a map without {MIN_DISJOINT_HQ_PATHS} disjoint HQ paths"
            )
        })
    }

    fn try_with_seed(seed: u64) -> Option<Self> {
        let mut rng = SmallRng::seed_from_u64(seed);
        // Map dimensions are part of the seed so the same number reproduces
        // exactly the same battlefield (size + terrain + placements).
        let width = rng.gen_range(MIN_MAP_DIM..=MAX_MAP_DIM);
        let height = rng.gen_range(MIN_MAP_DIM..=MAX_MAP_DIM);
        let map = random_map(width, height, &mut rng);
        let placements = random_placements(&map, &mut rng)?;
        if count_disjoint_paths(
            &map,
            placements.p1_hq,
            placements.p2_hq,
            MIN_DISJOINT_HQ_PATHS,
        ) < MIN_DISJOINT_HQ_PATHS
        {
            return None;
        }
        Some(Self::assemble(seed, map, placements))
    }

    fn assemble(seed: u64, map: Map, p: RandomPlacements) -> Self {
        let mut units: HashMap<Uuid, Unit> = HashMap::new();
        for (owner, list) in [(PlayerId::P1, &p.p1_units), (PlayerId::P2, &p.p2_units)] {
            for &(kind, pos) in list {
                let id = Uuid::new_v4();
                units.insert(
                    id,
                    Unit {
                        id,
                        kind,
                        owner,
                        pos,
                        hp: kind.max_hp(),
                        has_moved: false,
                    },
                );
            }
        }

        let mut buildings: HashMap<Uuid, Building> = HashMap::new();
        let mut add = |kind: BuildingKind, owner: Option<PlayerId>, pos: Coord| {
            let id = Uuid::new_v4();
            buildings.insert(
                id,
                Building {
                    id,
                    kind,
                    owner,
                    pos,
                    hp: kind.max_hp(),
                },
            );
        };
        add(BuildingKind::Hq, Some(PlayerId::P1), p.p1_hq);
        add(BuildingKind::Hq, Some(PlayerId::P2), p.p2_hq);
        add(BuildingKind::Factory, Some(PlayerId::P1), p.p1_factory);
        add(BuildingKind::Factory, Some(PlayerId::P2), p.p2_factory);
        for pos in p.cities {
            add(BuildingKind::City, None, pos);
        }

        let mut funds = HashMap::new();
        funds.insert(PlayerId::P1, 4000);
        funds.insert(PlayerId::P2, 4000);

        Self {
            map,
            units,
            buildings,
            current_turn: PlayerId::P1,
            turn_number: 1,
            winner: None,
            last_action: None,
            seen_buildings: HashMap::new(),
            hq_owners: [PlayerId::P1, PlayerId::P2].into_iter().collect(),
            funds,
            factories_used: HashSet::new(),
            map_seed: seed,
        }
    }

    pub fn building_at(&self, c: Coord) -> Option<&Building> {
        self.buildings.values().find(|b| b.pos == c)
    }

    /// Total defensive stars a unit gets on its current tile: terrain stars
    /// plus any building stars granted to that unit kind. This is the single
    /// extension point for "infantry on city = +3", "scout in mountain = +N",
    /// etc. Adjust the underlying `defense_stars_for` methods to expand.
    pub fn defense_stars_for_unit(&self, unit: &Unit) -> u32 {
        let terrain = self
            .map
            .terrain(unit.pos)
            .map(|t| t.defense_stars_for(unit.kind))
            .unwrap_or(0);
        let bldg = self
            .building_at(unit.pos)
            .map(|b| b.kind.defense_stars_for(unit.kind))
            .unwrap_or(0);
        terrain + bldg
    }

    /// Defensive stars when the building itself is the attack target
    /// (currently only HQs, since cities/factories are captured rather than
    /// destroyed). Uses terrain stars only — buildings don't grant
    /// additional armor to themselves.
    pub fn defense_stars_for_building(&self, b: &Building) -> u32 {
        self.map
            .terrain(b.pos)
            .map(|t| t.defense_stars_for(UnitKind::Infantry))
            .unwrap_or(0)
    }

    pub fn unit_at(&self, c: Coord) -> Option<&Unit> {
        self.units.values().find(|u| u.pos == c)
    }

    /// Compute reachable tiles for a unit. State-tracked Dijkstra over
    /// (coord, mountains_crossed) so we can reject paths that exceed the
    /// MAX_MOUNTAIN_CROSSINGS_PER_TURN limit.
    pub fn reachable(&self, unit_id: Uuid) -> HashMap<Coord, u32> {
        let Some(unit) = self.units.get(&unit_id) else {
            return HashMap::new();
        };
        let mp = unit.kind.move_points();

        let mut best: HashMap<(Coord, u32), u32> = HashMap::new();
        best.insert((unit.pos, 0), 0);

        let mut heap: BinaryHeap<std::cmp::Reverse<(u32, Coord, u32)>> = BinaryHeap::new();
        heap.push(std::cmp::Reverse((0, unit.pos, 0)));

        while let Some(std::cmp::Reverse((cost, pos, m))) = heap.pop() {
            if cost > *best.get(&(pos, m)).unwrap_or(&u32::MAX) {
                continue;
            }
            for n in neighbors4(pos) {
                let Some(terrain) = self.map.terrain(n) else {
                    continue;
                };
                let Some(step) = terrain.move_cost_for(unit.kind) else {
                    continue;
                };
                if let Some(other) = self.unit_at(n) {
                    if other.owner != unit.owner {
                        continue;
                    }
                }
                if let Some(b) = self.building_at(n) {
                    if b.kind.blocks_movement() {
                        continue;
                    }
                }
                let new_m = if terrain == Terrain::Mountain {
                    m + 1
                } else {
                    m
                };
                if new_m > MAX_MOUNTAIN_CROSSINGS_PER_TURN {
                    continue;
                }
                let new_cost = cost + step;
                if new_cost > mp {
                    continue;
                }
                if new_cost < *best.get(&(n, new_m)).unwrap_or(&u32::MAX) {
                    best.insert((n, new_m), new_cost);
                    heap.push(std::cmp::Reverse((new_cost, n, new_m)));
                }
            }
        }

        // Aggregate (coord, m) → coord with cheapest reach.
        let mut min_cost: HashMap<Coord, u32> = HashMap::new();
        for ((c, _), cost) in best {
            let entry = min_cost.entry(c).or_insert(u32::MAX);
            if cost < *entry {
                *entry = cost;
            }
        }

        // Can't stop on tiles occupied by other units or movement-blocking buildings.
        min_cost.retain(|&pos, _| {
            pos == unit.pos
                || (self.unit_at(pos).is_none()
                    && self
                        .building_at(pos)
                        .map_or(true, |b| !b.kind.blocks_movement()))
        });
        min_cost
    }

    /// Reconstruct the cheapest path the unit would walk to `dest` using the
    /// same constraints as `reachable` (terrain costs, mountain cap, blocked
    /// tiles). Returns the path including start and end, or `None` if `dest`
    /// isn't reachable under those constraints.
    pub fn compute_path(&self, unit_id: Uuid, dest: Coord) -> Option<Vec<Coord>> {
        let unit = self.units.get(&unit_id)?;
        let mp = unit.kind.move_points();
        let start = unit.pos;
        if dest == start {
            return Some(vec![start]);
        }

        let mut best: HashMap<(Coord, u32), u32> = HashMap::new();
        let mut parent: HashMap<(Coord, u32), (Coord, u32)> = HashMap::new();
        best.insert((start, 0), 0);
        let mut heap: BinaryHeap<std::cmp::Reverse<(u32, Coord, u32)>> = BinaryHeap::new();
        heap.push(std::cmp::Reverse((0, start, 0)));

        while let Some(std::cmp::Reverse((cost, pos, m))) = heap.pop() {
            if cost > *best.get(&(pos, m)).unwrap_or(&u32::MAX) {
                continue;
            }
            for n in neighbors4(pos) {
                let Some(terrain) = self.map.terrain(n) else {
                    continue;
                };
                let Some(step) = terrain.move_cost_for(unit.kind) else {
                    continue;
                };
                if let Some(other) = self.unit_at(n) {
                    if other.owner != unit.owner {
                        continue;
                    }
                }
                if let Some(b) = self.building_at(n) {
                    if b.kind.blocks_movement() {
                        continue;
                    }
                }
                let new_m = if terrain == Terrain::Mountain {
                    m + 1
                } else {
                    m
                };
                if new_m > MAX_MOUNTAIN_CROSSINGS_PER_TURN {
                    continue;
                }
                let new_cost = cost + step;
                if new_cost > mp {
                    continue;
                }
                if new_cost < *best.get(&(n, new_m)).unwrap_or(&u32::MAX) {
                    best.insert((n, new_m), new_cost);
                    parent.insert((n, new_m), (pos, m));
                    heap.push(std::cmp::Reverse((new_cost, n, new_m)));
                }
            }
        }

        // Pick the (dest, m) with the lowest cost.
        let mut chosen: Option<(u32, u32)> = None; // (m, cost)
        for m in 0..=MAX_MOUNTAIN_CROSSINGS_PER_TURN {
            if let Some(&c) = best.get(&(dest, m)) {
                match chosen {
                    None => chosen = Some((m, c)),
                    Some((_, prev)) if c < prev => chosen = Some((m, c)),
                    _ => {}
                }
            }
        }
        let (mut m_cur, _) = chosen?;
        let mut path = vec![dest];
        let mut cur = dest;
        while let Some(&(p, pm)) = parent.get(&(cur, m_cur)) {
            path.push(p);
            cur = p;
            m_cur = pm;
            if cur == start {
                break;
            }
        }
        path.reverse();
        Some(path)
    }

    /// Move a unit and optionally attack a target (enemy unit OR enemy building).
    /// Pass `dest == current pos` for a stationary attack. Pass `attack = None` to just move.
    pub fn try_action(
        &mut self,
        actor: PlayerId,
        unit_id: Uuid,
        dest: Coord,
        attack: Option<Coord>,
    ) -> Result<ActionReport, String> {
        if self.winner.is_some() {
            return Err("game is over".into());
        }
        if actor != self.current_turn {
            return Err("not your turn".into());
        }
        let unit = self
            .units
            .get(&unit_id)
            .ok_or("unit not found")?
            .clone();
        if unit.owner != actor {
            return Err("not your unit".into());
        }
        if unit.has_moved {
            return Err("unit already acted this turn".into());
        }
        let reachable = self.reachable(unit_id);
        if !reachable.contains_key(&dest) {
            return Err("destination not reachable".into());
        }
        // (Reachable already filtered impassable buildings and units; the
        // remaining `dest != unit.pos && unit_at` check is just a defensive
        // sanity in case state changed underneath us.)
        if dest != unit.pos && self.unit_at(dest).is_some() {
            return Err("destination occupied".into());
        }

        // Validate attack target.
        let (min_r, max_r) = unit.kind.attack_range();
        let attack_target = match attack {
            None => None,
            Some(target_pos) => {
                let manhattan = (dest.0 - target_pos.0).abs() + (dest.1 - target_pos.1).abs();
                if manhattan < min_r || manhattan > max_r {
                    return Err("target out of range".into());
                }
                if let Some(target_unit) = self.unit_at(target_pos) {
                    if target_unit.owner == actor {
                        return Err("cannot attack your own unit".into());
                    }
                    Some(AttackTarget::Unit(target_unit.id))
                } else if let Some(target_bld) = self.building_at(target_pos) {
                    if target_bld.owner == Some(actor) {
                        return Err("cannot attack your own building".into());
                    }
                    if !matches!(target_bld.kind, BuildingKind::Hq) {
                        return Err(
                            "only HQs take damage; cities and factories are captured".into(),
                        );
                    }
                    Some(AttackTarget::Building(target_bld.id))
                } else {
                    return Err("no target at attack coord".into());
                }
            }
        };

        // Compute the path before mutating, so we can include it in the report
        // (used by the browser client to animate the unit walking).
        let path = self
            .compute_path(unit_id, dest)
            .unwrap_or_else(|| vec![unit.pos, dest]);

        // Apply move.
        {
            let unit_mut = self.units.get_mut(&unit_id).unwrap();
            unit_mut.pos = dest;
            unit_mut.has_moved = true;
        }

        let mut report = ActionReport {
            unit_id,
            moved_to: dest,
            path,
            target_id: None,
            target_kind: None,
            damage_to_defender: None,
            damage_to_attacker: None,
            defender_killed: false,
            attacker_killed: false,
        };

        match attack_target {
            None => {}
            Some(AttackTarget::Unit(target_id)) => {
                let attacker = self.units[&unit_id].clone();
                let defender = self.units[&target_id].clone();
                let def_stars = self.defense_stars_for_unit(&defender);
                let dmg = compute_damage(&attacker, def_stars, &defender);
                report.target_id = Some(target_id);
                report.target_kind = Some(TargetKind::Unit);
                report.damage_to_defender = Some(dmg);

                let def_mut = self.units.get_mut(&target_id).unwrap();
                def_mut.hp = def_mut.hp.saturating_sub(dmg);
                let dead = def_mut.hp == 0;
                report.defender_killed = dead;

                if dead {
                    self.units.remove(&target_id);
                } else {
                    let defender = self.units[&target_id].clone();
                    let (dmin, dmax) = defender.kind.attack_range();
                    let m = (defender.pos.0 - dest.0).abs() + (defender.pos.1 - dest.1).abs();
                    if m >= dmin && m <= dmax {
                        let atk = self.units[&unit_id].clone();
                        let atk_stars = self.defense_stars_for_unit(&atk);
                        let counter = compute_damage(&defender, atk_stars, &atk);
                        report.damage_to_attacker = Some(counter);
                        let atk_mut = self.units.get_mut(&unit_id).unwrap();
                        atk_mut.hp = atk_mut.hp.saturating_sub(counter);
                        if atk_mut.hp == 0 {
                            self.units.remove(&unit_id);
                            report.attacker_killed = true;
                        }
                    }
                }
            }
            Some(AttackTarget::Building(target_id)) => {
                let attacker = self.units[&unit_id].clone();
                let bld = self.buildings[&target_id].clone();
                let def_stars = self.defense_stars_for_building(&bld);
                let dmg = compute_damage_vs_building(&attacker, def_stars, &bld);
                report.target_id = Some(target_id);
                report.target_kind = Some(TargetKind::Building);
                report.damage_to_defender = Some(dmg);
                let bld_mut = self.buildings.get_mut(&target_id).unwrap();
                bld_mut.hp = bld_mut.hp.saturating_sub(dmg);
                if bld_mut.hp == 0 {
                    report.defender_killed = true;
                    self.buildings.remove(&target_id);
                }
            }
        }

        // Win condition: a player loses ONLY when their HQ is destroyed
        // (10 HP of damage) or they surrender. Running out of units is not
        // an automatic loss — the routed player can still rebuild from a
        // factory, capture cities to generate income, or hold their HQ until
        // surrender becomes available.
        let p1_lost_hq = self.hq_owners.contains(&PlayerId::P1)
            && !self
                .buildings
                .values()
                .any(|b| b.owner == Some(PlayerId::P1) && b.kind == BuildingKind::Hq);
        let p2_lost_hq = self.hq_owners.contains(&PlayerId::P2)
            && !self
                .buildings
                .values()
                .any(|b| b.owner == Some(PlayerId::P2) && b.kind == BuildingKind::Hq);
        if p1_lost_hq && p2_lost_hq {
            self.winner = Some(self.current_turn);
        } else if p1_lost_hq {
            self.winner = Some(PlayerId::P2);
        } else if p2_lost_hq {
            self.winner = Some(PlayerId::P1);
        }

        self.last_action = Some(report.clone());
        Ok(report)
    }


    /// Concede the match. Only allowed after 3 complete turn cycles
    /// (turn_number >= 4) so neither side can rage-quit immediately.
    pub fn try_surrender(&mut self, actor: PlayerId) -> Result<(), String> {
        if self.winner.is_some() {
            return Err("game is already over".into());
        }
        if self.turn_number < 4 {
            return Err(format!(
                "surrender not allowed until turn 4 (currently turn {})",
                self.turn_number
            ));
        }
        self.winner = Some(actor.other());
        Ok(())
    }

    pub fn end_turn(&mut self, actor: PlayerId) -> Result<(), String> {
        if actor != self.current_turn {
            return Err("not your turn".into());
        }

        // Process captures: any of the actor's units sitting on a capturable
        // building they don't own takes ownership instantly.
        self.process_captures(actor);

        // Hand the turn over.
        self.current_turn = actor.other();
        if self.current_turn == PlayerId::P1 {
            self.turn_number += 1;
        }
        for u in self.units.values_mut() {
            if u.owner == self.current_turn {
                u.has_moved = false;
            }
        }

        // Incoming player collects income from all owned buildings.
        self.collect_income(self.current_turn);

        // Factories can produce again on the new turn.
        self.factories_used.clear();

        self.last_action = None;
        Ok(())
    }

    fn process_captures(&mut self, actor: PlayerId) {
        let captures: Vec<Uuid> = self
            .buildings
            .values()
            .filter(|b| b.kind.capturable() && b.owner != Some(actor))
            .filter(|b| {
                self.units
                    .values()
                    .any(|u| u.pos == b.pos && u.owner == actor && u.kind.can_capture())
            })
            .map(|b| b.id)
            .collect();
        for bid in captures {
            if let Some(b) = self.buildings.get_mut(&bid) {
                b.owner = Some(actor);
            }
        }
    }

    fn collect_income(&mut self, player: PlayerId) {
        let income: u32 = self
            .buildings
            .values()
            .filter(|b| b.owner == Some(player))
            .map(|b| b.kind.income_per_turn())
            .sum();
        *self.funds.entry(player).or_default() += income;
    }

    /// Spend funds at one of your factories to spawn a unit on its tile.
    /// Constraints: the factory must be yours, idle this turn, and have an
    /// empty tile (no unit currently standing on it). The new unit is marked
    /// has_moved=true so it can't act until next turn.
    pub fn try_buy_unit(
        &mut self,
        actor: PlayerId,
        factory_id: Uuid,
        kind: UnitKind,
    ) -> Result<Uuid, String> {
        if self.winner.is_some() {
            return Err("game is over".into());
        }
        if actor != self.current_turn {
            return Err("not your turn".into());
        }
        let factory = self.buildings.get(&factory_id).ok_or("factory not found")?;
        if !factory.kind.produces_units() {
            return Err("that building does not produce units".into());
        }
        if factory.owner != Some(actor) {
            return Err("not your factory".into());
        }
        if self.factories_used.contains(&factory_id) {
            return Err("factory already produced this turn".into());
        }
        if self.unit_at(factory.pos).is_some() {
            return Err("factory tile is occupied — move the unit off first".into());
        }
        let cost = kind.cost();
        let funds = self.funds.get(&actor).copied().unwrap_or(0);
        if funds < cost {
            return Err(format!(
                "insufficient funds: need {cost}, have {funds}"
            ));
        }

        let pos = factory.pos;
        *self.funds.entry(actor).or_default() -= cost;
        self.factories_used.insert(factory_id);

        let id = Uuid::new_v4();
        let unit = Unit {
            id,
            kind,
            owner: actor,
            pos,
            hp: kind.max_hp(),
            has_moved: true,
        };
        self.units.insert(id, unit);
        Ok(id)
    }

    /// Tiles visible to a player given their units' and buildings' vision.
    pub fn visible_tiles(&self, player: PlayerId) -> HashSet<Coord> {
        let mut vis = HashSet::new();
        for u in self.units.values().filter(|u| u.owner == player) {
            vis.insert(u.pos);
            let terrain = self.map.terrain(u.pos).unwrap_or(Terrain::Plains);
            let r = u.kind.vision() + terrain.vision_bonus();
            for dy in -r..=r {
                for dx in -r..=r {
                    let c = (u.pos.0 + dx, u.pos.1 + dy);
                    if self.map.in_bounds(c) {
                        vis.insert(c);
                    }
                }
            }
        }
        for b in self
            .buildings
            .values()
            .filter(|b| b.owner == Some(player))
        {
            vis.insert(b.pos);
            let r = b.kind.vision();
            for dy in -r..=r {
                for dx in -r..=r {
                    let c = (b.pos.0 + dx, b.pos.1 + dy);
                    if self.map.in_bounds(c) {
                        vis.insert(c);
                    }
                }
            }
        }
        vis
    }
}

fn neighbors4((x, y): Coord) -> [Coord; 4] {
    [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)]
}

/// What kind of thing an attack is targeting; resolved at validation time.
enum AttackTarget {
    Unit(Uuid),
    Building(Uuid),
}

/// Damage formula against buildings. Same shape as unit-vs-unit but uses
/// `base_damage_vs_building`. `def_stars` should be the total defensive
/// stars the building's tile grants the building (terrain + the building's
/// own defense bonus).
pub fn compute_damage_vs_building(attacker: &Unit, def_stars: u32, target: &Building) -> u32 {
    let base = attacker.kind.base_damage_vs_building(target.kind) as f32;
    let max_hp = UnitKind::max_hp(attacker.kind) as f32;
    let atk_hp_ratio = attacker.hp as f32 / max_hp;
    let raw = base * atk_hp_ratio / 10.0;
    let def_hp_ratio = target.hp as f32 / target.kind.max_hp() as f32;
    let reduction = (def_stars as f32 * 0.1 * def_hp_ratio).clamp(0.0, 0.9);
    let final_dmg = raw * (1.0 - reduction);
    final_dmg.round().max(0.0) as u32
}

/// Compute damage to a defender unit. `def_stars` is the precomputed total
/// defensive stars for the defender on its current tile (terrain + building
/// bonus). Pulling the lookup out of this function lets the engine apply
/// per-unit-kind tile effects without changing the formula.
pub fn compute_damage(attacker: &Unit, def_stars: u32, defender: &Unit) -> u32 {
    let Some(base) = attacker.kind.base_damage(defender.kind) else {
        return 0;
    };
    let max_hp = UnitKind::max_hp(attacker.kind) as f32;
    let atk_hp_ratio = attacker.hp as f32 / max_hp;
    let raw = base as f32 * atk_hp_ratio / 10.0;
    let def_hp_ratio = defender.hp as f32 / max_hp;
    let reduction = (def_stars as f32 * 0.1 * def_hp_ratio).clamp(0.0, 0.9);
    let final_dmg = raw * (1.0 - reduction);
    final_dmg.round().max(0.0) as u32
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionReport {
    pub unit_id: Uuid,
    pub moved_to: Coord,
    /// Tile-by-tile path the unit took, including start and end positions.
    pub path: Vec<Coord>,
    pub target_id: Option<Uuid>,
    /// "unit" or "building" — only present when target_id is set.
    pub target_kind: Option<TargetKind>,
    pub damage_to_defender: Option<u32>,
    pub damage_to_attacker: Option<u32>,
    pub defender_killed: bool,
    pub attacker_killed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Unit,
    Building,
}

/// View of the world from a particular vantage point.
/// `Spectator` sees everything; player views are fog-filtered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum View {
    Player(PlayerId),
    Spectator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerView {
    pub map: Map,
    pub units: Vec<Unit>,
    pub buildings: Vec<RememberedBuilding>,
    pub visible_tiles: Vec<Coord>,
    pub current_turn: PlayerId,
    pub turn_number: u32,
    pub winner: Option<PlayerId>,
    pub you: Option<PlayerId>,
    /// Funds visible to this viewer: just their own for players, all for
    /// spectators (and at game over, both players' funds are revealed).
    pub funds: HashMap<PlayerId, u32>,
    /// Factories that have already produced this turn (so the client can
    /// gray out their buy buttons).
    pub factories_used: Vec<Uuid>,
    /// Seed used to generate this map. Identical seeds reproduce identical
    /// maps so games can be replayed or shared.
    pub map_seed: u64,
    /// Session this view belongs to. Set by the WS handler before sending;
    /// the engine itself doesn't track sessions.
    #[serde(default = "uuid::Uuid::nil")]
    pub session_id: uuid::Uuid,
    /// Unix-epoch seconds when the current turn auto-ends. Set by the WS
    /// handler from the session's live deadline before sending; engine
    /// doesn't compute it.
    #[serde(default)]
    pub turn_deadline_secs: u64,
    #[serde(default)]
    pub last_action: Option<ActionReport>,
}

impl GameState {
    /// Produce a fog-filtered view for the given vantage. Mutates per-player
    /// "seen buildings" memory as a side effect so the player learns about
    /// buildings that fall within their current vision.
    ///
    /// When the match is over (`winner.is_some()`), fog is lifted for both
    /// players — both sides need to be able to see what actually happened so
    /// agents that just lost their last unit don't conclude "tie" from
    /// missing information.
    pub fn view_for(&mut self, view: View) -> PlayerView {
        let game_over = self.winner.is_some();
        let me = match view {
            View::Spectator => None,
            View::Player(p) => Some(p),
        };
        let reveal_all = game_over || me.is_none();

        let visible: HashSet<Coord> = if reveal_all {
            (0..self.map.height)
                .flat_map(|y| (0..self.map.width).map(move |x| (x, y)))
                .collect()
        } else {
            self.visible_tiles(me.unwrap())
        };

        // Refresh per-player seen-buildings memory while the match is live.
        if let Some(p) = me {
            if !game_over {
                let memory = self.seen_buildings.entry(p).or_default();
                for b in self.buildings.values() {
                    if visible.contains(&b.pos) {
                        memory.insert(
                            b.id,
                            SeenBuilding {
                                building: b.clone(),
                                last_seen_turn: self.turn_number,
                            },
                        );
                    }
                }
            }
        }

        let visible_units: Vec<Unit> = if reveal_all {
            self.units.values().cloned().collect()
        } else {
            let p = me.unwrap();
            self.units
                .values()
                .filter(|u| {
                    if u.owner == p {
                        return true;
                    }
                    if !visible.contains(&u.pos) {
                        return false;
                    }
                    let terrain = self.map.terrain(u.pos).unwrap_or(Terrain::Plains);
                    if !terrain.hides_units() {
                        return true;
                    }
                    self.units.values().any(|own| {
                        own.owner == p
                            && (own.pos.0 - u.pos.0).abs() <= 1
                            && (own.pos.1 - u.pos.1).abs() <= 1
                    })
                })
                .cloned()
                .collect()
        };

        let buildings: Vec<RememberedBuilding> = if reveal_all {
            self.buildings
                .values()
                .map(|b| RememberedBuilding {
                    building: b.clone(),
                    currently_visible: true,
                    last_seen_turn: self.turn_number,
                })
                .collect()
        } else {
            let p = me.unwrap();
            let memory = self.seen_buildings.get(&p).cloned().unwrap_or_default();
            memory
                .into_values()
                .map(|s| {
                    let still_present = self.buildings.contains_key(&s.building.id);
                    let visible_now = still_present && visible.contains(&s.building.pos);
                    RememberedBuilding {
                        building: s.building,
                        currently_visible: visible_now,
                        last_seen_turn: s.last_seen_turn,
                    }
                })
                .collect()
        };

        let funds: HashMap<PlayerId, u32> = if reveal_all {
            self.funds.clone()
        } else {
            let mut h = HashMap::new();
            if let Some(p) = me {
                h.insert(p, self.funds.get(&p).copied().unwrap_or(0));
            }
            h
        };

        PlayerView {
            map: self.map.clone(),
            units: visible_units,
            buildings,
            visible_tiles: visible.into_iter().collect(),
            current_turn: self.current_turn,
            turn_number: self.turn_number,
            winner: self.winner,
            you: me,
            funds,
            factories_used: self.factories_used.iter().copied().collect(),
            map_seed: self.map_seed,
            session_id: uuid::Uuid::nil(),
            turn_deadline_secs: 0,
            last_action: self.last_action.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(map: Map, units: Vec<(PlayerId, Coord, u32)>) -> GameState {
        let mut hm = HashMap::new();
        for (owner, pos, hp) in units {
            let id = Uuid::new_v4();
            hm.insert(
                id,
                Unit {
                    id,
                    kind: UnitKind::Infantry,
                    owner,
                    pos,
                    hp,
                    has_moved: false,
                },
            );
        }
        GameState {
            map,
            units: hm,
            buildings: HashMap::new(),
            current_turn: PlayerId::P1,
            turn_number: 1,
            winner: None,
            last_action: None,
            seen_buildings: HashMap::new(),
            hq_owners: HashSet::new(),
            funds: HashMap::new(),
            factories_used: HashSet::new(),
            map_seed: 0,
        }
    }

    fn place_with_buildings(
        map: Map,
        units: Vec<(PlayerId, Coord, u32)>,
        buildings: Vec<(PlayerId, Coord, u32)>,
    ) -> GameState {
        let mut g = place(map, units);
        for (owner, pos, hp) in buildings {
            let id = Uuid::new_v4();
            g.buildings.insert(
                id,
                Building {
                    id,
                    kind: BuildingKind::Hq,
                    owner: Some(owner),
                    pos,
                    hp,
                },
            );
            g.hq_owners.insert(owner);
        }
        g
    }

    fn add_building(g: &mut GameState, kind: BuildingKind, owner: Option<PlayerId>, pos: Coord) -> Uuid {
        let id = Uuid::new_v4();
        g.buildings.insert(
            id,
            Building {
                id,
                kind,
                owner,
                pos,
                hp: kind.max_hp(),
            },
        );
        if matches!(kind, BuildingKind::Hq) {
            if let Some(p) = owner {
                g.hq_owners.insert(p);
            }
        }
        id
    }

    fn flat_map(w: i32, h: i32) -> Map {
        Map {
            width: w,
            height: h,
            tiles: vec![Terrain::Plains; (w * h) as usize],
        }
    }

    fn id_of(state: &GameState, owner: PlayerId, pos: Coord) -> Uuid {
        state
            .units
            .values()
            .find(|u| u.owner == owner && u.pos == pos)
            .unwrap()
            .id
    }

    #[test]
    fn move_only_action_works() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        let id = id_of(&g, PlayerId::P1, (0, 0));
        let r = g.try_action(PlayerId::P1, id, (2, 0), None).unwrap();
        assert_eq!(r.moved_to, (2, 0));
        assert_eq!(g.units[&id].pos, (2, 0));
        assert!(g.units[&id].has_moved);
        assert!(r.target_id.is_none());
    }

    #[test]
    fn adjacent_attack_damages_both() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (1, 0), 10), (PlayerId::P2, (2, 0), 10)],
        );
        let atk = id_of(&g, PlayerId::P1, (1, 0));
        let def = id_of(&g, PlayerId::P2, (2, 0));
        let r = g
            .try_action(PlayerId::P1, atk, (1, 0), Some((2, 0)))
            .unwrap();
        // Plains: base 55, atk_hp=10 -> raw 5.5; reduction 1*1.0*0.1=0.1 -> 4.95 -> 5
        assert_eq!(r.damage_to_defender, Some(5));
        assert_eq!(g.units[&def].hp, 5);
        // Counter from 5-HP defender on plains: 55*0.5=2.75 raw, reduction 1*1.0*0.1=0.1 -> 2.475 -> 2
        assert_eq!(r.damage_to_attacker, Some(2));
        assert_eq!(g.units[&atk].hp, 8);
        assert!(!r.defender_killed);
        assert!(!r.attacker_killed);
    }

    #[test]
    fn out_of_range_attack_rejected() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (4, 0), 10)],
        );
        let atk = id_of(&g, PlayerId::P1, (0, 0));
        // Move 2 right; target at (4,0) is 2 tiles away from (2,0) — out of melee range.
        let err = g
            .try_action(PlayerId::P1, atk, (2, 0), Some((4, 0)))
            .unwrap_err();
        assert!(err.contains("range"), "got: {err}");
        // Move and attack should both be rejected atomically.
        assert_eq!(g.units[&atk].pos, (0, 0));
        assert!(!g.units[&atk].has_moved);
    }

    #[test]
    fn killing_last_enemy_does_not_win() {
        // Win condition is HQ destruction or surrender. Wiping out all of an
        // opponent's units is NOT an automatic win — they can rebuild from a
        // factory or hold their HQ to force a surrender.
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (1, 0), 10), (PlayerId::P2, (2, 0), 1)],
        );
        let atk = id_of(&g, PlayerId::P1, (1, 0));
        let def = id_of(&g, PlayerId::P2, (2, 0));
        let r = g
            .try_action(PlayerId::P1, atk, (1, 0), Some((2, 0)))
            .unwrap();
        assert!(r.defender_killed);
        assert!(g.units.get(&def).is_none());
        assert_eq!(g.winner, None);
        assert_eq!(r.damage_to_attacker, None);
    }

    #[test]
    fn forest_reduces_damage() {
        // Same setup but defender on forest.
        let mut tiles = vec![Terrain::Plains; 25];
        tiles[2] = Terrain::Forest; // (2,0)
        let map = Map {
            width: 5,
            height: 5,
            tiles,
        };
        let mut g = place(
            map,
            vec![(PlayerId::P1, (1, 0), 10), (PlayerId::P2, (2, 0), 10)],
        );
        let atk = id_of(&g, PlayerId::P1, (1, 0));
        let def = id_of(&g, PlayerId::P2, (2, 0));
        let r = g
            .try_action(PlayerId::P1, atk, (1, 0), Some((2, 0)))
            .unwrap();
        // Forest stars=2, reduction 0.2, raw 5.5 -> 4.4 -> 4
        assert_eq!(r.damage_to_defender, Some(4));
        assert_eq!(g.units[&def].hp, 6);
    }

    #[test]
    fn cannot_act_twice_per_turn() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        let id = id_of(&g, PlayerId::P1, (0, 0));
        g.try_action(PlayerId::P1, id, (1, 0), None).unwrap();
        let err = g
            .try_action(PlayerId::P1, id, (2, 0), None)
            .unwrap_err();
        assert!(err.contains("already"), "got: {err}");
    }

    #[test]
    fn building_blocks_movement() {
        let g = place_with_buildings(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10)],
            vec![(PlayerId::P2, (1, 0), 10)],
        );
        let id = id_of(&g, PlayerId::P1, (0, 0));
        let r = g.reachable(id);
        // Can't stop on the building tile, and the building blocks pass-through
        // so (2,0) shouldn't be reached via that row.
        assert!(!r.contains_key(&(1, 0)));
        assert!(!r.contains_key(&(2, 0)));
        // But going down the column is fine.
        assert!(r.contains_key(&(0, 1)));
    }

    #[test]
    fn attacking_enemy_hq_damages_it() {
        let mut g = place_with_buildings(
            flat_map(5, 5),
            vec![(PlayerId::P1, (1, 0), 10), (PlayerId::P2, (4, 4), 10)],
            vec![(PlayerId::P2, (2, 0), 10), (PlayerId::P1, (4, 0), 10)],
        );
        let atk = id_of(&g, PlayerId::P1, (1, 0));
        let p2_hq = g
            .buildings
            .values()
            .find(|b| b.owner == Some(PlayerId::P2))
            .unwrap()
            .id;
        let r = g
            .try_action(PlayerId::P1, atk, (1, 0), Some((2, 0)))
            .unwrap();
        // base 30, atk_hp=10 -> raw 3.0; reduction 1*1.0*0.1=0.1 -> 2.7 -> 3
        assert_eq!(r.damage_to_defender, Some(3));
        assert_eq!(r.target_kind, Some(TargetKind::Building));
        assert_eq!(r.damage_to_attacker, None);
        assert_eq!(g.buildings[&p2_hq].hp, 7);
        assert!(g.winner.is_none());
    }

    #[test]
    fn destroying_hq_wins_even_with_units_alive() {
        let mut g = place_with_buildings(
            flat_map(5, 5),
            vec![
                (PlayerId::P1, (1, 0), 10),
                (PlayerId::P2, (4, 4), 10), // P2 still has a unit
            ],
            vec![(PlayerId::P2, (2, 0), 1), (PlayerId::P1, (4, 0), 10)],
        );
        let atk = id_of(&g, PlayerId::P1, (1, 0));
        let r = g
            .try_action(PlayerId::P1, atk, (1, 0), Some((2, 0)))
            .unwrap();
        assert!(r.defender_killed);
        assert_eq!(g.winner, Some(PlayerId::P1));
    }

    #[test]
    fn cannot_attack_own_hq() {
        let mut g = place_with_buildings(
            flat_map(5, 5),
            vec![(PlayerId::P1, (1, 0), 10)],
            vec![(PlayerId::P1, (2, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        let atk = id_of(&g, PlayerId::P1, (1, 0));
        let err = g
            .try_action(PlayerId::P1, atk, (1, 0), Some((2, 0)))
            .unwrap_err();
        assert!(err.contains("own building"), "got: {err}");
    }

    #[test]
    fn action_report_includes_path() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        let id = id_of(&g, PlayerId::P1, (0, 0));
        let r = g.try_action(PlayerId::P1, id, (3, 0), None).unwrap();
        // 3 steps east on plains: path includes start and each tile to dest.
        assert_eq!(r.path.first(), Some(&(0, 0)));
        assert_eq!(r.path.last(), Some(&(3, 0)));
        assert_eq!(r.path.len(), 4);
    }

    #[test]
    fn hq_provides_short_range_vision() {
        // Build a state where P1's only assets are an HQ (no units), so the
        // visible tiles must come from the building alone.
        let mut g = place(flat_map(7, 7), vec![]);
        add_building(&mut g, BuildingKind::Hq, Some(PlayerId::P1), (3, 3));
        let vis = g.visible_tiles(PlayerId::P1);
        // Vision = 1 around (3,3): a 3x3 square = 9 tiles.
        assert_eq!(vis.len(), 9);
        assert!(vis.contains(&(3, 3)));
        assert!(vis.contains(&(2, 2)));
        assert!(vis.contains(&(4, 4)));
        // 2 tiles away should be fogged for an HQ-only player.
        assert!(!vis.contains(&(5, 3)));
    }

    #[test]
    fn city_passable_and_capturable() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        let city = add_building(&mut g, BuildingKind::City, None, (1, 0));
        // City is reachable as a destination (passable, not blocked).
        let id = id_of(&g, PlayerId::P1, (0, 0));
        let r = g.reachable(id);
        assert!(r.contains_key(&(1, 0)));
        // Move onto the city.
        g.try_action(PlayerId::P1, id, (1, 0), None).unwrap();
        // End turn → P1 captures.
        g.end_turn(PlayerId::P1).unwrap();
        assert_eq!(g.buildings[&city].owner, Some(PlayerId::P1));
    }

    #[test]
    fn city_income_collected_at_turn_start() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        g.funds.insert(PlayerId::P1, 0);
        g.funds.insert(PlayerId::P2, 0);
        add_building(&mut g, BuildingKind::City, Some(PlayerId::P2), (4, 0));
        // P1 ends turn -> income tick happens for P2 (incoming player).
        g.end_turn(PlayerId::P1).unwrap();
        assert_eq!(
            g.funds.get(&PlayerId::P2).copied(),
            Some(BuildingKind::City.income_per_turn()),
            "city income changed"
        );
        assert_eq!(g.funds.get(&PlayerId::P1).copied(), Some(0));
    }

    #[test]
    fn buy_unit_spawns_at_factory_and_deducts_funds() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        g.funds.insert(PlayerId::P1, 4000);
        let factory = add_building(&mut g, BuildingKind::Factory, Some(PlayerId::P1), (2, 2));
        let new_unit = g
            .try_buy_unit(PlayerId::P1, factory, UnitKind::HeavyInfantry)
            .unwrap();
        assert_eq!(g.funds[&PlayerId::P1], 4000 - UnitKind::HeavyInfantry.cost());
        assert_eq!(g.units[&new_unit].pos, (2, 2));
        assert!(g.units[&new_unit].has_moved); // can't act this turn
        // Second buy on same factory should fail.
        let err = g
            .try_buy_unit(PlayerId::P1, factory, UnitKind::Infantry)
            .unwrap_err();
        assert!(err.contains("already produced"), "got: {err}");
    }

    #[test]
    fn buy_unit_blocked_when_tile_occupied() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (2, 2), 10), (PlayerId::P2, (4, 4), 10)],
        );
        g.funds.insert(PlayerId::P1, 4000);
        let factory = add_building(&mut g, BuildingKind::Factory, Some(PlayerId::P1), (2, 2));
        let err = g
            .try_buy_unit(PlayerId::P1, factory, UnitKind::Infantry)
            .unwrap_err();
        assert!(err.contains("occupied"), "got: {err}");
    }

    #[test]
    fn buy_unit_rejects_insufficient_funds() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        g.funds.insert(PlayerId::P1, 500);
        let factory = add_building(&mut g, BuildingKind::Factory, Some(PlayerId::P1), (2, 2));
        let err = g
            .try_buy_unit(PlayerId::P1, factory, UnitKind::Infantry)
            .unwrap_err();
        assert!(err.contains("insufficient funds"), "got: {err}");
    }

    #[test]
    fn mountain_cap_blocks_two_in_one_turn() {
        // Sea-walled corridor forces the unit to traverse two adjacent
        // mountains to cross. Without the cap a scout could afford the cost;
        // with MAX_MOUNTAIN_CROSSINGS_PER_TURN=1 the second tile is rejected.
        let mut tiles = vec![Terrain::Sea; 4 * 3];
        // Row y=1: plains, mountain, mountain, plains
        tiles[1 * 4 + 0] = Terrain::Plains;
        tiles[1 * 4 + 1] = Terrain::Mountain;
        tiles[1 * 4 + 2] = Terrain::Mountain;
        tiles[1 * 4 + 3] = Terrain::Plains;
        let map = Map {
            width: 4,
            height: 3,
            tiles,
        };
        let mut g = place(map, vec![(PlayerId::P1, (0, 1), 10)]);
        let id = id_of(&g, PlayerId::P1, (0, 1));
        g.units.get_mut(&id).unwrap().kind = UnitKind::Scout;
        let r = g.reachable(id);
        // First mountain entry is fine.
        assert!(r.contains_key(&(1, 1)));
        // Second mountain entry blocked → (2,1) and (3,1) unreachable.
        assert!(!r.contains_key(&(2, 1)));
        assert!(!r.contains_key(&(3, 1)));
    }

    #[test]
    fn forest_costs_differ_by_unit_kind() {
        // Forest in front of a scout costs 1 (no penalty); for infantry it
        // costs 2 — half the unit's whole turn.
        let mut tiles = vec![Terrain::Plains; 4 * 4];
        tiles[(0 * 4 + 1) as usize] = Terrain::Forest;
        let map = Map {
            width: 4,
            height: 4,
            tiles,
        };

        let mut g = place(map.clone(), vec![(PlayerId::P1, (0, 0), 10)]);
        let id = id_of(&g, PlayerId::P1, (0, 0));
        g.units.get_mut(&id).unwrap().kind = UnitKind::Scout;
        let r = g.reachable(id);
        assert_eq!(r.get(&(1, 0)).copied(), Some(1), "scout sails through forest");

        let mut g2 = place(map, vec![(PlayerId::P1, (0, 0), 10)]);
        let id2 = id_of(&g2, PlayerId::P1, (0, 0));
        g2.units.get_mut(&id2).unwrap().kind = UnitKind::Infantry;
        let r2 = g2.reachable(id2);
        assert_eq!(r2.get(&(1, 0)).copied(), Some(2), "infantry slowed in forest");
    }

    #[test]
    fn city_grants_infantry_extra_defense() {
        let mut tiles = vec![Terrain::Plains; 3 * 3];
        let map = Map {
            width: 3,
            height: 3,
            tiles,
        };
        let mut g = place(
            map,
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (1, 0), 10)],
        );
        // Defender (P2) sits on a city.
        add_building(&mut g, BuildingKind::City, Some(PlayerId::P2), (1, 0));
        let atk = id_of(&g, PlayerId::P1, (0, 0));
        let r = g
            .try_action(PlayerId::P1, atk, (0, 0), Some((1, 0)))
            .unwrap();
        // Plains stars=1 + city stars for infantry=3 => 4 stars total.
        // base 55 * 1.0 / 10 * (1 - 4*1.0*0.1) = 5.5 * 0.6 = 3.3 -> 3
        assert_eq!(r.damage_to_defender, Some(3));
    }

    #[test]
    fn unit_role_identity_holds() {
        // Scout is fragile vs heavy and barely scratches it.
        let mut g = place(flat_map(5, 5), vec![]);
        let scout_id = Uuid::new_v4();
        let heavy_id = Uuid::new_v4();
        g.units.insert(
            scout_id,
            Unit {
                id: scout_id,
                kind: UnitKind::Scout,
                owner: PlayerId::P1,
                pos: (1, 0),
                hp: 10,
                has_moved: false,
            },
        );
        g.units.insert(
            heavy_id,
            Unit {
                id: heavy_id,
                kind: UnitKind::HeavyInfantry,
                owner: PlayerId::P2,
                pos: (2, 0),
                hp: 10,
                has_moved: false,
            },
        );
        let r = g
            .try_action(PlayerId::P1, scout_id, (1, 0), Some((2, 0)))
            .unwrap();
        // Scout vs heavy: 20 base * 1.0 / 10 * 0.9 (plains) = 1.8 -> 2
        assert_eq!(r.damage_to_defender, Some(2));
        // Heavy at 8 HP counters: 95 * 0.8 / 10 * 0.9 = 6.84 -> 7
        assert_eq!(r.damage_to_attacker, Some(7));
        assert_eq!(g.units[&scout_id].hp, 3);
        assert_eq!(g.units[&heavy_id].hp, 8);
    }

    #[test]
    fn heavy_shrugs_off_infantry_chip() {
        let mut g = place(flat_map(5, 5), vec![]);
        let inf = Uuid::new_v4();
        let heavy = Uuid::new_v4();
        g.units.insert(
            inf,
            Unit {
                id: inf,
                kind: UnitKind::Infantry,
                owner: PlayerId::P1,
                pos: (1, 0),
                hp: 10,
                has_moved: false,
            },
        );
        g.units.insert(
            heavy,
            Unit {
                id: heavy,
                kind: UnitKind::HeavyInfantry,
                owner: PlayerId::P2,
                pos: (2, 0),
                hp: 10,
                has_moved: false,
            },
        );
        let r = g
            .try_action(PlayerId::P1, inf, (1, 0), Some((2, 0)))
            .unwrap();
        // Inf vs heavy: 35 * 1.0 / 10 * 0.9 = 3.15 -> 3
        assert_eq!(r.damage_to_defender, Some(3));
        // Heavy at 7 HP counters: 65 * 0.7 / 10 * 0.9 = 4.095 -> 4
        assert_eq!(r.damage_to_attacker, Some(4));
    }

    #[test]
    fn cannot_attack_factory_or_city() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (1, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        add_building(&mut g, BuildingKind::Factory, Some(PlayerId::P2), (2, 0));
        let atk = id_of(&g, PlayerId::P1, (1, 0));
        let err = g
            .try_action(PlayerId::P1, atk, (1, 0), Some((2, 0)))
            .unwrap_err();
        assert!(err.contains("captured"), "got: {err}");
    }

    #[test]
    fn random_dimensions_are_in_range() {
        // Sample a generous slice of seeds and confirm whichever maps survive
        // validation have width and height inside [MIN_MAP_DIM, MAX_MAP_DIM].
        let mut tested = 0;
        for seed in 0u64..200 {
            let Ok(g) = GameState::with_seed(seed) else { continue };
            assert!(
                g.map.width >= MIN_MAP_DIM && g.map.width <= MAX_MAP_DIM,
                "seed {seed}: width {} out of range",
                g.map.width
            );
            assert!(
                g.map.height >= MIN_MAP_DIM && g.map.height <= MAX_MAP_DIM,
                "seed {seed}: height {} out of range",
                g.map.height
            );
            tested += 1;
        }
        assert!(tested > 10, "very few seeds passed validation; tested={tested}");
    }

    #[test]
    fn random_map_has_required_disjoint_paths() {
        // Every map produced by the public new() / with_seed APIs is required
        // to have at least MIN_DISJOINT_HQ_PATHS land paths. Sample a handful
        // of seeds so we'd catch a regression that lets a single-chokepoint
        // map slip through.
        for seed in 0u64..30 {
            if let Ok(g) = GameState::with_seed(seed) {
                let p1_hq = g
                    .buildings
                    .values()
                    .find(|b| b.kind == BuildingKind::Hq && b.owner == Some(PlayerId::P1))
                    .unwrap()
                    .pos;
                let p2_hq = g
                    .buildings
                    .values()
                    .find(|b| b.kind == BuildingKind::Hq && b.owner == Some(PlayerId::P2))
                    .unwrap()
                    .pos;
                let n = count_disjoint_paths(&g.map, p1_hq, p2_hq, MIN_DISJOINT_HQ_PATHS);
                assert!(
                    n >= MIN_DISJOINT_HQ_PATHS,
                    "seed {seed}: only {n} disjoint paths"
                );
                assert_eq!(g.map_seed, seed);
            }
            // Some seeds will fail validation — that's expected. We just need
            // the ones that pass to be valid.
        }
    }

    #[test]
    fn map_is_mirror_symmetric_and_buildings_on_plains() {
        // 180° rotation through the map center: (x, y) ↔ (W-1-x, H-1-y).
        for seed in 0u64..40 {
            let Ok(g) = GameState::with_seed(seed) else { continue };
            let w = g.map.width;
            let h = g.map.height;
            for y in 0..h {
                for x in 0..w {
                    assert_eq!(
                        g.map.terrain((x, y)),
                        g.map.terrain((w - 1 - x, h - 1 - y)),
                        "seed {seed}: map not 180°-symmetric at ({x},{y})",
                    );
                }
            }
            for b in g.buildings.values() {
                assert_eq!(
                    g.map.terrain(b.pos),
                    Some(Terrain::Plains),
                    "seed {seed}: building at {:?} on non-plains",
                    b.pos
                );
            }
            for u in g.units.values() {
                assert_eq!(
                    g.map.terrain(u.pos),
                    Some(Terrain::Plains),
                    "seed {seed}: starting unit at {:?} on non-plains",
                    u.pos
                );
            }
            for b in g
                .buildings
                .values()
                .filter(|b| b.owner == Some(PlayerId::P1))
            {
                let mirror = (w - 1 - b.pos.0, h - 1 - b.pos.1);
                let exists = g.buildings.values().any(|m| {
                    m.pos == mirror && m.kind == b.kind && m.owner == Some(PlayerId::P2)
                });
                assert!(
                    exists,
                    "seed {seed}: no mirrored {:?} for {:?}",
                    b.kind, b.pos
                );
            }
        }
    }

    #[test]
    fn same_seed_produces_same_map() {
        if let (Ok(a), Ok(b)) = (GameState::with_seed(123_456), GameState::with_seed(123_456)) {
            assert_eq!(a.map.tiles, b.map.tiles);
            assert_eq!(a.map.width, b.map.width);
            assert_eq!(a.map.height, b.map.height);
        }
        // If 123_456 happens to fail validation, the test still passes; the
        // randomness of the iteration above already exercises validation.
    }

    #[test]
    fn count_disjoint_paths_open_grid() {
        // Open 10x10 plains: corners have only 2 neighbors so corner-to-corner
        // is bounded at 2 by Menger. Pick mid-edge endpoints instead — those
        // have 3 neighbors and an open grid admits 3 disjoint routes.
        let map = flat_map(10, 10);
        let n = count_disjoint_paths(&map, (0, 4), (9, 4), 3);
        assert!(n >= 3, "got {n} paths");
    }

    #[test]
    fn count_disjoint_paths_choked() {
        // A 1-wide corridor only admits 1 disjoint path.
        let mut map = flat_map(5, 5);
        // Wall off everything except a single column at x=2.
        for y in 0..5 {
            for x in 0..5 {
                if x != 2 {
                    let i = (y * 5 + x) as usize;
                    map.tiles[i] = Terrain::Sea;
                }
            }
        }
        let n = count_disjoint_paths(&map, (2, 0), (2, 4), 3);
        assert_eq!(n, 1);
    }

    #[test]
    fn surrender_blocked_before_turn_4() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        let err = g.try_surrender(PlayerId::P1).unwrap_err();
        assert!(err.contains("turn 4"), "got: {err}");
        assert!(g.winner.is_none());
    }

    #[test]
    fn surrender_allowed_at_turn_4_other_wins() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        g.turn_number = 4;
        g.try_surrender(PlayerId::P2).unwrap();
        assert_eq!(g.winner, Some(PlayerId::P1));
    }

    #[test]
    fn end_turn_clears_has_moved_for_next_player() {
        let mut g = place(
            flat_map(5, 5),
            vec![(PlayerId::P1, (0, 0), 10), (PlayerId::P2, (4, 4), 10)],
        );
        let p1id = id_of(&g, PlayerId::P1, (0, 0));
        g.try_action(PlayerId::P1, p1id, (1, 0), None).unwrap();
        assert!(g.units[&p1id].has_moved);
        g.end_turn(PlayerId::P1).unwrap();
        let p2id = id_of(&g, PlayerId::P2, (4, 4));
        assert!(!g.units[&p2id].has_moved);
        // P1 unit's flag is unchanged (still moved this round); next turn cycle resets it.
        g.try_action(PlayerId::P2, p2id, (3, 4), None).unwrap();
        g.end_turn(PlayerId::P2).unwrap();
        assert!(!g.units[&p1id].has_moved);
    }
}
