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
#[serde(rename_all = "lowercase")]
pub enum UnitKind {
    Infantry,
}

impl UnitKind {
    pub fn move_points(self) -> u32 {
        match self {
            UnitKind::Infantry => 3,
        }
    }
    pub fn vision(self) -> i32 {
        match self {
            UnitKind::Infantry => 2,
        }
    }
    pub fn max_hp(self) -> u32 {
        10
    }
    /// Direct combat range in Manhattan tiles. (Indirect units come later.)
    pub fn attack_range(self) -> (i32, i32) {
        match self {
            UnitKind::Infantry => (1, 1),
        }
    }
    /// Base damage % out of 100 against a given defender. Drives Advance Wars-style
    /// matchup tables. Returns None for impossible matchups.
    pub fn base_damage(self, target: UnitKind) -> Option<u32> {
        match (self, target) {
            (UnitKind::Infantry, UnitKind::Infantry) => Some(55),
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
pub struct GameState {
    pub map: Map,
    pub units: HashMap<Uuid, Unit>,
    pub current_turn: PlayerId,
    pub turn_number: u32,
    pub winner: Option<PlayerId>,
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
        Self {
            map,
            units,
            current_turn: PlayerId::P1,
            turn_number: 1,
            winner: None,
        }
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

        // Filter out tiles occupied by other units (can't stop there) but keep origin.
        best.retain(|&pos, _| pos == unit.pos || self.unit_at(pos).is_none());
        best
    }

    /// Move a unit and optionally attack an adjacent enemy.
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
        let unit = self.units.get(&unit_id).ok_or("unit not found")?;
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
        if dest != unit.pos && self.unit_at(dest).is_some() {
            return Err("destination occupied".into());
        }

        // Resolve attack target before mutating, so we can validate range and ownership.
        let attack_outcome = if let Some(target_pos) = attack {
            let target = self
                .unit_at(target_pos)
                .ok_or("no unit at attack target")?
                .clone();
            if target.owner == actor {
                return Err("cannot attack your own unit".into());
            }
            let manhattan = (dest.0 - target_pos.0).abs() + (dest.1 - target_pos.1).abs();
            let (min_r, max_r) = unit.kind.attack_range();
            if manhattan < min_r || manhattan > max_r {
                return Err("target out of range".into());
            }
            Some((target.id, target_pos))
        } else {
            None
        };

        // Apply move.
        let unit = self.units.get_mut(&unit_id).unwrap();
        unit.pos = dest;
        unit.has_moved = true;
        let attacker_kind = unit.kind;
        let attacker_owner = unit.owner;

        let mut report = ActionReport {
            unit_id,
            moved_to: dest,
            damage_to_defender: None,
            damage_to_attacker: None,
            defender_killed: false,
            attacker_killed: false,
            target_id: None,
        };

        if let Some((target_id, target_pos)) = attack_outcome {
            // Read attacker snapshot.
            let attacker = self.units.get(&unit_id).unwrap().clone();
            let defender_terrain = self.map.terrain(target_pos).unwrap_or(Terrain::Plains);
            let dmg = compute_damage(&attacker, defender_terrain, &self.units[&target_id]);
            report.target_id = Some(target_id);
            report.damage_to_defender = Some(dmg);

            // Apply to defender.
            let defender = self.units.get_mut(&target_id).unwrap();
            defender.hp = defender.hp.saturating_sub(dmg);
            let defender_dead = defender.hp == 0;
            report.defender_killed = defender_dead;

            if defender_dead {
                self.units.remove(&target_id);
            } else {
                // Counterattack: defender hits back if still in range from where they stand.
                let defender = self.units[&target_id].clone();
                let (min_r, max_r) = defender.kind.attack_range();
                let manhattan = (defender.pos.0 - dest.0).abs() + (defender.pos.1 - dest.1).abs();
                if manhattan >= min_r && manhattan <= max_r {
                    let attacker_terrain = self.map.terrain(dest).unwrap_or(Terrain::Plains);
                    let counter =
                        compute_damage(&defender, attacker_terrain, &self.units[&unit_id]);
                    report.damage_to_attacker = Some(counter);
                    let attacker_mut = self.units.get_mut(&unit_id).unwrap();
                    attacker_mut.hp = attacker_mut.hp.saturating_sub(counter);
                    if attacker_mut.hp == 0 {
                        self.units.remove(&unit_id);
                        report.attacker_killed = true;
                    }
                }
            }

            // Silence unused-binding warnings if matchup table grows.
            let _ = (attacker_kind, attacker_owner);
        }

        // Rout check: a side with no remaining units loses.
        let p1_alive = self.units.values().any(|u| u.owner == PlayerId::P1);
        let p2_alive = self.units.values().any(|u| u.owner == PlayerId::P2);
        if !p1_alive && !p2_alive {
            // Tie shouldn't really happen in MVP, but call it for the current player.
            self.winner = Some(self.current_turn);
        } else if !p1_alive {
            self.winner = Some(PlayerId::P2);
        } else if !p2_alive {
            self.winner = Some(PlayerId::P1);
        }

        Ok(report)
    }


    pub fn end_turn(&mut self, actor: PlayerId) -> Result<(), String> {
        if actor != self.current_turn {
            return Err("not your turn".into());
        }
        self.current_turn = actor.other();
        if self.current_turn == PlayerId::P1 {
            self.turn_number += 1;
        }
        for u in self.units.values_mut() {
            if u.owner == self.current_turn {
                u.has_moved = false;
            }
        }
        Ok(())
    }

    /// Tiles visible to a player given their units' vision and terrain.
    pub fn visible_tiles(&self, player: PlayerId) -> HashSet<Coord> {
        let mut vis = HashSet::new();
        for u in self.units.values().filter(|u| u.owner == player) {
            // A unit always sees its own tile.
            vis.insert(u.pos);
            let terrain = self.map.terrain(u.pos).unwrap_or(Terrain::Plains);
            let r = u.kind.vision() + terrain.vision_bonus();
            for dy in -r..=r {
                for dx in -r..=r {
                    let c = (u.pos.0 + dx, u.pos.1 + dy);
                    if !self.map.in_bounds(c) {
                        continue;
                    }
                    // Chebyshev distance check (already implicit), just bounds.
                    vis.insert(c);
                }
            }
        }
        vis
    }
}

fn neighbors4((x, y): Coord) -> [Coord; 4] {
    [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)]
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionReport {
    pub unit_id: Uuid,
    pub moved_to: Coord,
    pub target_id: Option<Uuid>,
    pub damage_to_defender: Option<u32>,
    pub damage_to_attacker: Option<u32>,
    pub defender_killed: bool,
    pub attacker_killed: bool,
}

/// View of the world from a particular vantage point.
/// `Spectator` sees everything; player views are fog-filtered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum View {
    Player(PlayerId),
    Spectator,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerView {
    pub map: Map,
    pub units: Vec<Unit>,
    pub visible_tiles: Vec<Coord>,
    pub current_turn: PlayerId,
    pub turn_number: u32,
    pub winner: Option<PlayerId>,
    pub you: Option<PlayerId>,
}

impl GameState {
    /// Produce a fog-filtered view for the given vantage.
    pub fn view_for(&self, view: View) -> PlayerView {
        match view {
            View::Spectator => PlayerView {
                map: self.map.clone(),
                units: self.units.values().cloned().collect(),
                visible_tiles: (0..self.map.height)
                    .flat_map(|y| (0..self.map.width).map(move |x| (x, y)))
                    .collect(),
                current_turn: self.current_turn,
                turn_number: self.turn_number,
                winner: self.winner,
                you: None,
            },
            View::Player(p) => {
                let visible = self.visible_tiles(p);
                // Adjacency check for forest hiding.
                let visible_units: Vec<Unit> = self
                    .units
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
                        // Hidden in forest unless we have a unit adjacent.
                        self.units.values().any(|own| {
                            own.owner == p
                                && (own.pos.0 - u.pos.0).abs() <= 1
                                && (own.pos.1 - u.pos.1).abs() <= 1
                        })
                    })
                    .cloned()
                    .collect();
                PlayerView {
                    map: self.map.clone(),
                    units: visible_units,
                    visible_tiles: visible.into_iter().collect(),
                    current_turn: self.current_turn,
                    turn_number: self.turn_number,
                    winner: self.winner,
                    you: Some(p),
                }
            }
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
            current_turn: PlayerId::P1,
            turn_number: 1,
            winner: None,
        }
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
    fn killing_last_enemy_wins() {
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
        assert_eq!(g.winner, Some(PlayerId::P1));
        // No counter from a dead unit.
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
