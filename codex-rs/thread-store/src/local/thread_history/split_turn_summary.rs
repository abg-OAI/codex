//! A turn can start in one checkpoint segment, receive items in others, and
//! finish in the newest. Its terminal projection has only local summary IDs.

use super::super::rollout_lineage::LineageWalk;
use super::read::StoredTurnRow;
use super::read::stored_thread_item;
use super::sqlite_integer;
use crate::StoredThreadItem;
use crate::ThreadStoreResult;
use sqlx::Row;

pub(super) async fn load(
    pool: &sqlx::SqlitePool,
    lineage: &mut LineageWalk<'_>,
    turn: &StoredTurnRow,
) -> ThreadStoreResult<Vec<StoredThreadItem>> {
    let mut user = None;
    let mut final_answer = None;
    let mut unphased_answer = None;
    let mut depth = 0;
    while let Some((segment, _)) = lineage.at_depth(depth).await? {
        depth += 1;
        let start = sqlite_integer(segment.start_ordinal(), "rollout ordinal")?;
        if start > turn.position.rollout_ordinal {
            continue;
        }
        let end = segment
            .end_ordinal()
            .map(|n| sqlite_integer(n, "rollout ordinal"))
            .transpose()?;
        let physical_id = segment.rollout_id().to_string();
        // Select at most three messages per segment. The native summary prefers
        // an explicit final answer over a later message without a phase.
        let rows = sqlx::query(
            r#"
WITH messages AS (
    SELECT *, CASE WHEN item_type = '' THEN json_extract(item_json, '$.type') ELSE item_type END AS kind
    FROM thread_items
    WHERE thread_id = ?1 AND turn_id = ?2
      AND rollout_ordinal >= ?3 AND (?4 IS NULL OR rollout_ordinal < ?4)
      AND (item_type IN ('userMessage', 'agentMessage', ''))
), first_user AS (
    SELECT *, 'first_user' AS slot FROM messages
    WHERE kind = 'userMessage' ORDER BY rollout_ordinal LIMIT 1
), final_answer AS (
    SELECT *, 'final_answer' AS slot FROM messages
    WHERE kind = 'agentMessage' AND json_extract(item_json, '$.phase') = 'final_answer'
    ORDER BY rollout_ordinal DESC LIMIT 1
), unphased AS (
    SELECT *, 'unphased' AS slot FROM messages
    WHERE kind = 'agentMessage' AND json_extract(item_json, '$.phase') IS NULL
    ORDER BY rollout_ordinal DESC LIMIT 1
)
SELECT * FROM first_user UNION ALL SELECT * FROM final_answer UNION ALL SELECT * FROM unphased
"#,
        )
        .bind(&physical_id)
        .bind(&turn.turn_id)
        .bind(start)
        .bind(end)
        .fetch_all(pool)
        .await?;
        for row in rows {
            match row.try_get::<&str, _>("slot")? {
                "first_user" => user = Some(stored_thread_item(row)?),
                "final_answer" if final_answer.is_none() => {
                    final_answer = Some(stored_thread_item(row)?)
                }
                "unphased" if unphased_answer.is_none() => {
                    unphased_answer = Some(stored_thread_item(row)?)
                }
                _ => {}
            }
        }
        let has_start: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM thread_turns WHERE thread_id = ?1 AND turn_id = ?2
             AND rollout_ordinal >= ?3 AND (?4 IS NULL OR rollout_ordinal < ?4)
             AND (rollout_end_ordinal IS NULL OR rollout_end_ordinal != rollout_ordinal))",
        )
        .bind(physical_id)
        .bind(&turn.turn_id)
        .bind(start)
        .bind(end)
        .fetch_one(pool)
        .await?;
        if has_start {
            break;
        }
    }
    Ok(user
        .into_iter()
        .chain(final_answer.or(unphased_answer))
        .collect())
}
