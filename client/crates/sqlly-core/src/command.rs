use crate::{ConnectionId, ConnectionProfile, StudioResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Connect {
        profile: ConnectionProfile,
    },
    Disconnect {
        connection_id: ConnectionId,
    },
    TestConnection {
        profile: ConnectionProfile,
    },
    ListDatabases {
        connection_id: ConnectionId,
    },
    ExecuteQuery {
        connection_id: ConnectionId,
        database: Option<String>,
        sql: String,
    },
    CancelQuery {
        request_id: String,
    },
    LoadObjectExplorerNode {
        connection_id: ConnectionId,
        node_id: String,
    },
}

#[async_trait]
pub trait CommandBus: Send + Sync {
    async fn dispatch(&self, command: Command) -> StudioResult<()>;
}
