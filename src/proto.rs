use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::game::{Coord, PlayerView, View};

#[derive(Debug, Clone, Deserialize)]
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
    /// End your turn.
    EndTurn,
    /// Reset the lobby — wipe game state and start a fresh match.
    /// Allowed from any connected client (player or spectator).
    Reset,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ServerMsg {
    Joined { view: View },
    State(PlayerView),
    Error { message: String },
}
