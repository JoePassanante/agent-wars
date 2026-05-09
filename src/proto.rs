use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::game::{Coord, PlayerView, View};

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMsg {
    /// First message — pick a vantage.
    Join { view: View },
    /// Move one of your units to a destination.
    Move { unit_id: Uuid, to: Coord },
    /// End your turn.
    EndTurn,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMsg {
    Joined { view: View },
    State(PlayerView),
    Error { message: String },
}
