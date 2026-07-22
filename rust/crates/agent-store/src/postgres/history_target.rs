use agent_vocab::TranscriptItem;
use anyhow::Result;
use sqlx::{Postgres, Row, Transaction};

use crate::HistoryTarget;

use super::ensure_valid_transcript_ancestry;
use super::sql::{ensure_no_active_work_tx, ensure_no_running_delegation_tx};

pub(super) async fn validate_history_target_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    target: HistoryTarget<'_>,
) -> Result<()> {
    ensure_no_running_delegation_tx(tx, session_id).await?;
    let (current_active_leaf_id, current_transcript_revision): (Option<String>, i64) =
        sqlx::query_as("select active_leaf_id, transcript_revision from sessions where id=$1")
            .bind(session_id)
            .fetch_one(&mut **tx)
            .await?;
    ensure_no_active_work_tx(tx, session_id).await?;

    match target.expected_active_leaf_id {
        Some(expected) if current_active_leaf_id.as_deref() != expected => {
            return Err(crate::ExpectedActiveLeafMismatch::new(
                current_active_leaf_id,
                expected.map(str::to_string),
            )
            .into());
        }
        None | Some(_) => {}
    }
    match target.expected_transcript_revision {
        Some(expected) if current_transcript_revision != expected => {
            return Err(crate::HistoryChanged.into());
        }
        None | Some(_) => {}
    }
    if let Some(leaf_id) = target.leaf_id {
        let item: Option<serde_json::Value> = sqlx::query_scalar(
            "select item from transcript_entries where session_id=$1 and id=$2::text",
        )
        .bind(session_id)
        .bind(leaf_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(item) = item else {
            return Err(crate::HistoryTargetNotTurnBoundary.into());
        };
        if !matches!(
            serde_json::from_value(item)?,
            TranscriptItem::TurnFinished { .. } | TranscriptItem::CompactionSummary(_)
        ) {
            return Err(crate::HistoryTargetNotTurnBoundary.into());
        }
    }
    if let Some(source_entry_id) = target.source_entry_id {
        let resolved = history_target_context_for_user_tx(tx, session_id, source_entry_id).await?;
        if resolved.as_deref() != target.leaf_id {
            return Err(crate::HistoryChanged.into());
        }
    }
    if let Some(expected_ids) = target.expected_active_branch_entry_ids {
        let target_ids = branch_entry_ids_tx(tx, session_id, target.leaf_id).await?;
        if expected_ids != target_ids {
            return Err(crate::HistoryChanged.into());
        }
    }
    Ok(())
}

pub(super) async fn history_target_context_for_user_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry_id: &str,
) -> Result<Option<String>> {
    let row = sqlx::query(
        r#"
        with recursive reachable as (
            select t.id, t.parent_id, t.item->>'type' as item_type
            from transcript_entries t
            where t.session_id=$1 and t.id=$2::text
              and t.item->>'type' = 'user_message'

            union

            select parent.id, parent.parent_id, parent.item->>'type' as item_type
            from transcript_entries parent
            join reachable child
              on parent.session_id=$1 and parent.id=child.parent_id
            where child.item_type not in ('turn_finished', 'compaction_summary')
        ),
        path_state as (
            select coalesce(bool_and(
                reachable.item_type not in ('turn_finished', 'compaction_summary')
                and exists(
                    select 1
                    from transcript_entries parent
                    where parent.session_id=$1 and parent.id=reachable.parent_id
                )
            ), false) as ancestry_invalid
            from reachable
        ),
        path as (
            select t.id, t.parent_id, t.item->>'type' as item_type,
                   0::bigint as depth, path_state.ancestry_invalid
            from transcript_entries t
            cross join path_state
            where t.session_id=$1 and t.id=$2::text
              and t.item->>'type' = 'user_message'

            union all

            select parent.id, parent.parent_id, parent.item->>'type' as item_type,
                   child.depth + 1, child.ancestry_invalid
            from transcript_entries parent
            join path child on parent.session_id=$1 and parent.id=child.parent_id
            where child.item_type not in ('turn_finished', 'compaction_summary')
              and not child.ancestry_invalid
        ),
        context as (
            select
                (array_agg(id order by depth)
                    filter (where item_type in ('turn_finished', 'compaction_summary')))[1]
                    as target_leaf_id,
                bool_or(parent_id is null) as reached_root,
                bool_or(ancestry_invalid) as ancestry_invalid
            from path
            having count(*) > 0
        )
        select target_leaf_id, reached_root, ancestry_invalid
        from context
        "#,
    )
    .bind(session_id)
    .bind(entry_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Err(crate::HistoryChanged.into());
    };
    ensure_valid_transcript_ancestry(std::slice::from_ref(&row))?;
    let reached_root: bool = row.get("reached_root");
    let target_leaf_id: Option<String> = row.get("target_leaf_id");
    if target_leaf_id.is_none() && !reached_root {
        return Err(crate::HistoryChanged.into());
    }
    Ok(target_leaf_id)
}

pub(super) async fn branch_entry_ids_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    leaf_id: Option<&str>,
) -> Result<Vec<String>> {
    let Some(leaf_id) = leaf_id else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query(
        r#"
        with recursive reachable as (
            select t.id,
                   coalesce(
                       case
                           when t.item->>'type' = 'compaction_summary' then t.item->>'source_leaf_id'
                           else null
                       end,
                       t.parent_id
                   ) as next_id
            from transcript_entries t
            where t.session_id = $1 and t.id = $2::text

            union

            select parent.id,
                   coalesce(
                       case
                           when parent.item->>'type' = 'compaction_summary' then parent.item->>'source_leaf_id'
                           else null
                       end,
                       parent.parent_id
                   ) as next_id
            from transcript_entries parent
            join reachable child on parent.session_id=$1 and parent.id=child.next_id
        ),
        path_state as (
            select coalesce(bool_and(exists(
                select 1
                from transcript_entries parent
                where parent.session_id=$1 and parent.id=reachable.next_id
            )), false) as ancestry_invalid
            from reachable
        ),
        path as (
            select t.id,
                   coalesce(
                       case
                           when t.item->>'type' = 'compaction_summary' then t.item->>'source_leaf_id'
                           else null
                       end,
                       t.parent_id
                   ) as next_id,
                   0::bigint as depth,
                   path_state.ancestry_invalid
            from transcript_entries t
            cross join path_state
            where t.session_id=$1 and t.id=$2::text

            union all

            select parent.id,
                   coalesce(
                       case
                           when parent.item->>'type' = 'compaction_summary' then parent.item->>'source_leaf_id'
                           else null
                       end,
                       parent.parent_id
                   ) as next_id,
                   child.depth + 1,
                   child.ancestry_invalid
            from transcript_entries parent
            join path child on parent.session_id=$1 and parent.id=child.next_id
            where not child.ancestry_invalid
        )
        select id, ancestry_invalid
        from path
        order by depth desc
        "#,
    )
    .bind(session_id)
    .bind(leaf_id)
    .fetch_all(&mut **tx)
    .await?;
    ensure_valid_transcript_ancestry(&rows)?;
    Ok(rows
        .into_iter()
        .map(|row| row.get::<String, _>("id"))
        .collect())
}
