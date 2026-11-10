use crate::metadata::cache::MetadataCache;
use sqlly_core::{ConnectionId, ObjectKind, ObjectNode, StudioResult};

pub struct ObjectExplorer {
    cache: MetadataCache,
}

impl ObjectExplorer {
    pub fn new() -> Self {
        Self {
            cache: MetadataCache::new(),
        }
    }

    pub fn root_nodes(&self, _connection_id: &ConnectionId) -> Vec<ObjectNode> {
        vec![
            ObjectNode::new(ObjectKind::Database, "Databases".to_string()).expandable(),
            ObjectNode::new(ObjectKind::Login, "Security".to_string()).expandable(),
            ObjectNode::new(ObjectKind::Server, "Server Objects".to_string()).expandable(),
            ObjectNode::new(ObjectKind::ErrorLog, "Management".to_string()).expandable(),
            ObjectNode::new(ObjectKind::Job, "SQL Server Agent".to_string()).expandable(),
        ]
    }

    pub async fn expand_node(
        &self,
        _connection_id: &ConnectionId,
        _parent_id: &sqlly_core::ObjectId,
    ) -> StudioResult<Vec<ObjectNode>> {
        // Catalog-backed expansion is provided by the transport-specific metadata service.
        Ok(vec![])
    }

    pub async fn refresh_node(
        &mut self,
        _connection_id: &ConnectionId,
        node_id: &sqlly_core::ObjectId,
    ) -> StudioResult<Vec<ObjectNode>> {
        self.cache.invalidate(node_id);
        self.expand_node(_connection_id, node_id).await
    }
}

impl Default for ObjectExplorer {
    fn default() -> Self {
        Self::new()
    }
}
