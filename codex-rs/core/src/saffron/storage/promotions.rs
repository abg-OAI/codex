//! Durable identity and creation boundary for one subagent promotion.

use super::SaffronStore;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;

/// A completion boundary and its persisted parent edge, captured before creation.
#[derive(Clone, Deserialize, Serialize)]
pub(in crate::saffron) struct CompletedAgent {
    /// Member whose completed turn may be retired.
    pub(in crate::saffron) thread_id: ThreadId,
    /// Stored spawn-edge owner, retained after runtime membership is released.
    pub(in crate::saffron) parent_id: ThreadId,
    /// Completion witness that must still be current at retirement.
    pub(in crate::saffron) turn_id: String,
}

/// A reserved destination is never reused for a different source snapshot.
pub(in crate::saffron) struct Promotion {
    /// Selected subagent and stable lookup key for retries.
    pub(in crate::saffron) source_id: ThreadId,
    /// Root authorized to reconcile this handoff.
    pub(in crate::saffron) root_id: ThreadId,
    /// Standalone identity reserved before attempting native creation.
    pub(in crate::saffron) destination_id: ThreadId,
    /// Completed source subtree, with parents preceding their descendants.
    pub(in crate::saffron) members: Vec<CompletedAgent>,
    /// Last confirmed durable boundary.
    pub(in crate::saffron) phase: PromotionPhase,
}

/// Creation and retirement are separate durable effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::saffron) enum PromotionPhase {
    /// An identity is reserved; creation may be running or unconfirmed.
    Preparing,
    /// The new root's history and settings were flushed; retirement remains.
    Ready,
    /// Original subtree closure completed.
    Retired,
}

impl PromotionPhase {
    /// Encodes the phase in the Saffron-owned schema.
    fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Ready => "ready",
            Self::Retired => "retired",
        }
    }
    /// Rejects stored phases that this implementation cannot reconcile.
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "preparing" => Ok(Self::Preparing),
            "ready" => Ok(Self::Ready),
            "retired" => Ok(Self::Retired),
            _ => anyhow::bail!("unknown promotion phase: {value}"),
        }
    }
}

impl SaffronStore {
    /// Reads the saved identity and completion boundaries for a source.
    pub(in crate::saffron) async fn promotion(
        &self,
        source: ThreadId,
    ) -> anyhow::Result<Option<Promotion>> {
        let row = sqlx::query("SELECT root_id, destination_id, members_json, phase FROM subagent_promotions WHERE source_id = ?")
            .bind(source.to_string()).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(Promotion {
                source_id: source,
                root_id: ThreadId::from_string(row.try_get("root_id")?)?,
                destination_id: ThreadId::from_string(row.try_get("destination_id")?)?,
                members: serde_json::from_str(row.try_get("members_json")?)?,
                phase: PromotionPhase::parse(row.try_get("phase")?)?,
            })
        })
        .transpose()
    }

    /// Only the writer that reserved the identity may attempt native creation.
    pub(in crate::saffron) async fn reserve_promotion(
        &self,
        promotion: &Promotion,
    ) -> anyhow::Result<bool> {
        Ok(sqlx::query("INSERT INTO subagent_promotions (source_id, root_id, destination_id, members_json, phase) VALUES (?, ?, ?, ?, 'preparing') ON CONFLICT(source_id) DO NOTHING")
            .bind(promotion.source_id.to_string()).bind(promotion.root_id.to_string())
            .bind(promotion.destination_id.to_string()).bind(serde_json::to_string(&promotion.members)?)
            .execute(&self.pool).await?.rows_affected() == 1)
    }

    /// Creation is ready only after settings and history have been durably saved.
    pub(in crate::saffron) async fn set_promotion_phase(
        &self,
        source: ThreadId,
        phase: PromotionPhase,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE subagent_promotions SET phase = ? WHERE source_id = ?")
            .bind(phase.as_str())
            .bind(source.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
