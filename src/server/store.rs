//! In-memory storage for local response continuation state and Anthropic batches.

use crate::{
    anthropic::{MessageBatch, MessageBatchResult},
    config::Provider,
    openai::response::ResponseObject,
};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

/// Stored `OpenAI` Responses API object plus its input items.
#[derive(Debug, Clone)]
pub struct StoredResponse {
    /// Response object returned to the client.
    pub response: ResponseObject,
    /// Input items recorded for local `previous_response_id` continuation.
    pub input_items: Vec<Value>,
    /// Upstream provider that created or backed this response.
    pub provider: Provider,
    /// Whether the response id is known to be an upstream retrievable resource.
    pub upstream_resource: bool,
}

/// In-memory store for local `previous_response_id` continuation state.
#[derive(Debug, Clone, Default)]
pub struct ResponseStore {
    inner: Arc<RwLock<HashMap<String, StoredResponse>>>,
}

impl ResponseStore {
    /// Saves response continuation state for later in-process reuse.
    pub async fn insert(&self, stored: StoredResponse) {
        self.inner
            .write()
            .await
            .insert(stored.response.id.clone(), stored);
    }

    /// Loads previously stored response continuation state.
    #[must_use]
    pub async fn get(&self, id: &str) -> Option<StoredResponse> {
        self.inner.read().await.get(id).cloned()
    }

    /// Removes previously stored response continuation state.
    pub async fn remove(&self, id: &str) -> Option<StoredResponse> {
        self.inner.write().await.remove(id)
    }
}

/// Stored `Anthropic` message batch object and JSONL-ready result lines.
#[derive(Debug, Clone)]
pub struct StoredBatch {
    /// Batch metadata returned by create and retrieve calls.
    pub batch: MessageBatch,
    /// Individual result lines returned by the batch results endpoint.
    pub results: Vec<MessageBatchResult>,
    /// Whether a caller has requested cancellation.
    pub cancel_requested: bool,
}

/// In-memory store for `Anthropic` message batches.
#[derive(Debug, Clone, Default)]
pub struct BatchStore {
    inner: Arc<RwLock<HashMap<String, StoredBatch>>>,
}

impl BatchStore {
    /// Saves a completed or in-progress batch object.
    pub async fn insert(&self, stored: StoredBatch) {
        self.inner
            .write()
            .await
            .insert(stored.batch.id.clone(), stored);
    }

    /// Loads a previously stored batch object.
    #[must_use]
    pub async fn get(&self, id: &str) -> Option<StoredBatch> {
        self.inner.read().await.get(id).cloned()
    }

    /// Lists stored batches ordered from newest to oldest.
    #[must_use]
    pub async fn list(&self) -> Vec<StoredBatch> {
        let mut batches = self
            .inner
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        batches.sort_by(|left, right| right.batch.created_at.cmp(&left.batch.created_at));
        batches
    }

    /// Updates a stored batch in place and returns the updated value.
    pub async fn update<F>(&self, id: &str, update: F) -> Option<StoredBatch>
    where
        F: FnOnce(&mut StoredBatch),
    {
        let mut guard = self.inner.write().await;
        let stored = guard.get_mut(id)?;
        update(stored);
        let updated = stored.clone();
        drop(guard);
        Some(updated)
    }

    /// Removes a stored batch and returns the removed value.
    pub async fn remove(&self, id: &str) -> Option<StoredBatch> {
        self.inner.write().await.remove(id)
    }

    /// Returns whether cancellation has been requested for the batch.
    #[must_use]
    pub async fn cancel_requested(&self, id: &str) -> Option<bool> {
        self.inner
            .read()
            .await
            .get(id)
            .map(|stored| stored.cancel_requested)
    }
}
