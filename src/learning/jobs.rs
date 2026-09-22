//! Private, reference-only durable evaluation queue.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

use super::{CrossProcessLock, project_learning_dir};
use crate::{
    atomic::{path_lock, prepare_state_path, read_json, write_json},
    error::Result,
};

const MAX_RETRIES: u8 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LearningJob {
    pub id: Uuid,
    pub session_id: String,
    pub first_record_id: Uuid,
    pub last_record_id: Uuid,
    #[serde(default)]
    pub exposed_candidate_ids: Vec<Uuid>,
    pub attempts: u8,
    pub available_at: DateTime<Utc>,
    #[serde(default)]
    pub claimed_at: Option<DateTime<Utc>>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Queue {
    schema: u16,
    jobs: Vec<LearningJob>,
}
fn path(root: &Path) -> std::path::PathBuf {
    project_learning_dir(root).join("queue.json")
}

pub async fn enqueue(
    root: &Path,
    session_id: &str,
    first: Uuid,
    last: Uuid,
    exposed: Vec<Uuid>,
) -> Result<LearningJob> {
    let job = LearningJob {
        id: Uuid::new_v4(),
        session_id: session_id.into(),
        first_record_id: first,
        last_record_id: last,
        exposed_candidate_ids: exposed,
        attempts: 0,
        available_at: Utc::now(),
        claimed_at: None,
    };
    let queue_path = path(root);
    prepare_state_path(root, &queue_path).await?;
    let lock = path_lock(&queue_path);
    let _guard = lock.lock().await;
    let _process_guard = CrossProcessLock::acquire(&queue_path).await?;
    let mut queue: Queue = read_json(&queue_path).await?.unwrap_or_default();
    queue.schema = 1;
    queue.jobs.push(job.clone());
    write_json(&queue_path, &queue).await?;
    Ok(job)
}

/// Claims at most one ready job. A stale claim is recovered after five minutes.
pub async fn claim(root: &Path) -> Result<Option<LearningJob>> {
    let queue_path = path(root);
    prepare_state_path(root, &queue_path).await?;
    let lock = path_lock(&queue_path);
    let _guard = lock.lock().await;
    let _process_guard = CrossProcessLock::acquire(&queue_path).await?;
    let mut queue: Queue = read_json(&queue_path).await?.unwrap_or_default();
    let now = Utc::now();
    let job = queue.jobs.iter_mut().find(|job| {
        job.available_at <= now
            && job
                .claimed_at
                .is_none_or(|claimed| claimed < now - Duration::minutes(5))
    });
    let claimed = job.map(|job| {
        job.claimed_at = Some(now);
        job.clone()
    });
    write_json(&queue_path, &queue).await?;
    Ok(claimed)
}

pub async fn complete(root: &Path, id: Uuid, succeeded: bool) -> Result<()> {
    let queue_path = path(root);
    prepare_state_path(root, &queue_path).await?;
    let lock = path_lock(&queue_path);
    let _guard = lock.lock().await;
    let _process_guard = CrossProcessLock::acquire(&queue_path).await?;
    let mut queue: Queue = read_json(&queue_path).await?.unwrap_or_default();
    if succeeded {
        queue.jobs.retain(|job| job.id != id);
    } else if let Some(job) = queue.jobs.iter_mut().find(|job| job.id == id) {
        job.attempts = job.attempts.saturating_add(1);
        job.claimed_at = None;
        job.available_at = Utc::now() + Duration::seconds(i64::from(job.attempts) * 10);
        if job.attempts >= MAX_RETRIES {
            queue.jobs.retain(|item| item.id != id);
        }
    }
    write_json(&queue_path, &queue).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    #[tokio::test]
    async fn stale_claim_is_recovered() {
        let temp = TempDir::new().unwrap();
        let id = Uuid::new_v4();
        enqueue(temp.path(), "s", id, id, vec![]).await.unwrap();
        let first = claim(temp.path()).await.unwrap().unwrap();
        assert!(claim(temp.path()).await.unwrap().is_none());
        complete(temp.path(), first.id, false).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_claims_only_return_a_job_once() {
        let temp = TempDir::new().unwrap();
        let id = Uuid::new_v4();
        enqueue(temp.path(), "s", id, id, vec![]).await.unwrap();
        let root = temp.path().to_owned();
        let (left, right) = tokio::join!(claim(&root), claim(&root));
        assert_eq!(
            usize::from(left.unwrap().is_some()) + usize::from(right.unwrap().is_some()),
            1
        );
    }
}
