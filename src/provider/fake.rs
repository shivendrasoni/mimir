use std::{collections::VecDeque, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::model::{ModelRequest, ModelResponse};

use super::{Provider, ProviderError};

#[derive(Clone, Default)]
pub struct FakeProvider {
    responses: Arc<Mutex<VecDeque<Result<ModelResponse, ProviderError>>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    delay: Duration,
}

impl FakeProvider {
    pub fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().map(Ok).collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
            delay: Duration::ZERO,
        }
    }

    pub fn with_results(responses: Vec<Result<ModelResponse, ProviderError>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
            delay: Duration::ZERO,
        }
    }

    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub async fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().await.clone()
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.requests.lock().await.push(request);
        let response = self.responses.lock().await.pop_front().unwrap_or_else(|| {
            Err(ProviderError::Protocol {
                message: "fake response script is exhausted".into(),
            })
        });
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        response
    }
}
