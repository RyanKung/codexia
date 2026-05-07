//! In-memory storage for local response continuation state and Anthropic batches.

use crate::{
    anthropic::{MessageBatch, MessageBatchResult},
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
}

/// Stored `Anthropic` message batch object and JSONL-ready result lines.
#[derive(Debug, Clone)]
pub struct StoredBatch {
    /// Batch metadata returned by create and retrieve calls.
    pub batch: MessageBatch,
    /// Individual result lines returned by the batch results endpoint.
    pub results: Vec<MessageBatchResult>,
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
}
