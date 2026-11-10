use sqlly_core::ObjectId;
use std::collections::HashMap;

pub struct MetadataCache {
    nodes: HashMap<ObjectId, Vec<sqlly_core::ObjectNode>>,
}

impl MetadataCache {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
        }
    }

    pub fn get(&self, parent_id: &ObjectId) -> Option<&Vec<sqlly_core::ObjectNode>> {
        self.nodes.get(parent_id)
    }

    pub fn insert(&mut self, parent_id: ObjectId, children: Vec<sqlly_core::ObjectNode>) {
        self.nodes.insert(parent_id, children);
    }

    pub fn invalidate(&mut self, node_id: &ObjectId) {
        self.nodes.remove(node_id);
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
    }
}

impl Default for MetadataCache {
    fn default() -> Self {
        Self::new()
    }
}
