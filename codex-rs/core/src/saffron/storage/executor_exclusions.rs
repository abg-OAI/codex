//! Thread-lifetime executor exclusions stored only in Saffron's auxiliary database.

use super::SaffronStore;
use codex_protocol::ThreadId;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;

/// The selected workspace that must remain unavailable when a thread resumes.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(in crate::saffron) struct ExecutorExclusion {
    pub(in crate::saffron) position: usize,
    pub(in crate::saffron) environment_id: String,
    pub(in crate::saffron) cwd: PathUri,
    pub(in crate::saffron) workspace_roots: Vec<PathUri>,
}

impl SaffronStore {
    /// Loads exclusions without consulting the current executor registry.
    pub(in crate::saffron) async fn executor_exclusions(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Vec<ExecutorExclusion>> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT selection_json FROM executor_exclusions WHERE thread_id = ? ORDER BY position",
        )
        .bind(thread_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|json| serde_json::from_str(&json).map_err(Into::into))
            .collect()
    }

    /// Commits exclusions before startup succeeds; an existing exclusion cannot be replaced.
    pub(in crate::saffron) async fn save_executor_exclusions(
        &self,
        thread_id: ThreadId,
        exclusions: &[ExecutorExclusion],
    ) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        for exclusion in exclusions {
            sqlx::query(
                "INSERT INTO executor_exclusions
                 (thread_id, environment_id, position, selection_json) VALUES (?, ?, ?, ?)
                 ON CONFLICT(thread_id, environment_id) DO NOTHING",
            )
            .bind(thread_id.to_string())
            .bind(&exclusion.environment_id)
            .bind(i64::try_from(exclusion.position)?)
            .bind(serde_json::to_string(exclusion)?)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }
}
