use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Idle,
    Executing,
    Canceling,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub connection_id: crate::connection::ConnectionId,
    pub database: String,
    pub state: SessionState,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_active: chrono::DateTime<chrono::Utc>,
}

pub struct SessionManager {
    sessions: dashmap::DashMap<SessionId, SessionInfo>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: dashmap::DashMap::new(),
        }
    }

    pub fn create(
        &self,
        connection_id: crate::connection::ConnectionId,
        database: String,
    ) -> SessionId {
        let id = SessionId::new();
        let now = chrono::Utc::now();
        let info = SessionInfo {
            id: id.clone(),
            connection_id,
            database,
            state: SessionState::Idle,
            created_at: now,
            last_active: now,
        };
        self.sessions.insert(id.clone(), info);
        id
    }

    pub fn get(&self, id: &SessionId) -> Option<SessionInfo> {
        self.sessions.get(id).map(|r| r.value().clone())
    }

    pub fn update_state(&self, id: &SessionId, state: SessionState) {
        if let Some(mut session) = self.sessions.get_mut(id) {
            session.state = state;
            session.last_active = chrono::Utc::now();
        }
    }

    pub fn remove(&self, id: &SessionId) {
        self.sessions.remove(id);
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions.iter().map(|r| r.value().clone()).collect()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}
