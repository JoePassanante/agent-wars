use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::game::{Coord, PlayerView, UnitKind, View};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ClientMsg {
    /// First message — pick a vantage.
    Join { view: View },
    /// Move a unit to `to`. If `attack` is set, the unit then attacks the
    /// enemy at that coord (which must be in range from `to`).
    Move {
        unit_id: Uuid,
        to: Coord,
        #[serde(default)]
        attack: Option<Coord>,
    },
    /// Buy a unit at one of your factories. The factory tile must be empty
    /// and the factory must not have already produced this turn.
    BuyUnit {
        factory_id: Uuid,
        kind: UnitKind,
    },
    /// End your turn.
    EndTurn,
    /// Concede the match. Only allowed after at least 3 full turns have
    /// elapsed (turn_number >= 4) so a player can't bail out instantly.
    Surrender,
    /// Reset the lobby — wipe game state and start a fresh match.
    /// Allowed from any connected client (player or spectator).
    Reset,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ServerMsg {
    Joined { view: View },
    State(PlayerView),
    Error { message: String },
}
