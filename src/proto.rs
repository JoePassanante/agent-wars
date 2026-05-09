use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::game::{Coord, PlayerId, PlayerView, UnitKind};
use crate::lobby::SessionId;

/// Messages sent client → server. Every connection MUST send `Hello` first;
/// all other commands require an established session attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ClientMsg {
    /// First message after connect. Declares identity and what you want to
    /// do. The server will respond with `Hello`, then either `Queued` →
    /// `Matched`, `Reconnected`, or `Spectating` depending on the intent.
    Hello {
        username: String,
        intent: ClientIntent,
    },
    /// Move a unit and optionally attack. Requires a Player attachment.
    Move {
        unit_id: Uuid,
        to: Coord,
        #[serde(default)]
        attack: Option<Coord>,
    },
    BuyUnit {
        factory_id: Uuid,
        kind: UnitKind,
    },
    EndTurn,
    Surrender,
    /// Resign / stop spectating. Detaches the user from the session
    /// (players who Leave forfeit by surrendering).
    Leave,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ClientIntent {
    /// Join the matchmaking queue. Reconnects automatically if the username
    /// already has an active session.
    Play,
    /// Watch a specific session as a spectator.
    Watch { session_id: SessionId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ServerMsg {
    /// Acknowledges Hello. Echoes the username (servers may normalize it)
    /// and reports the server's game version so clients can warn on a
    /// mismatch.
    Hello {
        username: String,
        server_version: String,
    },
    /// You are queued; another player is needed before a match starts.
    Queued { position: u32 },
    /// You've been matched into a session as the given role.
    Matched {
        session_id: SessionId,
        role: PlayerId,
    },
    /// You reconnected to a session you were already in.
    Reconnected {
        session_id: SessionId,
        role: PlayerId,
    },
    /// You're attached as a spectator to this session.
    Spectating { session_id: SessionId },
    /// Player view of the current session state.
    State(PlayerView),
    Error {
        message: String,
    },
}
