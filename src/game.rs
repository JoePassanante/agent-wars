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
    #[rustfmt::skip]
    let layout = [
        "PPPPPPPPPPPP",
        "PPFFPPPPMMPP",
        "PPFPPPPPMMPP",
        "PPPPPP~~PPPP",
        "PPPPP~~~~PPP",
        "PPPP~~~~~PPP",
        "PPPPP~~PPPPP",
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
            (PlayerId::P1, (1, 1)),
            (PlayerId::P1, (1, 2)),
            (PlayerId::P1, (2, 1)),
            (PlayerId::P2, (10, 8)),
            (PlayerId::P2, (10, 7)),
            (PlayerId::P2, (9, 8)),
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

    /// Attempt to move a unit. Returns Ok on success.
    pub fn try_move(&mut self, actor: PlayerId, unit_id: Uuid, dest: Coord) -> Result<(), String> {
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
            return Err("unit already moved this turn".into());
        }
        let reachable = self.reachable(unit_id);
        if !reachable.contains_key(&dest) {
            return Err("destination not reachable".into());
        }
        if dest != unit.pos && self.unit_at(dest).is_some() {
            return Err("destination occupied".into());
        }
        let unit = self.units.get_mut(&unit_id).unwrap();
        unit.pos = dest;
        unit.has_moved = true;
        Ok(())
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
