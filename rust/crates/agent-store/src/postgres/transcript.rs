use std::collections::{BTreeMap, BTreeSet, HashMap};

use agent_session::{
    ModelContext, ModelContextEntry, StoredSession, StoredTranscriptEntry, TranscriptStorageNode,
};
use agent_vocab::TranscriptItem;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sqlx::{Postgres, Row, Transaction};

use crate::{
    ActiveBranchSync, ActiveBranchSyncStatus, EventFrame, EventType, HistoryTree, SessionActivity,
    SwitchActiveLeafResult, TranscriptEntriesResult, TranscriptEntryBodyMode,
    TranscriptEntryRecord, TranscriptEntryScope, TranscriptTreeIndex, TranscriptTreeNodeRecord,
    TranscriptTurnDetailResult, TranscriptTurnsResult, TurnCardRecord, TurnCardStatus,
};

use super::events::{insert_event_tx, insert_transcript_item_events_tx};
use super::queue::bump_revisions_tx;
use super::rows::{row_to_stored_entry, row_to_transcript_entry};
use super::sql::{action_is_unfinished, lock_session_tx};
use super::PostgresAgentStore;

const DEFAULT_TRANSCRIPT_INDEX_LIMIT: i64 = 1000;
const MAX_TRANSCRIPT_INDEX_LIMIT: i64 = 5000;
const DEFAULT_TRANSCRIPT_TURN_LIMIT: i64 = 50;
const MAX_TRANSCRIPT_TURN_LIMIT: i64 = 200;

fn provider_replay_select(body_mode: TranscriptEntryBodyMode) -> &'static str {
    match body_mode {
        TranscriptEntryBodyMode::Full => "provider_replay",
        TranscriptEntryBodyMode::Ui => "'[]'::jsonb as provider_replay",
    }
}

fn aliased_provider_replay_select(alias: &str, body_mode: TranscriptEntryBodyMode) -> String {
    match body_mode {
        TranscriptEntryBodyMode::Full => format!("{alias}.provider_replay"),
        TranscriptEntryBodyMode::Ui => "'[]'::jsonb as provider_replay".to_string(),
    }
}

async fn active_branch_entry_ids_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
) -> Result<Vec<String>> {
    let rows = sqlx::query(
        r#"
        with recursive branch as (
            select t.id, t.parent_id, t.item, t.sequence
            from transcript_entries t
            join sessions s on s.id = t.session_id and s.active_leaf_id = t.id
            where t.session_id = $1

            union all

            select parent.id, parent.parent_id, parent.item, parent.sequence
            from transcript_entries parent
            join branch child
              on parent.session_id = $1
             and parent.id = coalesce(
                case
                    when child.item->>'type' = 'compaction_summary' then child.item->>'source_leaf_id'
                    else null
                end,
                child.parent_id
             )
        )
        select id from branch order by sequence
        "#,
    )
    .bind(session_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| row.get::<String, _>("id"))
        .collect())
}

async fn transcript_entry_records_by_id_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry_ids: &[String],
    body_mode: TranscriptEntryBodyMode,
) -> Result<Vec<TranscriptEntryRecord>> {
    if entry_ids.is_empty() {
        return Ok(Vec::new());
    }
    let provider_replay_select = provider_replay_select(body_mode);
    let query = format!(
        r#"
        select id, parent_id, timestamp_ms, sequence, item, {provider_replay_select}
        from transcript_entries
        where session_id=$1 and id = any($2::text[])
        order by sequence
        "#
    );
    let rows = sqlx::query(&query)
        .bind(session_id)
        .bind(entry_ids)
        .fetch_all(&mut **tx)
        .await?;
    rows.into_iter()
        .map(|row| row_to_transcript_entry(&row))
        .collect()
}

async fn active_branch_entry_records_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    body_mode: TranscriptEntryBodyMode,
) -> Result<Vec<TranscriptEntryRecord>> {
    let provider_replay_select = provider_replay_select(body_mode);
    let leaf_provider_replay_select = aliased_provider_replay_select("t", body_mode);
    let parent_provider_replay_select = aliased_provider_replay_select("parent", body_mode);
    let query = format!(
        r#"
        with recursive branch as (
            select t.id, t.parent_id, t.timestamp_ms, t.item, {leaf_provider_replay_select}, t.sequence
            from transcript_entries t
            join sessions s on s.id = t.session_id and s.active_leaf_id = t.id
            where t.session_id = $1

            union all

            select parent.id, parent.parent_id, parent.timestamp_ms, parent.item, {parent_provider_replay_select}, parent.sequence
            from transcript_entries parent
            join branch child
              on parent.session_id = $1
             and parent.id = coalesce(
                case
                    when child.item->>'type' = 'compaction_summary' then child.item->>'source_leaf_id'
                    else null
                end,
                child.parent_id
             )
        )
        select id, parent_id, timestamp_ms, sequence, item, {provider_replay_select}
        from branch
        order by sequence
        "#
    );
    let rows = sqlx::query(&query)
        .bind(session_id)
        .fetch_all(&mut **tx)
        .await?;
    rows.into_iter()
        .map(|row| row_to_transcript_entry(&row))
        .collect()
}

async fn active_branch_turn_card_rows_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    before_entry_id: Option<&str>,
    limit: i64,
) -> Result<Vec<TurnCardRow>> {
    let rows = sqlx::query(
        r#"
        with recursive branch as (
            select t.id,
                   t.parent_id,
                   t.timestamp_ms,
                   t.item,
                   t.sequence,
                   case
                     when t.item->>'type' in ('turn_started', 'compaction_summary') then 1
                     else 0
                   end as boundary_count
            from transcript_entries t
            left join sessions s on s.id = t.session_id
            where t.session_id = $1
              and t.id = coalesce($2::text, s.active_leaf_id)

            union all

            select parent.id,
                   parent.parent_id,
                   parent.timestamp_ms,
                   parent.item,
                   parent.sequence,
                   child.boundary_count
                     + case
                         when parent.item->>'type' in ('turn_started', 'compaction_summary') then 1
                         else 0
                       end as boundary_count
            from transcript_entries parent
            join branch child
              on parent.session_id = $1
             and parent.id = coalesce(
                case
                    when child.item->>'type' = 'compaction_summary' then child.item->>'source_leaf_id'
                    else null
                end,
                child.parent_id
             )
            where child.boundary_count < $3
        ),
        card_rows as (
            select id,
                   parent_id,
                   timestamp_ms,
                   item,
                   sequence,
                   sum(
                     case
                       when item->>'type' in ('turn_started', 'compaction_summary') then 1
                       else 0
                     end
                   ) over (order by sequence rows between unbounded preceding and current row) as card_ordinal
            from branch
            where item->>'type' in ('turn_started', 'user_message', 'assistant_message', 'tool_call_started', 'tool_result', 'turn_finished', 'compaction_summary')
        ),
        terminal_assistant as (
            select card_ordinal, max(sequence) as sequence
            from card_rows
            where item->>'type' = 'assistant_message'
            group by card_ordinal
        )
        select card_rows.id,
               card_rows.parent_id,
               card_rows.timestamp_ms,
               card_rows.sequence,
               case
                 when card_rows.item->>'type' = 'user_message'
                      or terminal_assistant.sequence = card_rows.sequence then card_rows.item
                 else null
               end as body_item,
               card_rows.item->>'type' as item_type,
               card_rows.item->'turn_id' as turn_id,
               card_rows.item->'outcome' as outcome,
               card_rows.item->>'source_leaf_id' as compaction_source_leaf_id,
               card_rows.item #>> '{last_turn_id}' as compaction_last_turn_id,
               card_rows.item #>> '{turn_started_at_ms}' as compaction_turn_started_at_ms,
               card_rows.item->>'summary' as compaction_summary
        from card_rows
        left join terminal_assistant
          on terminal_assistant.card_ordinal = card_rows.card_ordinal
        order by card_rows.sequence
        "#
    )
    .bind(session_id)
    .bind(before_entry_id)
    .bind(limit)
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter().map(|row| turn_card_row(&row)).collect()
}

async fn active_branch_entry_records_between_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    leaf_id: &str,
    start_sequence: i64,
    end_sequence: i64,
    body_mode: TranscriptEntryBodyMode,
) -> Result<Vec<TranscriptEntryRecord>> {
    let provider_replay_select = provider_replay_select(body_mode);
    let leaf_provider_replay_select = aliased_provider_replay_select("t", body_mode);
    let parent_provider_replay_select = aliased_provider_replay_select("parent", body_mode);
    let query = format!(
        r#"
        with recursive card_path as (
            select t.id, t.parent_id, t.timestamp_ms, t.item, {leaf_provider_replay_select}, t.sequence
            from transcript_entries t
            where t.session_id = $1
              and t.id = $2::text
              and t.sequence = $4

            union all

            select parent.id, parent.parent_id, parent.timestamp_ms, parent.item, {parent_provider_replay_select}, parent.sequence
            from transcript_entries parent
            join card_path child
              on parent.session_id = $1
             and parent.id = child.parent_id
            where child.sequence > $3
        )
        select id, parent_id, timestamp_ms, sequence, item, {provider_replay_select}
        from card_path
        where sequence between $3 and $4
        order by sequence
        "#
    );
    let rows = sqlx::query(&query)
        .bind(session_id)
        .bind(leaf_id)
        .bind(start_sequence)
        .bind(end_sequence)
        .fetch_all(&mut **tx)
        .await?;
    rows.into_iter()
        .map(|row| row_to_transcript_entry(&row))
        .collect()
}

impl PostgresAgentStore {
    pub async fn load_stored_session(&self, session_id: &str) -> Result<StoredSession> {
        let session_row = sqlx::query("select active_leaf_id, metadata from sessions where id=$1")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let entries = self.stored_transcript_entries(session_id).await?;
        let mut metadata = BTreeMap::new();
        if let Value::Object(map) = session_row.get::<Value, _>("metadata") {
            for (key, value) in map {
                if let Some(value) = value.as_str() {
                    metadata.insert(key, value.to_string());
                }
            }
        }
        Ok(StoredSession {
            session_id: session_id.to_string(),
            active_leaf_id: session_row.get("active_leaf_id"),
            metadata,
            entries,
        })
    }

    pub async fn sync_active_branch(
        &self,
        session_id: &str,
        base_leaf_id: Option<&str>,
        body_mode: TranscriptEntryBodyMode,
    ) -> Result<ActiveBranchSync> {
        let active_leaf_id = self.active_leaf_id(session_id).await?;
        if active_leaf_id.as_deref() == base_leaf_id {
            return Ok(ActiveBranchSync {
                session_id: session_id.to_string(),
                base_leaf_id: base_leaf_id.map(ToOwned::to_owned),
                active_leaf_id,
                status: ActiveBranchSyncStatus::Unchanged,
                entries: Vec::new(),
            });
        }
        let Some(active_leaf_id) = active_leaf_id else {
            return Ok(ActiveBranchSync {
                session_id: session_id.to_string(),
                base_leaf_id: base_leaf_id.map(ToOwned::to_owned),
                active_leaf_id: None,
                status: ActiveBranchSyncStatus::BranchChanged,
                entries: Vec::new(),
            });
        };
        let entries = match base_leaf_id {
            Some(base_leaf_id) => {
                self.active_branch_entry_records_after(session_id, base_leaf_id, body_mode)
                    .await?
            }
            None => {
                self.active_branch_entry_records(session_id, body_mode)
                    .await?
            }
        };
        let status = match base_leaf_id {
            Some(_) if entries.is_empty() => ActiveBranchSyncStatus::BranchChanged,
            _ => ActiveBranchSyncStatus::Extended,
        };
        Ok(ActiveBranchSync {
            session_id: session_id.to_string(),
            base_leaf_id: base_leaf_id.map(ToOwned::to_owned),
            active_leaf_id: Some(active_leaf_id),
            status,
            entries,
        })
    }

    pub async fn has_transcript_entries(&self, session_id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "select exists(select 1 from transcript_entries where session_id=$1)",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn active_leaf_is_turn_boundary(&self, session_id: &str) -> Result<bool> {
        let active_leaf_id = self.active_leaf_id(session_id).await?;
        self.transcript_leaf_is_turn_boundary(session_id, active_leaf_id.as_deref())
            .await
    }

    pub async fn transcript_leaf_is_turn_boundary(
        &self,
        session_id: &str,
        leaf_id: Option<&str>,
    ) -> Result<bool> {
        let Some(leaf_id) = leaf_id else {
            return Ok(true);
        };
        let item: Option<Value> = sqlx::query_scalar(
            "select item from transcript_entries where session_id=$1 and id=$2::text",
        )
        .bind(session_id)
        .bind(leaf_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(item) = item else {
            return Ok(false);
        };
        let item: TranscriptItem = serde_json::from_value(item)?;
        Ok(is_switch_boundary_item(&item))
    }

    pub async fn history_tree(&self, session_id: &str) -> Result<HistoryTree> {
        let active_leaf_id = self.active_leaf_id(session_id).await?;
        let entries = self
            .transcript_entry_records(session_id, TranscriptEntryBodyMode::Ui)
            .await?;
        Ok(HistoryTree {
            session_id: session_id.to_string(),
            active_leaf_id,
            entries,
        })
    }

    pub async fn active_branch(&self, session_id: &str) -> Result<HistoryTree> {
        let active_leaf_id = self.active_leaf_id(session_id).await?;
        let entries = match active_leaf_id.as_deref() {
            Some(_) => {
                self.active_branch_entry_records(session_id, TranscriptEntryBodyMode::Ui)
                    .await?
            }
            None => Vec::new(),
        };
        Ok(HistoryTree {
            session_id: session_id.to_string(),
            active_leaf_id,
            entries,
        })
    }

    pub async fn transcript_entries_for_scope(
        &self,
        session_id: &str,
        scope: TranscriptEntryScope,
        body_mode: TranscriptEntryBodyMode,
    ) -> Result<Vec<TranscriptEntryRecord>> {
        match scope {
            TranscriptEntryScope::FullTree => {
                self.transcript_entry_records(session_id, body_mode).await
            }
            TranscriptEntryScope::ActiveBranch => {
                self.active_branch_entry_records(session_id, body_mode)
                    .await
            }
        }
    }

    pub async fn transcript_entries_by_id(
        &self,
        session_id: &str,
        entry_ids: &[String],
        body_mode: TranscriptEntryBodyMode,
    ) -> Result<TranscriptEntriesResult> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read read only")
            .execute(&mut *tx)
            .await?;
        let session =
            sqlx::query("select session_revision, transcript_revision from sessions where id=$1")
                .bind(session_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let entries = if entry_ids.is_empty() {
            Vec::new()
        } else {
            transcript_entry_records_by_id_tx(&mut tx, session_id, entry_ids, body_mode).await?
        };
        tx.commit().await?;
        Ok(TranscriptEntriesResult {
            session_id: session_id.to_string(),
            session_revision: session.get("session_revision"),
            transcript_revision: session.get("transcript_revision"),
            entries,
        })
    }

    pub async fn transcript_tree_index(
        &self,
        session_id: &str,
        after_sequence: Option<i64>,
        limit: Option<i64>,
    ) -> Result<TranscriptTreeIndex> {
        let after_sequence = after_sequence.unwrap_or_default().max(0);
        let limit = limit
            .unwrap_or(DEFAULT_TRANSCRIPT_INDEX_LIMIT)
            .clamp(1, MAX_TRANSCRIPT_INDEX_LIMIT);
        let mut tx = self.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read read only")
            .execute(&mut *tx)
            .await?;
        let session = sqlx::query(
            r#"
            select active_leaf_id, session_revision, transcript_revision
            from sessions
            where id=$1
            "#,
        )
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let max_sequence: i64 = sqlx::query_scalar(
            "select coalesce(max(sequence),0)::bigint from transcript_entries where session_id=$1",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;
        let rows = sqlx::query(
            r#"
            select id, parent_id, timestamp_ms, sequence, item
            from transcript_entries
            where session_id=$1 and sequence>$2
            order by sequence
            limit $3
            "#,
        )
        .bind(session_id)
        .bind(after_sequence)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        let mut nodes = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row.get::<String, _>("id");
            let parent_id = row.get::<Option<String>, _>("parent_id");
            let timestamp_ms = row.get::<i64, _>("timestamp_ms") as u64;
            let sequence = row.get::<i64, _>("sequence");
            let item: TranscriptItem = serde_json::from_value(row.get("item"))?;
            nodes.push(tree_node_from_item(
                id,
                parent_id,
                timestamp_ms,
                sequence,
                &item,
            ));
        }
        let last_sequence = nodes
            .last()
            .map(|node| node.sequence)
            .unwrap_or(after_sequence);
        tx.commit().await?;
        Ok(TranscriptTreeIndex {
            session_id: session_id.to_string(),
            active_leaf_id: session.get("active_leaf_id"),
            session_revision: session.get("session_revision"),
            transcript_revision: session.get("transcript_revision"),
            after_sequence,
            max_sequence,
            complete: last_sequence >= max_sequence,
            nodes,
        })
    }

    pub async fn transcript_turns(
        &self,
        session_id: &str,
        before_entry_id: Option<&str>,
        limit: Option<i64>,
    ) -> Result<TranscriptTurnsResult> {
        let limit = limit
            .unwrap_or(DEFAULT_TRANSCRIPT_TURN_LIMIT)
            .clamp(1, MAX_TRANSCRIPT_TURN_LIMIT);
        let mut tx = self.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read read only")
            .execute(&mut *tx)
            .await?;
        let session = sqlx::query(
            r#"
            select active_leaf_id, session_revision, transcript_revision
            from sessions
            where id=$1
            "#,
        )
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let active_leaf_id: Option<String> = session.get("active_leaf_id");
        let rows = if active_leaf_id.is_some() || before_entry_id.is_some() {
            active_branch_turn_card_rows_tx(&mut tx, session_id, before_entry_id, limit).await?
        } else {
            Vec::new()
        };
        let next_before_entry_id = rows
            .first()
            .and_then(TurnCardRow::display_parent_id)
            .map(ToOwned::to_owned);
        let has_more_before = next_before_entry_id.is_some();
        let cards = turn_cards_from_rows(&rows);
        tx.commit().await?;
        Ok(TranscriptTurnsResult {
            session_id: session_id.to_string(),
            active_leaf_id,
            session_revision: session.get("session_revision"),
            transcript_revision: session.get("transcript_revision"),
            before_entry_id: before_entry_id.map(ToOwned::to_owned),
            next_before_entry_id,
            has_more_before,
            limit,
            cards,
        })
    }

    pub async fn transcript_turn_detail(
        &self,
        session_id: &str,
        card_id: &str,
        leaf_id: &str,
        start_sequence: i64,
        end_sequence: i64,
        body_mode: TranscriptEntryBodyMode,
    ) -> Result<TranscriptTurnDetailResult> {
        if start_sequence > end_sequence {
            return Err(anyhow!(
                "invalid turn-card sequence range: {start_sequence}..{end_sequence}"
            ));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read read only")
            .execute(&mut *tx)
            .await?;
        let session = sqlx::query(
            r#"
            select active_leaf_id, session_revision, transcript_revision
            from sessions
            where id=$1
            "#,
        )
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let active_leaf_id: Option<String> = session.get("active_leaf_id");
        let detail_entries = active_branch_entry_records_between_tx(
            &mut tx,
            session_id,
            leaf_id,
            start_sequence,
            end_sequence,
            body_mode,
        )
        .await?;
        if detail_entries.is_empty()
            || detail_entries.first().map(|entry| entry.sequence) != Some(start_sequence)
            || detail_entries.last().map(|entry| entry.sequence) != Some(end_sequence)
            || !detail_entries.iter().any(|entry| entry.id == card_id)
        {
            tx.commit().await?;
            return Err(anyhow!("turn card detail not found: {card_id}"));
        }
        tx.commit().await?;
        Ok(TranscriptTurnDetailResult {
            session_id: session_id.to_string(),
            active_leaf_id,
            session_revision: session.get("session_revision"),
            transcript_revision: session.get("transcript_revision"),
            card_id: card_id.to_string(),
            entries: detail_entries,
        })
    }

    pub async fn model_context_for_leaf(
        &self,
        session_id: &str,
        leaf_id: &str,
    ) -> Result<ModelContext> {
        let entries = self.branch_entries_to_leaf(session_id, leaf_id).await?;
        if entries.is_empty() {
            return Err(anyhow!("transcript leaf not found: {leaf_id}"));
        }
        Ok(model_context_from_entries(entries))
    }

    pub async fn active_leaf_id(&self, session_id: &str) -> Result<Option<String>> {
        sqlx::query_scalar("select active_leaf_id from sessions where id=$1")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))
    }

    async fn stored_transcript_entries(
        &self,
        session_id: &str,
    ) -> Result<Vec<StoredTranscriptEntry>> {
        let rows = sqlx::query(
            "select id, parent_id, timestamp_ms, item, provider_replay from transcript_entries where session_id=$1 order by sequence",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| row_to_stored_entry(&row))
            .collect()
    }

    async fn transcript_entry_records(
        &self,
        session_id: &str,
        body_mode: TranscriptEntryBodyMode,
    ) -> Result<Vec<TranscriptEntryRecord>> {
        let provider_replay_select = provider_replay_select(body_mode);
        let query = format!(
            "select id, parent_id, timestamp_ms, sequence, item, {provider_replay_select} from transcript_entries where session_id=$1 order by sequence",
        );
        let rows = sqlx::query(&query)
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| row_to_transcript_entry(&row))
            .collect()
    }

    async fn active_branch_entry_records_after(
        &self,
        session_id: &str,
        base_leaf_id: &str,
        body_mode: TranscriptEntryBodyMode,
    ) -> Result<Vec<TranscriptEntryRecord>> {
        let provider_replay_select = provider_replay_select(body_mode);
        let leaf_provider_replay_select = aliased_provider_replay_select("t", body_mode);
        let parent_provider_replay_select = aliased_provider_replay_select("parent", body_mode);
        let query = format!(
            r#"
            with recursive branch as (
                select t.id, t.parent_id, t.timestamp_ms, t.item, {leaf_provider_replay_select}, t.sequence, 0 as depth
                from transcript_entries t
                join sessions s on s.id = t.session_id and s.active_leaf_id = t.id
                where t.session_id = $1

                union all

                select parent.id, parent.parent_id, parent.timestamp_ms, parent.item, {parent_provider_replay_select}, parent.sequence, child.depth + 1
                from transcript_entries parent
                join branch child
                  on parent.session_id = $1
                 and parent.id = coalesce(
                    case
                        when child.item->>'type' = 'compaction_summary' then child.item->>'source_leaf_id'
                        else null
                    end,
                    child.parent_id
                 )
                where child.id <> $2::text
            ),
            base as (
                select depth from branch where id = $2::text
            )
            select branch.id, branch.parent_id, branch.timestamp_ms, branch.sequence, branch.item, {provider_replay_select}
            from branch, base
            where branch.depth < base.depth
            order by branch.depth desc
            "#
        );
        let rows = sqlx::query(&query)
            .bind(session_id)
            .bind(base_leaf_id)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| row_to_transcript_entry(&row))
            .collect()
    }

    async fn active_branch_entry_records(
        &self,
        session_id: &str,
        body_mode: TranscriptEntryBodyMode,
    ) -> Result<Vec<TranscriptEntryRecord>> {
        let mut tx = self.pool.begin().await?;
        let records = active_branch_entry_records_tx(&mut tx, session_id, body_mode).await?;
        tx.commit().await?;
        Ok(records)
    }

    pub(crate) async fn branch_entries_to_leaf(
        &self,
        session_id: &str,
        leaf_id: &str,
    ) -> Result<Vec<StoredTranscriptEntry>> {
        let rows = sqlx::query(
            r#"
            with recursive branch as (
                select t.id, t.parent_id, t.timestamp_ms, t.item, t.provider_replay, t.sequence
                from transcript_entries t
                where t.session_id = $1
                  and t.id = $2::text

                union all

                select parent.id, parent.parent_id, parent.timestamp_ms, parent.item, parent.provider_replay, parent.sequence
                from transcript_entries parent
                join branch child
                  on parent.session_id = $1
                 and parent.id = child.parent_id
            )
            select id, parent_id, timestamp_ms, item, provider_replay
            from branch
            order by sequence
            "#,
        )
        .bind(session_id)
        .bind(leaf_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| row_to_stored_entry(&row))
            .collect()
    }

    pub async fn set_active_leaf(
        &self,
        session_id: &str,
        leaf_id: Option<&str>,
    ) -> Result<Vec<EventFrame>> {
        let result = self
            .switch_active_leaf(session_id, leaf_id, false, None, None, None)
            .await?;
        Ok(result.events)
    }

    pub async fn switch_active_leaf(
        &self,
        session_id: &str,
        leaf_id: Option<&str>,
        return_active_branch: bool,
        expected_transcript_revision: Option<i64>,
        expected_active_branch_entry_ids: Option<&[String]>,
        missing_body_ids: Option<&[String]>,
    ) -> Result<SwitchActiveLeafResult> {
        let expected_active_branch_entry_ids =
            expected_active_branch_entry_ids.filter(|ids| !ids.is_empty());
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let current_transcript_revision: i64 =
            sqlx::query_scalar("select transcript_revision from sessions where id=$1")
                .bind(session_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        if let Some(expected) = expected_transcript_revision {
            if current_transcript_revision != expected {
                return Err(anyhow!(
                    "history_changed: transcript revision changed before history.switch was applied"
                ));
            }
        }
        if let Some(leaf_id) = leaf_id {
            let belongs_to_session: bool = sqlx::query_scalar(
                "select exists(select 1 from transcript_entries where session_id=$1 and id=$2::text)",
            )
            .bind(session_id)
            .bind(leaf_id)
            .fetch_one(&mut *tx)
            .await?;
            if !belongs_to_session {
                return Err(anyhow!("active leaf does not belong to session: {leaf_id}"));
            }
        }
        sqlx::query("update sessions set active_leaf_id=$2::text, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(leaf_id)
            .execute(&mut *tx)
            .await?;
        bump_revisions_tx(&mut tx, session_id, false, false).await?;
        let state = session_state_for_event_tx(&mut tx, session_id).await?;
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::HistorySwitched,
            json!({
                "active_leaf_id": leaf_id,
                "activity": state.activity,
                "session_revision": state.session_revision,
                "queue_revision": state.queue_revision,
                "transcript_revision": state.transcript_revision,
            }),
        )
        .await?;
        let last_event_id = event.event_id;
        let expected_ids = expected_active_branch_entry_ids.map(|ids| ids.to_vec());
        let active_branch_entry_ids =
            if return_active_branch || expected_ids.is_some() || missing_body_ids.is_some() {
                Some(active_branch_entry_ids_tx(&mut tx, session_id).await?)
            } else {
                None
            };
        if let (Some(expected_ids), Some(active_branch_entry_ids)) =
            (expected_ids.as_deref(), active_branch_entry_ids.as_deref())
        {
            if expected_ids != active_branch_entry_ids {
                return Err(anyhow!(
                    "history_changed: target branch changed before history.switch was applied"
                ));
            }
        }
        let active_branch_entries = if let (Some(missing_ids), Some(active_branch_entry_ids)) =
            (missing_body_ids, active_branch_entry_ids.as_deref())
        {
            let branch_ids = active_branch_entry_ids
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let requested_branch_ids = missing_ids
                .iter()
                .filter(|entry_id| branch_ids.contains(entry_id.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            Some(
                transcript_entry_records_by_id_tx(
                    &mut tx,
                    session_id,
                    &requested_branch_ids,
                    TranscriptEntryBodyMode::Ui,
                )
                .await?,
            )
        } else if return_active_branch {
            Some(
                transcript_entry_records_by_id_tx(
                    &mut tx,
                    session_id,
                    active_branch_entry_ids.as_deref().unwrap_or(&[]),
                    TranscriptEntryBodyMode::Ui,
                )
                .await?,
            )
        } else {
            None
        };
        tx.commit().await?;
        let should_return_branch_ids =
            expected_active_branch_entry_ids.is_some() || missing_body_ids.is_some();
        let active_branch_entry_ids = active_branch_entry_ids.filter(|_| should_return_branch_ids);
        Ok(SwitchActiveLeafResult {
            session_id: session_id.to_string(),
            active_leaf_id: leaf_id.map(str::to_string),
            activity: state.activity,
            session_revision: state.session_revision,
            queue_revision: state.queue_revision,
            transcript_revision: state.transcript_revision,
            last_event_id,
            active_branch_entry_ids,
            active_branch_entries,
            events: vec![event],
        })
    }

    pub async fn recover_session(
        &self,
        session_id: &str,
        entries: &[StoredTranscriptEntry],
        active_leaf_id: Option<&str>,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let mut inserted_records = HashMap::new();
        for entry in entries {
            if let Some(record) = insert_stored_entry_tx(&mut tx, session_id, entry).await? {
                inserted_records.insert(record.id.clone(), record);
            }
        }
        let unfinished_actions = action_is_unfinished(None);
        let query = format!(
            "update actions set status='stale', updated_at=now() where session_id=$1 and {unfinished_actions}",
        );
        sqlx::query(&query)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("update sessions set active_leaf_id=$2::text, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(active_leaf_id)
            .execute(&mut *tx)
            .await?;
        bump_revisions_tx(&mut tx, session_id, false, true).await?;
        let state = session_state_for_event_tx(&mut tx, session_id).await?;
        let mut frames = Vec::new();
        for entry in entries {
            frames.extend(
                insert_transcript_item_events_tx(
                    &mut tx,
                    session_id,
                    Some(&state),
                    inserted_records.get(entry.id.as_str()),
                    &entry.id,
                    &entry.item,
                )
                .await?,
            );
        }
        frames.push(
            insert_event_tx(
                &mut tx,
                session_id,
                EventType::SessionRecovered,
                json!({
                    "active_leaf_id": active_leaf_id,
                    "session_revision": state.session_revision,
                    "queue_revision": state.queue_revision,
                    "transcript_revision": state.transcript_revision,
                    "activity": state.activity,
                }),
            )
            .await?,
        );
        tx.commit().await?;
        Ok(frames)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionEventState {
    pub(crate) active_leaf_id: Option<String>,
    pub(crate) session_revision: i64,
    pub(crate) queue_revision: i64,
    pub(crate) transcript_revision: i64,
    pub(crate) activity: SessionActivity,
}

pub(crate) async fn session_state_for_event_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
) -> Result<SessionEventState> {
    let row = sqlx::query(
        r#"
        select active_leaf_id, session_revision, queue_revision, transcript_revision,
            exists(select 1 from actions where session_id=$1 and status in ('pending','blocked','running')) as has_running_work,
            exists(select 1 from queued_inputs where session_id=$1 and status in ('queued','consuming')) as has_queued_input
        from sessions
        where id=$1
        "#,
    )
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
    let activity = if row.get::<bool, _>("has_running_work") {
        SessionActivity::Running
    } else if row.get::<bool, _>("has_queued_input") {
        SessionActivity::Queued
    } else {
        SessionActivity::Idle
    };
    Ok(SessionEventState {
        active_leaf_id: row.get("active_leaf_id"),
        session_revision: row.get("session_revision"),
        queue_revision: row.get("queue_revision"),
        transcript_revision: row.get("transcript_revision"),
        activity,
    })
}

pub(crate) async fn transcript_entry_record_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry_id: &str,
    body_mode: TranscriptEntryBodyMode,
) -> Result<Option<TranscriptEntryRecord>> {
    let provider_replay_select = provider_replay_select(body_mode);
    let query = format!(
        r#"
        select id, parent_id, timestamp_ms, sequence, item, {provider_replay_select}
        from transcript_entries
        where session_id=$1 and id=$2::text
        "#
    );
    let row = sqlx::query(&query)
        .bind(session_id)
        .bind(entry_id)
        .fetch_optional(&mut **tx)
        .await?;
    row.map(|row| row_to_transcript_entry(&row)).transpose()
}

pub(crate) fn tree_node_from_entry(entry: &TranscriptEntryRecord) -> TranscriptTreeNodeRecord {
    tree_node_from_item(
        entry.id.clone(),
        entry.parent_id.clone(),
        entry.timestamp_ms,
        entry.sequence,
        &entry.item,
    )
}

fn tree_node_from_item(
    id: String,
    parent_id: Option<String>,
    timestamp_ms: u64,
    sequence: i64,
    item: &TranscriptItem,
) -> TranscriptTreeNodeRecord {
    let item_type = item_type(item).to_string();
    let turn_id = item.turn_id();
    let outcome = match item {
        TranscriptItem::TurnFinished { outcome, .. } => Some(*outcome),
        _ => None,
    };
    let source_leaf_id = match item {
        TranscriptItem::CompactionSummary(summary) => Some(summary.source_leaf_id.clone()),
        _ => None,
    };
    let can_switch_to = is_switch_boundary_item(item);
    let edit_target_leaf_id = match item {
        TranscriptItem::UserMessage(_) => previous_boundary_leaf_id_placeholder(&parent_id),
        _ => None,
    };
    TranscriptTreeNodeRecord {
        id,
        parent_id,
        source_leaf_id,
        timestamp_ms,
        sequence,
        item_type,
        turn_id,
        outcome,
        can_switch_to,
        edit_target_leaf_id,
        display_hint: display_hint(item),
    }
}

fn previous_boundary_leaf_id_placeholder(parent_id: &Option<String>) -> Option<String> {
    // The daemon cannot infer the previous boundary without walking ancestors for
    // every row in the compact index. Returning the parent is not correct, so keep
    // this nullable and let the frontend derive the exact previous boundary from
    // compact topology. The backend remains authoritative for switchable boundary
    // rows via `can_switch_to` and history.switch validation.
    let _ = parent_id;
    None
}

fn item_type(item: &TranscriptItem) -> &'static str {
    match item {
        TranscriptItem::TurnStarted { .. } => "turn_started",
        TranscriptItem::UserMessage(_) => "user_message",
        TranscriptItem::AssistantMessage(_) => "assistant_message",
        TranscriptItem::ToolCallStarted { .. } => "tool_call_started",
        TranscriptItem::ToolResult(_) => "tool_result",
        TranscriptItem::TurnFinished { .. } => "turn_finished",
        TranscriptItem::CompactionSummary(_) => "compaction_summary",
    }
}

fn is_switch_boundary_item(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::TurnFinished { .. } | TranscriptItem::CompactionSummary(_)
    )
}

fn display_hint(item: &TranscriptItem) -> Option<String> {
    let text = match item {
        TranscriptItem::TurnStarted { turn_id } => format!("start turn {}", turn_id.0),
        TranscriptItem::UserMessage(message) => content_blocks_text(&message.content),
        TranscriptItem::AssistantMessage(message) => message
            .items
            .iter()
            .map(|item| match item {
                agent_vocab::AssistantItem::Text(text) => text.clone(),
                agent_vocab::AssistantItem::ToolCall(call) => {
                    format!("tool call: {}", call.tool_name)
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        TranscriptItem::ToolCallStarted { tool_call, .. } => {
            format!("tool call: {}", tool_call.tool_name)
        }
        TranscriptItem::ToolResult(result) => format!("{}: {}", result.tool_name, result.output),
        TranscriptItem::TurnFinished { turn_id, outcome } => {
            format!("{outcome:?} turn boundary for turn {}", turn_id.0)
        }
        TranscriptItem::CompactionSummary(summary) => summary.summary.clone(),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(truncate_chars(trimmed, 160))
    }
}

fn content_blocks_text(blocks: &[agent_vocab::ContentBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            agent_vocab::ContentBlock::Text { text } => text.clone(),
            agent_vocab::ContentBlock::Image { .. } => "[image]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn turn_card_row(row: &sqlx::postgres::PgRow) -> Result<TurnCardRow> {
    let item_type: String = row.get("item_type");
    let mut turn_id = row
        .get::<Option<Value>, _>("turn_id")
        .and_then(|value| value.as_u64().map(agent_vocab::TurnId));
    let compaction_last_turn_id = row
        .get::<Option<String>, _>("compaction_last_turn_id")
        .as_deref()
        .and_then(parse_u64)
        .map(agent_vocab::TurnId);
    if item_type == "compaction_summary" {
        turn_id = compaction_last_turn_id;
    }
    let outcome = row
        .get::<Option<Value>, _>("outcome")
        .and_then(|value| value.as_str().and_then(parse_turn_outcome));
    let summary = if item_type == "compaction_summary" {
        row.get::<Option<String>, _>("compaction_summary")
    } else {
        None
    };
    let body_item = row
        .get::<Option<Value>, _>("body_item")
        .map(serde_json::from_value)
        .transpose()?;
    let entry = match (item_type.as_str(), body_item) {
        ("user_message" | "assistant_message", Some(item)) => Some(TranscriptEntryRecord {
            id: row.get("id"),
            parent_id: row.get("parent_id"),
            timestamp_ms: row.get::<i64, _>("timestamp_ms") as u64,
            sequence: row.get("sequence"),
            item,
            provider_replay: Vec::new(),
        }),
        ("user_message", None) => {
            return Err(anyhow!("turn-card row missing body item for user_message"));
        }
        ("assistant_message", None) => None,
        _ => None,
    };
    Ok(TurnCardRow {
        id: row.get("id"),
        parent_id: row.get("parent_id"),
        compaction_source_leaf_id: row.get("compaction_source_leaf_id"),
        timestamp_ms: row.get::<i64, _>("timestamp_ms") as u64,
        sequence: row.get("sequence"),
        entry,
        item_type,
        turn_id,
        compaction_turn_started_at_ms: row
            .get::<Option<String>, _>("compaction_turn_started_at_ms")
            .as_deref()
            .and_then(parse_u64),
        outcome,
        summary,
    })
}

fn parse_u64(value: &str) -> Option<u64> {
    value.parse::<u64>().ok()
}

fn parse_turn_outcome(value: &str) -> Option<agent_vocab::TurnOutcome> {
    match value {
        "Graceful" => Some(agent_vocab::TurnOutcome::Graceful),
        "Interrupted" => Some(agent_vocab::TurnOutcome::Interrupted),
        "Crashed" => Some(agent_vocab::TurnOutcome::Crashed),
        _ => None,
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut truncated = String::new();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return value.to_string();
        };
        truncated.push(ch);
    }
    if chars.next().is_some() {
        truncated.push('…');
    }
    truncated
}

#[derive(Debug)]
struct TurnCardRow {
    id: String,
    parent_id: Option<String>,
    compaction_source_leaf_id: Option<String>,
    timestamp_ms: u64,
    sequence: i64,
    entry: Option<TranscriptEntryRecord>,
    item_type: String,
    turn_id: Option<agent_vocab::TurnId>,
    compaction_turn_started_at_ms: Option<u64>,
    outcome: Option<agent_vocab::TurnOutcome>,
    summary: Option<String>,
}

impl TurnCardRow {
    fn entry_record(&self) -> Option<TranscriptEntryRecord> {
        self.entry.clone()
    }

    fn display_parent_id(&self) -> Option<&str> {
        if self.item_type == "compaction_summary" {
            self.compaction_source_leaf_id.as_deref()
        } else {
            self.parent_id.as_deref()
        }
    }
}

#[derive(Debug)]
struct MutableTurnCard {
    turn_id: Option<agent_vocab::TurnId>,
    start_entry_id: Option<String>,
    boundary_entry_id: Option<String>,
    active_leaf_id: String,
    start_sequence: i64,
    end_sequence: i64,
    start_timestamp_ms: u64,
    timestamp_ms: u64,
    user_messages: Vec<TranscriptEntryRecord>,
    assistant_message: Option<TranscriptEntryRecord>,
    summary: Option<String>,
    status: TurnCardStatus,
    outcome: Option<agent_vocab::TurnOutcome>,
}

#[derive(Debug, Clone, Copy)]
struct CompactionResumeAnchor {
    turn_id: Option<agent_vocab::TurnId>,
    start_timestamp_ms: Option<u64>,
}

fn turn_cards_from_rows(rows: &[TurnCardRow]) -> Vec<TurnCardRecord> {
    let mut cards = Vec::new();
    let mut current: Option<MutableTurnCard> = None;
    let mut compaction_resume_anchor: Option<CompactionResumeAnchor> = None;

    for row in rows {
        if row.item_type == "compaction_summary" {
            close_open_turn(&mut cards, &mut current);
            cards.push(compaction_turn_card(row));
            compaction_resume_anchor = Some(CompactionResumeAnchor {
                turn_id: row.turn_id,
                start_timestamp_ms: row.compaction_turn_started_at_ms,
            });
            continue;
        }
        if row.item_type == "turn_started" {
            close_open_turn(&mut cards, &mut current);
            compaction_resume_anchor = None;
            current = Some(MutableTurnCard {
                turn_id: row.turn_id,
                start_entry_id: Some(row.id.clone()),
                boundary_entry_id: None,
                active_leaf_id: row.id.clone(),
                start_sequence: row.sequence,
                end_sequence: row.sequence,
                start_timestamp_ms: row.timestamp_ms,
                timestamp_ms: row.timestamp_ms,
                user_messages: Vec::new(),
                assistant_message: None,
                summary: None,
                status: TurnCardStatus::Open,
                outcome: None,
            });
        }

        let anchor = compaction_resume_anchor;
        let turn = current.get_or_insert_with(|| MutableTurnCard {
            turn_id: row.turn_id.or(anchor.and_then(|anchor| anchor.turn_id)),
            start_entry_id: None,
            boundary_entry_id: None,
            active_leaf_id: row.id.clone(),
            start_sequence: row.sequence,
            end_sequence: row.sequence,
            start_timestamp_ms: anchor
                .and_then(|anchor| anchor.start_timestamp_ms)
                .unwrap_or(row.timestamp_ms),
            timestamp_ms: row.timestamp_ms,
            user_messages: Vec::new(),
            assistant_message: None,
            summary: None,
            status: TurnCardStatus::Open,
            outcome: None,
        });
        append_entry_to_turn_card(turn, row);
        if row.item_type == "turn_finished" {
            close_open_turn(&mut cards, &mut current);
        }
    }
    close_open_turn(&mut cards, &mut current);

    cards
}

fn append_entry_to_turn_card(card: &mut MutableTurnCard, row: &TurnCardRow) {
    card.active_leaf_id = row.id.clone();
    card.end_sequence = row.sequence;
    card.timestamp_ms = row.timestamp_ms;
    if card.turn_id.is_none() {
        card.turn_id = row.turn_id;
    }
    match row.item_type.as_str() {
        "user_message" => {
            if let Some(entry) = row.entry_record() {
                card.user_messages.push(entry);
            }
        }
        "assistant_message" => {
            if let Some(entry) = row.entry_record() {
                card.assistant_message = Some(entry);
            }
        }
        "turn_finished" => {
            if let Some(turn_id) = row.turn_id {
                card.turn_id = Some(turn_id);
            }
            card.boundary_entry_id = Some(row.id.clone());
            card.status = TurnCardStatus::Completed;
            card.outcome = row.outcome;
        }
        "turn_started" => {
            if let Some(turn_id) = row.turn_id {
                card.turn_id = Some(turn_id);
            }
            card.start_entry_id.get_or_insert_with(|| row.id.clone());
        }
        _ => {}
    }
}

fn close_open_turn(cards: &mut Vec<TurnCardRecord>, current: &mut Option<MutableTurnCard>) {
    let Some(card) = current.take() else {
        return;
    };
    cards.push(finalize_turn_card(card));
}

fn finalize_turn_card(card: MutableTurnCard) -> TurnCardRecord {
    let id = card
        .boundary_entry_id
        .clone()
        .or_else(|| card.start_entry_id.clone())
        .unwrap_or_else(|| card.active_leaf_id.clone());
    let can_resume = matches!(
        card.outcome,
        Some(agent_vocab::TurnOutcome::Interrupted | agent_vocab::TurnOutcome::Crashed)
    );
    TurnCardRecord {
        id,
        turn_id: card.turn_id,
        status: card.status,
        outcome: card.outcome,
        start_entry_id: card.start_entry_id,
        boundary_entry_id: card.boundary_entry_id,
        active_leaf_id: card.active_leaf_id,
        start_sequence: card.start_sequence,
        end_sequence: card.end_sequence,
        start_timestamp_ms: card.start_timestamp_ms,
        timestamp_ms: card.timestamp_ms,
        user_messages: card.user_messages,
        assistant_message: card.assistant_message,
        summary: card.summary,
        can_resume,
    }
}

fn compaction_turn_card(row: &TurnCardRow) -> TurnCardRecord {
    let summary = row.summary.as_ref().map(|value| value.trim().to_string());
    TurnCardRecord {
        id: row.id.clone(),
        turn_id: row.turn_id,
        status: TurnCardStatus::Compacted,
        outcome: None,
        start_entry_id: Some(row.id.clone()),
        boundary_entry_id: Some(row.id.clone()),
        active_leaf_id: row.id.clone(),
        start_sequence: row.sequence,
        end_sequence: row.sequence,
        start_timestamp_ms: row
            .compaction_turn_started_at_ms
            .unwrap_or(row.timestamp_ms),
        timestamp_ms: row.timestamp_ms,
        user_messages: Vec::new(),
        assistant_message: None,
        summary: summary.filter(|value| !value.is_empty()),
        can_resume: false,
    }
}

pub(super) async fn insert_entry_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry: &TranscriptStorageNode,
) -> Result<Option<TranscriptEntryRecord>> {
    let stored = StoredTranscriptEntry {
        id: entry.id.clone(),
        parent_id: entry.parent_id.clone(),
        timestamp_ms: entry.timestamp_ms,
        item: entry.item.clone(),
        provider_replay: entry.provider_replay.clone(),
    };
    insert_stored_entry_tx(tx, session_id, &stored).await
}

pub(super) async fn insert_stored_entry_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry: &StoredTranscriptEntry,
) -> Result<Option<TranscriptEntryRecord>> {
    let turn_id = entry.item.turn_id().map(|turn_id| turn_id.0 as i64);
    let row = sqlx::query(
        r#"
        insert into transcript_entries (session_id, id, parent_id, timestamp_ms, item, provider_replay, turn_id)
        values ($1::text, $2::text, $3::text, $4, $5, $6, $7::bigint)
        on conflict (session_id, id) do nothing
        returning id, parent_id, timestamp_ms, sequence, item
        "#,
    )
    .bind(session_id)
    .bind(&entry.id)
    .bind(&entry.parent_id)
    .bind(entry.timestamp_ms as i64)
    .bind(serde_json::to_value(&entry.item)?)
    .bind(serde_json::to_value(&entry.provider_replay)?)
    .bind(turn_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| {
        Ok(TranscriptEntryRecord {
            id: row.get("id"),
            parent_id: row.get("parent_id"),
            timestamp_ms: row.get::<i64, _>("timestamp_ms") as u64,
            sequence: row.get("sequence"),
            item: serde_json::from_value(row.get("item"))?,
            provider_replay: Vec::new(),
        })
    })
    .transpose()
}

pub(super) fn model_context_from_entries(entries: Vec<StoredTranscriptEntry>) -> ModelContext {
    ModelContext::from_entries(
        entries
            .into_iter()
            .map(|entry| ModelContextEntry {
                item: entry.item,
                provider_replay: entry.provider_replay,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_session::{SessionEvent, TranscriptStorageNode};
    use agent_vocab::{
        AssistantItem, AssistantMessage, CompactionSummary, ProviderConfig, ProviderKind,
        ProviderReplayItem, ReasoningEffort, ToolCallId, ToolResultMessage, TranscriptItem, TurnId,
        TurnOutcome, UserMessage,
    };
    use serde_json::json;
    use uuid::Uuid;

    use crate::{OutputBatch, SessionConfig};

    use super::*;

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(30_000);

    struct TestDb {
        store: PostgresAgentStore,
        admin_url: String,
        name: String,
    }

    impl TestDb {
        async fn cleanup(self) {
            self.store.close().await;
            if let Ok(admin) = sqlx::PgPool::connect(&self.admin_url).await {
                let _ = sqlx::query(&format!(r#"drop database if exists "{}""#, self.name))
                    .execute(&admin)
                    .await;
                admin.close().await;
            }
        }
    }

    async fn test_store() -> Option<TestDb> {
        let admin_url = std::env::var("PI_RELAY_TEST_DATABASE_URL").ok()?;
        let name = format!(
            "pi_relay_transcript_test_{}_{}",
            std::process::id(),
            TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = sqlx::PgPool::connect(&admin_url)
            .await
            .expect("connect to PI_RELAY_TEST_DATABASE_URL");
        sqlx::query(&format!(r#"create database "{name}""#))
            .execute(&admin)
            .await
            .expect("create isolated test database");
        admin.close().await;
        let database_url = database_url_with_name(&admin_url, &name);
        let store = PostgresAgentStore::connect(&database_url)
            .await
            .expect("connect isolated test database");
        store
            .migrate()
            .await
            .expect("migrate isolated test database");
        Some(TestDb {
            store,
            admin_url,
            name,
        })
    }

    fn database_url_with_name(base: &str, name: &str) -> String {
        let (prefix, query) = base
            .split_once('?')
            .map(|(prefix, query)| (prefix, format!("?{query}")))
            .unwrap_or((base, String::new()));
        let Some((root, _)) = prefix.rsplit_once('/') else {
            return format!("{base}_{name}");
        };
        format!("{root}/{name}{query}")
    }

    fn session_config(project_id: Uuid) -> SessionConfig {
        SessionConfig {
            project_id: Some(project_id),
            outer_cwd: "/tmp".to_string(),
            workspaces: Vec::new(),
            system_prompt: "test prompt".to_string(),
            provider: ProviderConfig {
                kind: ProviderKind::OpenAi,
                model: "test-model".to_string(),
                reasoning_effort: ReasoningEffort::Medium,
                max_tokens: None,
                prompt_cache: None,
            },
            metadata: json!({}),
        }
    }

    async fn create_session(store: &PostgresAgentStore, session_id: &str) {
        let project_id = Uuid::new_v4();
        store
            .create_project(project_id, "transcript test", &[], json!({}))
            .await
            .expect("project creates");
        store
            .create_session(session_id, &session_config(project_id))
            .await
            .expect("session creates");
    }

    fn entry(id: &str, parent_id: Option<&str>, item: TranscriptItem) -> TranscriptStorageNode {
        TranscriptStorageNode {
            id: id.to_string(),
            parent_id: parent_id.map(str::to_string),
            timestamp_ms: 1,
            item,
            provider_replay: Vec::new(),
        }
    }

    fn turn_started(id: &str, parent_id: Option<&str>, turn_id: u64) -> TranscriptStorageNode {
        entry(
            id,
            parent_id,
            TranscriptItem::TurnStarted {
                turn_id: TurnId(turn_id),
            },
        )
    }

    fn user_message(id: &str, parent_id: Option<&str>, text: &str) -> TranscriptStorageNode {
        entry(
            id,
            parent_id,
            TranscriptItem::UserMessage(UserMessage::text(text)),
        )
    }

    fn assistant_message_with_replay(
        id: &str,
        parent_id: Option<&str>,
        text: &str,
    ) -> TranscriptStorageNode {
        TranscriptStorageNode {
            id: id.to_string(),
            parent_id: parent_id.map(str::to_string),
            timestamp_ms: 1,
            item: TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text(text.to_string())],
            }),
            provider_replay: vec![ProviderReplayItem::new(
                ProviderKind::OpenAi,
                &json!({ "type": "message", "large": "raw" }),
            )
            .expect("provider replay serializes")],
        }
    }

    fn turn_finished(id: &str, parent_id: Option<&str>, turn_id: u64) -> TranscriptStorageNode {
        entry(
            id,
            parent_id,
            TranscriptItem::TurnFinished {
                turn_id: TurnId(turn_id),
                outcome: TurnOutcome::Graceful,
            },
        )
    }

    fn compaction_summary(
        id: &str,
        parent_id: Option<&str>,
        session_id: &str,
        source_leaf_id: &str,
    ) -> TranscriptStorageNode {
        entry(
            id,
            parent_id,
            TranscriptItem::CompactionSummary(CompactionSummary::new(
                session_id,
                source_leaf_id,
                "summary",
                None,
                TurnId(0),
            )),
        )
    }

    fn assistant_message(id: &str, parent_id: Option<&str>, text: &str) -> TranscriptStorageNode {
        entry(
            id,
            parent_id,
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text(text.to_string())],
            }),
        )
    }

    fn tool_result(id: &str, parent_id: Option<&str>) -> TranscriptStorageNode {
        entry(
            id,
            parent_id,
            TranscriptItem::ToolResult(ToolResultMessage::success(
                ToolCallId::from("tool_1"),
                "Bash",
                "ok",
            )),
        )
    }

    fn appended_event(entry: &TranscriptStorageNode) -> SessionEvent {
        SessionEvent::TranscriptItemAppended {
            entry_id: entry.id.clone(),
            item: entry.item.clone(),
        }
    }

    #[tokio::test]
    async fn transcript_tree_index_paginates_and_entries_include_sequence() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "transcript-index-page";
        create_session(store, session_id).await;

        let entries = vec![
            turn_started("entry_start", None, 1),
            user_message("entry_user", Some("entry_start"), "hello"),
            turn_finished("entry_finish", Some("entry_user"), 1),
        ];
        store
            .persist_outputs(
                session_id,
                OutputBatch::new(&entries, Some("entry_finish"), &[], &[]),
            )
            .await
            .expect("transcript persists");

        let first_page = store
            .transcript_tree_index(session_id, Some(0), Some(2))
            .await
            .expect("first page loads");
        assert_eq!(first_page.nodes.len(), 2);
        assert_eq!(first_page.after_sequence, 0);
        assert_eq!(first_page.max_sequence, 3);
        assert!(!first_page.complete);
        assert_eq!(first_page.nodes[0].id, "entry_start");
        assert_eq!(first_page.nodes[0].sequence, 1);
        assert_eq!(first_page.nodes[0].item_type, "turn_started");

        let second_page = store
            .transcript_tree_index(
                session_id,
                Some(first_page.nodes.last().expect("node").sequence),
                Some(2),
            )
            .await
            .expect("second page loads");
        assert_eq!(second_page.nodes.len(), 1);
        assert!(second_page.complete);
        assert_eq!(second_page.nodes[0].id, "entry_finish");
        assert!(second_page.nodes[0].can_switch_to);

        let sparse_entries = store
            .transcript_entries_by_id(
                session_id,
                &["entry_finish".to_string(), "entry_start".to_string()],
                TranscriptEntryBodyMode::Ui,
            )
            .await
            .expect("entries by id load");
        assert_eq!(
            sparse_entries
                .entries
                .iter()
                .map(|entry| (entry.id.as_str(), entry.sequence))
                .collect::<Vec<_>>(),
            vec![("entry_start", 1), ("entry_finish", 3)]
        );

        db.cleanup().await;
    }

    #[tokio::test]
    async fn switch_active_leaf_can_return_branch_ids_and_sparse_bodies() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "switch-sparse-branch";
        create_session(store, session_id).await;

        let entries = vec![
            turn_started("entry_start", None, 1),
            user_message("entry_user", Some("entry_start"), "hello"),
            turn_finished("entry_finish", Some("entry_user"), 1),
        ];
        store
            .persist_outputs(
                session_id,
                OutputBatch::new(&entries, Some("entry_finish"), &[], &[]),
            )
            .await
            .expect("transcript persists");
        let before = store
            .session_snapshot(session_id)
            .await
            .expect("snapshot loads");
        let expected_ids = entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        let missing_ids = vec!["entry_user".to_string(), "not-on-branch".to_string()];

        let result = store
            .switch_active_leaf(
                session_id,
                Some("entry_finish"),
                false,
                Some(before.transcript_revision),
                Some(&expected_ids),
                Some(&missing_ids),
            )
            .await
            .expect("switch succeeds");

        assert_eq!(
            result.active_branch_entry_ids.as_deref(),
            Some(expected_ids.as_slice())
        );
        assert_eq!(
            result
                .active_branch_entries
                .expect("sparse entries returned")
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["entry_user"]
        );

        db.cleanup().await;
    }

    #[tokio::test]
    async fn ui_transcript_projection_omits_provider_replay() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "ui-transcript-projection";
        create_session(store, session_id).await;

        let entries = vec![
            turn_started("entry_start", None, 1),
            assistant_message_with_replay("entry_assistant", Some("entry_start"), "hello"),
        ];
        store
            .persist_outputs(
                session_id,
                OutputBatch::new(&entries, Some("entry_assistant"), &[], &[]),
            )
            .await
            .expect("transcript persists");

        let ui_entries = store
            .transcript_entries_by_id(
                session_id,
                &["entry_assistant".to_string()],
                TranscriptEntryBodyMode::Ui,
            )
            .await
            .expect("ui entries load");
        let full_entries = store
            .transcript_entries_by_id(
                session_id,
                &["entry_assistant".to_string()],
                TranscriptEntryBodyMode::Full,
            )
            .await
            .expect("full entries load");

        assert!(ui_entries.entries[0].provider_replay.is_empty());
        assert_eq!(full_entries.entries[0].provider_replay.len(), 1);

        db.cleanup().await;
    }

    #[tokio::test]
    async fn active_branch_full_projection_preserves_provider_replay() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "active-branch-full-projection";
        create_session(store, session_id).await;

        let entries = vec![
            turn_started("entry_start", None, 1),
            assistant_message_with_replay("entry_assistant", Some("entry_start"), "hello"),
            turn_finished("entry_finish", Some("entry_assistant"), 1),
        ];
        store
            .persist_outputs(
                session_id,
                OutputBatch::new(&entries, Some("entry_finish"), &[], &[]),
            )
            .await
            .expect("transcript persists");

        let ui_entries = store
            .transcript_entries_for_scope(
                session_id,
                TranscriptEntryScope::ActiveBranch,
                TranscriptEntryBodyMode::Ui,
            )
            .await
            .expect("ui active branch loads");
        let full_entries = store
            .transcript_entries_for_scope(
                session_id,
                TranscriptEntryScope::ActiveBranch,
                TranscriptEntryBodyMode::Full,
            )
            .await
            .expect("full active branch loads");
        let synced_entries = store
            .sync_active_branch(
                session_id,
                Some("entry_start"),
                TranscriptEntryBodyMode::Full,
            )
            .await
            .expect("full active branch suffix syncs")
            .entries;

        let ui_assistant = ui_entries
            .iter()
            .find(|entry| entry.id == "entry_assistant")
            .expect("ui assistant entry");
        let full_assistant = full_entries
            .iter()
            .find(|entry| entry.id == "entry_assistant")
            .expect("full assistant entry");
        let synced_assistant = synced_entries
            .iter()
            .find(|entry| entry.id == "entry_assistant")
            .expect("synced assistant entry");

        assert!(ui_assistant.provider_replay.is_empty());
        assert_eq!(full_assistant.provider_replay.len(), 1);
        assert_eq!(synced_assistant.provider_replay.len(), 1);

        db.cleanup().await;
    }

    #[tokio::test]
    async fn transcript_turns_returns_full_user_and_final_assistant_entries() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "turn-card-full-messages";
        create_session(store, session_id).await;

        let long_user = "u".repeat(500);
        let long_first_assistant = "first assistant".repeat(40);
        let long_final_assistant = "final assistant answer ".repeat(40);
        let entries = vec![
            turn_started("entry_start", None, 1),
            user_message("entry_user", Some("entry_start"), &long_user),
            assistant_message(
                "entry_assistant_first",
                Some("entry_user"),
                &long_first_assistant,
            ),
            tool_result("entry_tool_result", Some("entry_assistant_first")),
            assistant_message(
                "entry_assistant_final",
                Some("entry_tool_result"),
                &long_final_assistant,
            ),
            turn_finished("entry_finish", Some("entry_assistant_final"), 1),
        ];
        store
            .persist_outputs(
                session_id,
                OutputBatch::new(&entries, Some("entry_finish"), &[], &[]),
            )
            .await
            .expect("transcript persists");

        let turns = store
            .transcript_turns(session_id, None, None)
            .await
            .expect("turn cards load");
        assert_eq!(turns.cards.len(), 1);
        let card = &turns.cards[0];
        assert_eq!(card.user_messages.len(), 1);
        assert_eq!(card.user_messages[0].id, "entry_user");
        match &card.user_messages[0].item {
            TranscriptItem::UserMessage(message) => {
                assert_eq!(content_blocks_text(&message.content), long_user);
            }
            other => panic!("expected user message, got {other:?}"),
        }
        let assistant = card
            .assistant_message
            .as_ref()
            .expect("final assistant included");
        assert_eq!(assistant.id, "entry_assistant_final");
        match &assistant.item {
            TranscriptItem::AssistantMessage(message) => {
                assert_eq!(
                    message.items,
                    vec![AssistantItem::Text(long_final_assistant)]
                );
            }
            other => panic!("expected assistant message, got {other:?}"),
        }

        db.cleanup().await;
    }

    #[tokio::test]
    async fn transcript_turns_preserves_compaction_resume_metadata() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "turn-card-compaction-metadata";
        create_session(store, session_id).await;

        let turn_started_at_ms = 1_700_000_123_456;
        let mut compacted = compaction_summary("entry_compact", None, session_id, "entry_source");
        compacted.timestamp_ms = turn_started_at_ms + 5_000;
        if let TranscriptItem::CompactionSummary(summary) = &mut compacted.item {
            summary.last_turn_id = TurnId(7);
            summary.turn_started_at_ms = Some(turn_started_at_ms);
        }
        let final_assistant = assistant_message("entry_assistant", Some("entry_compact"), "done");
        let entries = vec![
            compacted,
            final_assistant.clone(),
            turn_finished("entry_finish", Some("entry_assistant"), 7),
        ];
        store
            .persist_outputs(
                session_id,
                OutputBatch::new(&entries, Some("entry_finish"), &[], &[]),
            )
            .await
            .expect("transcript persists");

        let turns = store
            .transcript_turns(session_id, None, None)
            .await
            .expect("turn cards load");
        assert_eq!(turns.cards.len(), 2);
        let compact_card = &turns.cards[0];
        assert_eq!(compact_card.turn_id, Some(TurnId(7)));
        assert_eq!(compact_card.start_timestamp_ms, turn_started_at_ms);
        let resumed_card = &turns.cards[1];
        assert_eq!(resumed_card.turn_id, Some(TurnId(7)));
        assert_eq!(resumed_card.start_timestamp_ms, turn_started_at_ms);
        assert_eq!(
            resumed_card
                .assistant_message
                .as_ref()
                .map(|entry| entry.id.as_str()),
            Some(final_assistant.id.as_str())
        );

        db.cleanup().await;
    }

    #[tokio::test]
    async fn transcript_turns_pages_from_tail_and_detail_uses_card_bounds() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "turn-card-tail-pages";
        create_session(store, session_id).await;

        let entries = vec![
            turn_started("entry_1_start", None, 1),
            user_message("entry_1_user", Some("entry_1_start"), "one"),
            turn_finished("entry_1_finish", Some("entry_1_user"), 1),
            turn_started("entry_2_start", Some("entry_1_finish"), 2),
            user_message("entry_2_user", Some("entry_2_start"), "two"),
            turn_finished("entry_2_finish", Some("entry_2_user"), 2),
            turn_started("entry_3_start", Some("entry_2_finish"), 3),
            user_message("entry_3_user", Some("entry_3_start"), "three"),
            turn_finished("entry_3_finish", Some("entry_3_user"), 3),
        ];
        store
            .persist_outputs(
                session_id,
                OutputBatch::new(&entries, Some("entry_3_finish"), &[], &[]),
            )
            .await
            .expect("transcript persists");

        let latest = store
            .transcript_turns(session_id, None, Some(1))
            .await
            .expect("latest turn page loads");
        assert_eq!(latest.cards.len(), 1);
        assert_eq!(latest.cards[0].id, "entry_3_finish");
        assert_eq!(
            latest.next_before_entry_id.as_deref(),
            Some("entry_2_finish")
        );
        assert!(latest.has_more_before);

        let previous = store
            .transcript_turns(session_id, latest.next_before_entry_id.as_deref(), Some(1))
            .await
            .expect("previous turn page loads");
        assert_eq!(previous.cards.len(), 1);
        assert_eq!(previous.cards[0].id, "entry_2_finish");
        assert_eq!(
            previous.next_before_entry_id.as_deref(),
            Some("entry_1_finish")
        );

        let card = &latest.cards[0];
        let detail = store
            .transcript_turn_detail(
                session_id,
                &card.id,
                &card.active_leaf_id,
                card.start_sequence,
                card.end_sequence,
                TranscriptEntryBodyMode::Ui,
            )
            .await
            .expect("card detail loads from targeted bounds");
        assert_eq!(
            detail
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["entry_3_start", "entry_3_user", "entry_3_finish"]
        );

        db.cleanup().await;
    }

    #[tokio::test]
    async fn switch_active_leaf_returns_revisions_and_active_branch() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "switch-active-branch";
        create_session(store, session_id).await;

        let entries = vec![
            turn_started("entry_source_start", None, 1),
            user_message("entry_source_user", Some("entry_source_start"), "source"),
            turn_finished("entry_source_finish", Some("entry_source_user"), 1),
            compaction_summary("entry_root", None, session_id, "entry_source_finish"),
            turn_started("entry_a_start", Some("entry_root"), 1),
            user_message("entry_a_user", Some("entry_a_start"), "branch a"),
            turn_finished("entry_a_finish", Some("entry_a_user"), 1),
            turn_started("entry_b_start", Some("entry_root"), 2),
            user_message("entry_b_user", Some("entry_b_start"), "branch b"),
            turn_finished("entry_b_finish", Some("entry_b_user"), 2),
        ];
        store
            .persist_outputs(
                session_id,
                OutputBatch::new(&entries, Some("entry_b_finish"), &[], &[]),
            )
            .await
            .expect("transcript persists");
        let before = store
            .session_snapshot(session_id)
            .await
            .expect("snapshot loads");

        let result = store
            .switch_active_leaf(session_id, Some("entry_a_finish"), true, None, None, None)
            .await
            .expect("switch succeeds");

        assert_eq!(result.session_id, session_id);
        assert_eq!(result.active_leaf_id.as_deref(), Some("entry_a_finish"));
        assert_eq!(result.activity, SessionActivity::Idle);
        assert!(result.session_revision > before.session_revision);
        assert_eq!(result.queue_revision, before.queue_revision);
        assert_eq!(result.transcript_revision, before.transcript_revision);
        assert!(result.last_event_id > before.last_event_id);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event, EventType::HistorySwitched);
        assert_eq!(
            result
                .active_branch_entries
                .expect("active branch returned")
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "entry_source_start",
                "entry_source_user",
                "entry_source_finish",
                "entry_root",
                "entry_a_start",
                "entry_a_user",
                "entry_a_finish",
            ]
        );

        db.cleanup().await;
    }

    #[tokio::test]
    async fn transcript_appended_event_uses_bumped_revision_and_sequence() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "transcript-event-revision";
        create_session(store, session_id).await;

        let first = user_message("entry_user", None, "hello");
        let events = vec![appended_event(&first)];
        let (frames, _) = store
            .persist_outputs(
                session_id,
                OutputBatch::new(
                    std::slice::from_ref(&first),
                    Some("entry_user"),
                    &events,
                    &[],
                ),
            )
            .await
            .expect("transcript persists");
        let snapshot = store
            .session_snapshot(session_id)
            .await
            .expect("snapshot loads");
        let frame = frames
            .iter()
            .find(|frame| frame.event == EventType::TranscriptAppended)
            .expect("transcript appended event");

        assert_eq!(snapshot.transcript_revision, 1);
        assert_eq!(
            frame.data["transcript_revision"].as_i64(),
            Some(snapshot.transcript_revision)
        );
        assert_eq!(
            frame.data["session_revision"].as_i64(),
            Some(snapshot.session_revision)
        );
        assert_eq!(frame.data["entry"]["sequence"].as_i64(), Some(1));
        assert_eq!(frame.data["tree_node"]["sequence"].as_i64(), Some(1));
        assert_eq!(frame.data["active_leaf_id"].as_str(), Some("entry_user"));

        db.cleanup().await;
    }
}
