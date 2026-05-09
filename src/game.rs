use serde::{Deserialize, Serialize};
use std::collections::{BinaryHeap, HashMap, HashSet};
use uuid::Uuid;

pub type Coord = (i32, i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Terrain {
    Plains,
    Forest,
    Mountain,
    Sea,
}

impl Terrain {
    /// Movement cost for infantry. None = impassable.
    pub fn infantry_move_cost(self) -> Option<u32> {
        match self {
            Terrain::Plains => Some(1),
            Terrain::Forest => Some(1),
            Terrain::Mountain => Some(2),
            Terrain::Sea => None,
        }
    }

    /// Defense stars (Advance Wars-style; higher = more damage reduction).
    pub fn defense(self) -> u32 {
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
#[serde(rename_all = "camelCase")]
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
    /// Cost to produce at a factory.
    pub fn cost(self) -> u32 {
        match self {
            UnitKind::Infantry => 1000,
            UnitKind::Scout => 3000,
            UnitKind::HeavyInfantry => 2500,
        }
    }
    /// All current unit kinds are infantry-class and can capture buildings.
    pub fn can_capture(self) -> bool {
        true
    }
    /// Base damage % out of 100 against a given defender (Advance Wars-style table).
    pub fn base_damage(self, target: UnitKind) -> Option<u32> {
        use UnitKind::*;
        Some(match (self, target) {
            (Infantry,      Infantry)      => 55,
            (Infantry,      Scout)         => 60,
            (Infantry,      HeavyInfantry) => 45,
            (Scout,         Infantry)      => 70,
            (Scout,         Scout)         => 35,
            (Scout,         HeavyInfantry) => 55,
            (HeavyInfantry, Infantry)      => 65,
            (HeavyInfantry, Scout)         => 85,
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
            BuildingKind::Hq => 1000,
            BuildingKind::Factory => 1000,
            BuildingKind::City => 1000,
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

/// Build a small demo map for MVP testing.
pub fn demo_map() -> Map {
    use Terrain::*;
    let w = 12;
    let h = 10;
    // P = plains, F = forest, M = mountain, ~ = sea
    // A small central lake forces infantry to flow around it but doesn't
    // cleave the map, so the two armies can engage.
    #[rustfmt::skip]
    let layout = [
        "PPPPPPPPPPPP",
        "PPFFPPPPMMPP",
        "PPFFPPPPMMPP",
        "PPPPPPPPPPPP",
        "PPPPP~~PPPPP",
        "PPPPP~~PPPPP",
        "PPPPPPPPPPPP",
        "PPMMPPPPFFPP",
        "PPMMPPPPFFPP",
        "PPPPPPPPPPPP",
    ];
    let tiles: Vec<Terrain> = layout
        .iter()
        .flat_map(|row| {
            row.chars().map(|c| match c {
                'P' => Plains,
                'F' => Forest,
                'M' => Mountain,
                '~' => Sea,
                _ => Plains,
            })
        })
        .collect();
    assert_eq!(tiles.len(), (w * h) as usize);
    Map {
        width: w,
        height: h,
        tiles,
    }
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
    pub fn new() -> Self {
        let map = demo_map();
        let mut units = HashMap::new();
        let starts = [
            (PlayerId::P1, (2, 2)),
            (PlayerId::P1, (3, 2)),
            (PlayerId::P1, (4, 2)),
            (PlayerId::P2, (7, 7)),
            (PlayerId::P2, (8, 7)),
            (PlayerId::P2, (9, 7)),
        ];
        for (owner, pos) in starts {
            let id = Uuid::new_v4();
            units.insert(
                id,
                Unit {
                    id,
                    kind: UnitKind::Infantry,
                    owner,
                    pos,
                    hp: UnitKind::Infantry.max_hp(),
                    has_moved: false,
                },
            );
        }
        let mut buildings = HashMap::new();
        let add = |kind: BuildingKind, owner: Option<PlayerId>, pos: Coord, map_ref: &mut HashMap<Uuid, Building>| {
            let id = Uuid::new_v4();
            map_ref.insert(
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
        add(BuildingKind::Hq, Some(PlayerId::P1), (3, 1), &mut buildings);
        add(BuildingKind::Hq, Some(PlayerId::P2), (8, 8), &mut buildings);
        add(BuildingKind::Factory, Some(PlayerId::P1), (4, 1), &mut buildings);
        add(BuildingKind::Factory, Some(PlayerId::P2), (7, 8), &mut buildings);
        // Four neutral cities scattered roughly symmetrically — race to
        // capture them for income.
        for pos in [(1, 4), (10, 5), (5, 3), (6, 6)] {
            add(BuildingKind::City, None, pos, &mut buildings);
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
        }
    }

    pub fn building_at(&self, c: Coord) -> Option<&Building> {
        self.buildings.values().find(|b| b.pos == c)
    }

    pub fn unit_at(&self, c: Coord) -> Option<&Unit> {
        self.units.values().find(|u| u.pos == c)
    }

    /// Compute reachable tiles for a unit using Dijkstra over terrain costs.
    /// Cannot pass through enemy units. Cannot stop on a tile occupied by another unit.
    pub fn reachable(&self, unit_id: Uuid) -> HashMap<Coord, u32> {
        let Some(unit) = self.units.get(&unit_id) else {
            return HashMap::new();
        };
        let mp = unit.kind.move_points();

        // Coord -> best cost to reach
        let mut best: HashMap<Coord, u32> = HashMap::new();
        best.insert(unit.pos, 0);

        // Min-heap on cost
        let mut heap: BinaryHeap<std::cmp::Reverse<(u32, Coord)>> = BinaryHeap::new();
        heap.push(std::cmp::Reverse((0, unit.pos)));

        while let Some(std::cmp::Reverse((cost, pos))) = heap.pop() {
            if cost > *best.get(&pos).unwrap_or(&u32::MAX) {
                continue;
            }
            for n in neighbors4(pos) {
                let Some(terrain) = self.map.terrain(n) else {
                    continue;
                };
                let Some(step) = terrain.infantry_move_cost() else {
                    continue;
                };
                // Block movement through enemy units.
                if let Some(other) = self.unit_at(n) {
                    if other.owner != unit.owner {
                        continue;
                    }
                }
                // HQs block movement; factories and cities are passable.
                if let Some(b) = self.building_at(n) {
                    if b.kind.blocks_movement() {
                        continue;
                    }
                }
                let new_cost = cost + step;
                if new_cost > mp {
                    continue;
                }
                if new_cost < *best.get(&n).unwrap_or(&u32::MAX) {
                    best.insert(n, new_cost);
                    heap.push(std::cmp::Reverse((new_cost, n)));
                }
            }
        }

        // Filter out tiles occupied by other units or by a movement-blocking
        // building (HQ). Cities/factories are valid destinations because units
        // can stand on them to capture or to spawn from.
        best.retain(|&pos, _| {
            pos == unit.pos
                || (self.unit_at(pos).is_none()
                    && self
                        .building_at(pos)
                        .map_or(true, |b| !b.kind.blocks_movement()))
        });
        best
    }

    /// Reconstruct the cheapest path the unit would walk to `dest` using the
    /// same constraints as `reachable`. Returns the path including start and
    /// end positions, or `None` if `dest` isn't actually reachable.
    pub fn compute_path(&self, unit_id: Uuid, dest: Coord) -> Option<Vec<Coord>> {
        let unit = self.units.get(&unit_id)?;
        let mp = unit.kind.move_points();
        let start = unit.pos;
        if dest == start {
            return Some(vec![start]);
        }

        let mut best: HashMap<Coord, u32> = HashMap::new();
        let mut parent: HashMap<Coord, Coord> = HashMap::new();
        best.insert(start, 0);
        let mut heap: BinaryHeap<std::cmp::Reverse<(u32, Coord)>> = BinaryHeap::new();
        heap.push(std::cmp::Reverse((0, start)));

        while let Some(std::cmp::Reverse((cost, pos))) = heap.pop() {
            if cost > *best.get(&pos).unwrap_or(&u32::MAX) {
                continue;
            }
            for n in neighbors4(pos) {
                let Some(terrain) = self.map.terrain(n) else {
                    continue;
                };
                let Some(step) = terrain.infantry_move_cost() else {
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
                let new_cost = cost + step;
                if new_cost > mp {
                    continue;
                }
                if new_cost < *best.get(&n).unwrap_or(&u32::MAX) {
                    best.insert(n, new_cost);
                    parent.insert(n, pos);
                    heap.push(std::cmp::Reverse((new_cost, n)));
                }
            }
        }

        if !best.contains_key(&dest) {
            return None;
        }
        let mut path = vec![dest];
        let mut cur = dest;
        while let Some(&p) = parent.get(&cur) {
            path.push(p);
            cur = p;
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
                let defender_terrain =
                    self.map.terrain(defender.pos).unwrap_or(Terrain::Plains);
                let dmg = compute_damage(&attacker, defender_terrain, &defender);
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
                        let attacker_terrain =
                            self.map.terrain(dest).unwrap_or(Terrain::Plains);
                        let counter = compute_damage(
                            &defender,
                            attacker_terrain,
                            &self.units[&unit_id],
                        );
                        report.damage_to_attacker = Some(counter);
                        let atk = self.units.get_mut(&unit_id).unwrap();
                        atk.hp = atk.hp.saturating_sub(counter);
                        if atk.hp == 0 {
                            self.units.remove(&unit_id);
                            report.attacker_killed = true;
                        }
                    }
                }
            }
            Some(AttackTarget::Building(target_id)) => {
                let attacker = self.units[&unit_id].clone();
                let bld = self.buildings[&target_id].clone();
                let bld_terrain = self.map.terrain(bld.pos).unwrap_or(Terrain::Plains);
                let dmg = compute_damage_vs_building(&attacker, bld_terrain, &bld);
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
/// `base_damage_vs_building` and the building's HP for terrain reduction
/// scaling.
pub fn compute_damage_vs_building(
    attacker: &Unit,
    target_terrain: Terrain,
    target: &Building,
) -> u32 {
    let base = attacker.kind.base_damage_vs_building(target.kind) as f32;
    let max_hp = UnitKind::max_hp(attacker.kind) as f32;
    let atk_hp_ratio = attacker.hp as f32 / max_hp;
    let raw = base * atk_hp_ratio / 10.0;
    let def_stars = target_terrain.defense() as f32;
    let def_hp_ratio = target.hp as f32 / target.kind.max_hp() as f32;
    let reduction = (def_stars * 0.1 * def_hp_ratio).clamp(0.0, 0.9);
    let final_dmg = raw * (1.0 - reduction);
    final_dmg.round().max(0.0) as u32
}

/// Compute damage in HP points an attacker deals to a defender on the given terrain.
/// Simplified Advance Wars formula: scaled base damage × attacker HP, reduced by
/// defender terrain stars × defender HP ratio.
pub fn compute_damage(attacker: &Unit, defender_terrain: Terrain, defender: &Unit) -> u32 {
    let Some(base) = attacker.kind.base_damage(defender.kind) else {
        return 0;
    };
    let max_hp = UnitKind::max_hp(attacker.kind) as f32;
    let atk_hp_ratio = attacker.hp as f32 / max_hp;
    // Raw HP damage out of 10.
    let raw = base as f32 * atk_hp_ratio / 10.0;
    let def_stars = defender_terrain.defense() as f32;
    let def_hp_ratio = defender.hp as f32 / max_hp;
    let reduction = (def_stars * 0.1 * def_hp_ratio).clamp(0.0, 0.9);
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
        assert_eq!(g.funds.get(&PlayerId::P2).copied(), Some(1000));
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
        assert_eq!(g.funds[&PlayerId::P1], 4000 - 2500);
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
