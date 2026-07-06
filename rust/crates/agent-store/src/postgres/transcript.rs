use std::collections::{BTreeMap, BTreeSet, HashMap};
#[cfg(test)]
use std::{cell::Cell, future::Future};

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
    TranscriptTurnDetailResult, TranscriptTurnsResult,
};

#[cfg(test)]
use super::events::with_event_insert_statement_count;
use super::events::{insert_event_tx, insert_transcript_item_events_tx};
use super::queue::bump_revisions_tx;
use super::rows::{row_to_stored_entry, row_to_transcript_entry};
use super::sql::{ensure_no_active_work_tx, lock_session_tx, stale_unfinished_actions_for_session};
use super::turn_cards::{active_branch_turn_card_page_tx, TurnCardPage};
use super::PostgresAgentStore;

const DEFAULT_TRANSCRIPT_INDEX_LIMIT: i64 = 1000;
const MAX_TRANSCRIPT_INDEX_LIMIT: i64 = 5000;
const DEFAULT_TRANSCRIPT_TURN_LIMIT: i64 = 50;
const MAX_TRANSCRIPT_TURN_LIMIT: i64 = 200;
// Seven arrays plus the session ID stay far below PostgreSQL's 65,535 bind
// limit. Cap each statement to bound accumulated transcript JSON payloads.
const TRANSCRIPT_INSERT_BATCH_CAPACITY: usize = 128;

#[cfg(test)]
tokio::task_local! {
    static TRANSCRIPT_INSERT_STATEMENTS: Cell<usize>;
    static TRANSCRIPT_ENTRY_RECORD_READS: Cell<usize>;
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TranscriptQueryCounts {
    pub(super) insert_statements: usize,
    pub(super) entry_record_reads: usize,
}

#[cfg(test)]
pub(super) async fn with_transcript_query_counts<F>(future: F) -> (F::Output, TranscriptQueryCounts)
where
    F: Future,
{
    TRANSCRIPT_INSERT_STATEMENTS
        .scope(Cell::new(0), async {
            TRANSCRIPT_ENTRY_RECORD_READS
                .scope(Cell::new(0), async {
                    let output = future.await;
                    let counts = TranscriptQueryCounts {
                        insert_statements: TRANSCRIPT_INSERT_STATEMENTS.with(Cell::get),
                        entry_record_reads: TRANSCRIPT_ENTRY_RECORD_READS.with(Cell::get),
                    };
                    (output, counts)
                })
                .await
        })
        .await
}

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
             and parent.id = coalesce(
                case
                    when child.item->>'type' = 'compaction_summary' then child.item->>'source_leaf_id'
                    else null
                end,
                child.parent_id
             )
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
        let TurnCardPage {
            next_before_entry_id,
            cards,
        } = if active_leaf_id.is_some() || before_entry_id.is_some() {
            active_branch_turn_card_page_tx(&mut tx, session_id, before_entry_id, limit).await?
        } else {
            TurnCardPage::default()
        };
        let has_more_before = next_before_entry_id.is_some();
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
        let entries = branch_entries_to_leaf(&self.pool, session_id, leaf_id).await?;
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

    pub async fn set_active_leaf(
        &self,
        session_id: &str,
        leaf_id: Option<&str>,
    ) -> Result<Vec<EventFrame>> {
        let result = self
            .switch_active_leaf(session_id, leaf_id, false, None, None, None, None)
            .await?;
        Ok(result.events)
    }

    pub async fn switch_active_leaf(
        &self,
        session_id: &str,
        leaf_id: Option<&str>,
        return_active_branch: bool,
        expected_active_leaf_id: Option<Option<&str>>,
        expected_transcript_revision: Option<i64>,
        expected_active_branch_entry_ids: Option<&[String]>,
        missing_body_ids: Option<&[String]>,
    ) -> Result<SwitchActiveLeafResult> {
        let expected_active_branch_entry_ids =
            expected_active_branch_entry_ids.filter(|ids| !ids.is_empty());
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let (current_active_leaf_id, current_transcript_revision): (Option<String>, i64) =
            sqlx::query_as("select active_leaf_id, transcript_revision from sessions where id=$1")
                .bind(session_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        ensure_no_active_work_tx(&mut tx, session_id).await?;
        if let Some(expected) = expected_active_leaf_id {
            if current_active_leaf_id.as_deref() != expected {
                return Err(crate::ExpectedActiveLeafMismatch::new(
                    current_active_leaf_id,
                    expected.map(str::to_string),
                )
                .into());
            }
        }
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
        let query = stale_unfinished_actions_for_session();
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

pub(super) async fn branch_entries_to_leaf<'e, E>(
    executor: E,
    session_id: &str,
    leaf_id: &str,
) -> Result<Vec<StoredTranscriptEntry>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
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
    .fetch_all(executor)
    .await?;
    rows.into_iter()
        .map(|row| row_to_stored_entry(&row))
        .collect()
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
    #[cfg(test)]
    let _ = TRANSCRIPT_ENTRY_RECORD_READS.try_with(|count| count.set(count.get() + 1));
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
        TranscriptItem::DaemonToolObservation(_) => "daemon_tool_observation",
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
        TranscriptItem::DaemonToolObservation(observation) => observation
            .summary
            .clone()
            .unwrap_or_else(|| format!("daemon observation: {}", observation.tool_name)),
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

pub(super) async fn insert_entry_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry: &TranscriptStorageNode,
) -> Result<Option<TranscriptEntryRecord>> {
    Ok(
        insert_entries_tx(tx, session_id, std::slice::from_ref(entry))
            .await?
            .pop(),
    )
}

pub(super) async fn insert_entries_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entries: &[TranscriptStorageNode],
) -> Result<Vec<TranscriptEntryRecord>> {
    let mut records = Vec::with_capacity(entries.len());
    for batch in entries.chunks(TRANSCRIPT_INSERT_BATCH_CAPACITY) {
        let ids = batch
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        let parent_ids = batch
            .iter()
            .map(|entry| entry.parent_id.clone())
            .collect::<Vec<_>>();
        let timestamp_ms = batch
            .iter()
            .map(|entry| entry.timestamp_ms as i64)
            .collect::<Vec<_>>();
        let items = batch
            .iter()
            .map(|entry| serde_json::to_value(&entry.item))
            .collect::<serde_json::Result<Vec<_>>>()?;
        let provider_replay = batch
            .iter()
            .map(|entry| serde_json::to_value(&entry.provider_replay))
            .collect::<serde_json::Result<Vec<_>>>()?;
        let turn_ids = batch
            .iter()
            .map(|entry| entry.item.turn_id().map(|turn_id| turn_id.0 as i64))
            .collect::<Vec<_>>();
        let user_messages = batch
            .iter()
            .map(|entry| matches!(entry.item, TranscriptItem::UserMessage(_)))
            .collect::<Vec<_>>();
        #[cfg(test)]
        let _ = TRANSCRIPT_INSERT_STATEMENTS.try_with(|count| count.set(count.get() + 1));
        let rows = sqlx::query(
            r#"
            with input as materialized (
                select id, parent_id, timestamp_ms, item, provider_replay, turn_id,
                    user_message, input_ordinal
                from unnest(
                    $2::text[],
                    $3::text[],
                    $4::bigint[],
                    $5::jsonb[],
                    $6::jsonb[],
                    $7::bigint[],
                    $8::boolean[]
                ) with ordinality as input(
                    id,
                    parent_id,
                    timestamp_ms,
                    item,
                    provider_replay,
                    turn_id,
                    user_message,
                    input_ordinal
                )
            ),
            first_input as materialized (
                select distinct on (id)
                    id,
                    timestamp_ms,
                    user_message,
                    input_ordinal
                from input
                order by id, input_ordinal
            ),
            inserted as (
                insert into transcript_entries (
                    session_id,
                    id,
                    parent_id,
                    timestamp_ms,
                    item,
                    provider_replay,
                    turn_id
                )
                select
                    $1::text,
                    id,
                    parent_id,
                    timestamp_ms,
                    item,
                    provider_replay,
                    turn_id
                from input
                order by input_ordinal
                on conflict (session_id, id) do nothing
                returning id, parent_id, timestamp_ms, sequence, item
            ),
            updated_session as (
                update sessions
                set last_user_message_timestamp_ms = greatest(
                    coalesce(sessions.last_user_message_timestamp_ms, 0),
                    inserted_users.timestamp_ms
                )
                from (
                    select max(first_input.timestamp_ms) as timestamp_ms
                    from inserted
                    join first_input using (id)
                    where first_input.user_message
                ) inserted_users
                where sessions.id=$1
                    and inserted_users.timestamp_ms is not null
                returning sessions.id
            )
            select
                inserted.id,
                inserted.parent_id,
                inserted.timestamp_ms,
                inserted.sequence,
                inserted.item,
                '[]'::jsonb as provider_replay,
                (select count(*) from updated_session) as updated_sessions
            from inserted
            join first_input using (id)
            order by first_input.input_ordinal
            "#,
        )
        .bind(session_id)
        .bind(ids)
        .bind(parent_ids)
        .bind(timestamp_ms)
        .bind(items)
        .bind(provider_replay)
        .bind(turn_ids)
        .bind(user_messages)
        .fetch_all(&mut **tx)
        .await?;
        records.extend(
            rows.into_iter()
                .map(|row| row_to_transcript_entry(&row))
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(records)
}

pub(super) async fn insert_stored_entry_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry: &StoredTranscriptEntry,
) -> Result<Option<TranscriptEntryRecord>> {
    let storage_node = TranscriptStorageNode {
        id: entry.id.clone(),
        parent_id: entry.parent_id.clone(),
        timestamp_ms: entry.timestamp_ms,
        item: entry.item.clone(),
        provider_replay: entry.provider_replay.clone(),
    };
    insert_entry_tx(tx, session_id, &storage_node).await
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
pub(super) mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_session::{SessionEvent, TranscriptStorageNode};
    use agent_vocab::{
        AssistantItem, AssistantMessage, CompactionSummary, DaemonToolObservation, ProviderConfig,
        ProviderKind, ProviderReplayItem, ReasoningEffort, ToolCallId, ToolResultMessage,
        TranscriptItem, TurnId, TurnOutcome, UserMessage,
    };
    use serde_json::json;
    use uuid::Uuid;

    use crate::{OutputBatch, SessionConfig};

    use super::*;

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(30_000);

    pub(crate) struct TestDb {
        pub(crate) store: PostgresAgentStore,
        admin_url: String,
        name: String,
    }

    impl TestDb {
        pub(crate) async fn cleanup(self) {
            self.store.close().await;
            if let Ok(admin) = sqlx::PgPool::connect(&self.admin_url).await {
                let _ = sqlx::query(&format!(r#"drop database if exists "{}""#, self.name))
                    .execute(&admin)
                    .await;
                admin.close().await;
            }
        }
    }

    pub(crate) async fn test_store() -> Option<TestDb> {
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

    pub(crate) async fn create_session(store: &PostgresAgentStore, session_id: &str) {
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
        entry_at(id, parent_id, 1, item)
    }

    fn entry_at(
        id: &str,
        parent_id: Option<&str>,
        timestamp_ms: u64,
        item: TranscriptItem,
    ) -> TranscriptStorageNode {
        TranscriptStorageNode {
            id: id.to_string(),
            parent_id: parent_id.map(str::to_string),
            timestamp_ms,
            item,
            provider_replay: Vec::new(),
        }
    }

    fn with_timestamp(
        mut entry: TranscriptStorageNode,
        timestamp_ms: u64,
    ) -> TranscriptStorageNode {
        entry.timestamp_ms = timestamp_ms;
        entry
    }

    fn expected_record(entry: &TranscriptStorageNode, sequence: i64) -> TranscriptEntryRecord {
        TranscriptEntryRecord {
            id: entry.id.clone(),
            parent_id: entry.parent_id.clone(),
            timestamp_ms: entry.timestamp_ms,
            sequence,
            item: entry.item.clone(),
            provider_replay: entry.provider_replay.clone(),
        }
    }

    fn expected_inserted_record(
        entry: &TranscriptStorageNode,
        sequence: i64,
    ) -> TranscriptEntryRecord {
        TranscriptEntryRecord {
            provider_replay: Vec::new(),
            ..expected_record(entry, sequence)
        }
    }

    async fn insert_entries(
        store: &PostgresAgentStore,
        session_id: &str,
        entries: &[TranscriptStorageNode],
    ) -> Result<Vec<TranscriptEntryRecord>> {
        let mut tx = store.pool.begin().await?;
        let records = insert_entries_tx(&mut tx, session_id, entries).await?;
        tx.commit().await?;
        Ok(records)
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

    fn daemon_observation(id: &str, parent_id: Option<&str>) -> TranscriptStorageNode {
        entry(
            id,
            parent_id,
            TranscriptItem::DaemonToolObservation(DaemonToolObservation::inspect_delegation(
                ToolCallId::new("call_delegation_1_attempt_1"),
                "delegation_1",
                Some("Delegation delegation_1 completed".to_string()),
                json!({
                    "delegation_id": "delegation_1",
                    "status": "done",
                    "outcome": "approved"
                }),
            )),
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
    async fn bounded_insert_returns_complete_records_for_single_and_rich_batches() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "bounded-insert-records";
        create_session(store, session_id).await;

        let first = with_timestamp(user_message("entry_user", None, "héllo 世界"), 10);
        assert_eq!(
            insert_entries(store, session_id, std::slice::from_ref(&first))
                .await
                .expect("single entry inserts"),
            vec![expected_inserted_record(&first, 1)]
        );

        let rich_entries = vec![
            with_timestamp(turn_started("entry_start", Some("entry_user"), 42), 20),
            with_timestamp(
                assistant_message_with_replay("entry_assistant", Some("entry_start"), "replayed"),
                30,
            ),
            with_timestamp(tool_result("entry_tool", Some("entry_assistant")), 40),
            with_timestamp(
                compaction_summary("entry_compaction", None, session_id, "entry_tool"),
                50,
            ),
            with_timestamp(
                assistant_message("entry_branch", Some("entry_start"), "branch"),
                60,
            ),
        ];
        let expected = rich_entries
            .iter()
            .enumerate()
            .map(|(index, entry)| expected_inserted_record(entry, index as i64 + 2))
            .collect::<Vec<_>>();
        assert_eq!(
            insert_entries(store, session_id, &rich_entries)
                .await
                .expect("rich batch inserts"),
            expected
        );
        assert_eq!(
            store
                .transcript_entry_records(session_id, TranscriptEntryBodyMode::Full)
                .await
                .expect("complete entries load"),
            [
                vec![expected_record(&first, 1)],
                rich_entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| expected_record(entry, index as i64 + 2))
                    .collect(),
            ]
            .concat()
        );
        let stored_turn_ids = sqlx::query_as::<_, (String, Option<i64>)>(
            "select id, turn_id from transcript_entries where session_id=$1 order by sequence",
        )
        .bind(session_id)
        .fetch_all(&store.pool)
        .await
        .expect("turn ids load");
        assert_eq!(
            stored_turn_ids,
            vec![
                ("entry_user".to_string(), None),
                ("entry_start".to_string(), Some(42)),
                ("entry_assistant".to_string(), None),
                ("entry_tool".to_string(), None),
                ("entry_compaction".to_string(), Some(0)),
                ("entry_branch".to_string(), None),
            ]
        );

        db.cleanup().await;
    }

    #[tokio::test]
    async fn output_persistence_keeps_conflict_fallback_entry_lookup() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "bounded-output-conflict-fallback";
        create_session(store, session_id).await;

        let original = with_timestamp(user_message("entry_user", None, "original"), 10);
        insert_entries(store, session_id, std::slice::from_ref(&original))
            .await
            .expect("original inserts");
        let conflicting = with_timestamp(user_message("entry_user", None, "replacement"), 99);
        let events = vec![appended_event(&conflicting)];
        let ((result, event_statements), counts) =
            with_transcript_query_counts(with_event_insert_statement_count(store.persist_outputs(
                session_id,
                OutputBatch::new(
                    std::slice::from_ref(&conflicting),
                    Some("entry_user"),
                    &events,
                    &[],
                ),
            )))
            .await;
        let (frames, _) = result.expect("conflicting output remains idempotent");

        assert_eq!(event_statements, 1);
        assert_eq!(
            counts,
            TranscriptQueryCounts {
                insert_statements: 1,
                entry_record_reads: 1,
            }
        );
        assert_eq!(
            frames[0].data["entry"],
            json!({
                "id": "entry_user",
                "parent_id": null,
                "timestamp_ms": 10,
                "sequence": 1,
                "item": {
                    "type": "user_message",
                    "content": [{
                        "type": "text",
                        "text": "original",
                    }],
                },
            })
        );
        assert_eq!(
            store
                .transcript_entry_records(session_id, TranscriptEntryBodyMode::Full)
                .await
                .expect("entry loads"),
            vec![expected_record(&original, 1)]
        );

        db.cleanup().await;
    }

    #[tokio::test]
    async fn output_persistence_batches_transcript_rows_without_requerying_returned_entries() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "bounded-output-events";
        create_session(store, session_id).await;

        let entries = vec![
            with_timestamp(turn_started("entry_start", None, 7), 10),
            with_timestamp(user_message("entry_user", Some("entry_start"), "hello"), 20),
            with_timestamp(
                assistant_message("entry_assistant", Some("entry_user"), "done"),
                30,
            ),
            with_timestamp(
                turn_finished("entry_finish", Some("entry_assistant"), 7),
                40,
            ),
        ];
        let events = vec![appended_event(&entries[1])];
        let (result, counts) = with_transcript_query_counts(store.persist_outputs(
            session_id,
            OutputBatch::new(&entries, Some("entry_finish"), &events, &[]),
        ))
        .await;
        let (frames, actions) = result.expect("outputs persist");

        assert!(actions.is_empty());
        assert_eq!(
            counts,
            TranscriptQueryCounts {
                insert_statements: 1,
                entry_record_reads: 0,
            }
        );
        let user_record = expected_inserted_record(&entries[1], 2);
        assert_eq!(
            frames,
            vec![EventFrame {
                event_id: 2,
                event: EventType::TranscriptAppended,
                session_id: session_id.to_string(),
                data: json!({
                    "entry_id": "entry_user",
                    "item": entries[1].item,
                    "entry": user_record,
                    "tree_node": tree_node_from_entry(&user_record),
                    "active_leaf_id": "entry_finish",
                    "session_revision": 1,
                    "queue_revision": 0,
                    "transcript_revision": 1,
                }),
            }]
        );

        db.cleanup().await;
    }

    #[tokio::test]
    async fn bounded_insert_mixed_conflicts_returns_only_new_records_in_input_order() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "bounded-insert-conflicts";
        create_session(store, session_id).await;

        let original = with_timestamp(user_message("entry_existing", None, "original"), 10);
        assert_eq!(
            insert_entries(store, session_id, std::slice::from_ref(&original))
                .await
                .expect("original inserts"),
            vec![expected_inserted_record(&original, 1)]
        );

        let duplicate = with_timestamp(
            user_message("entry_existing", Some("wrong-parent"), "replacement"),
            999,
        );
        let new_a = assistant_message("entry_new_a", Some("entry_existing"), "a");
        let new_b = tool_result("entry_new_b", Some("entry_new_a"));
        let batch = vec![duplicate.clone(), new_a.clone(), duplicate, new_b.clone()];
        assert_eq!(
            insert_entries(store, session_id, &batch)
                .await
                .expect("mixed batch inserts"),
            vec![
                expected_inserted_record(&new_a, 3),
                expected_inserted_record(&new_b, 5),
            ]
        );
        assert_eq!(
            store
                .transcript_entry_records(session_id, TranscriptEntryBodyMode::Full)
                .await
                .expect("entries load"),
            vec![
                expected_record(&original, 1),
                expected_record(&new_a, 3),
                expected_record(&new_b, 5),
            ]
        );

        db.cleanup().await;
    }

    #[tokio::test]
    async fn bounded_insert_updates_last_user_timestamp_only_for_new_user_entries() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "bounded-insert-user-timestamp";
        create_session(store, session_id).await;

        insert_entries(
            store,
            session_id,
            &[with_timestamp(
                assistant_message("entry_non_user_first", None, "not a user"),
                900,
            )],
        )
        .await
        .expect("non-user inserts");
        assert_eq!(
            sqlx::query_scalar::<_, Option<i64>>(
                "select last_user_message_timestamp_ms from sessions where id=$1",
            )
            .bind(session_id)
            .fetch_one(&store.pool)
            .await
            .expect("initial timestamp loads"),
            None
        );

        let duplicate_user = with_timestamp(
            user_message(
                "entry_duplicate_user",
                Some("entry_non_user_first"),
                "original",
            ),
            50,
        );
        insert_entries(store, session_id, std::slice::from_ref(&duplicate_user))
            .await
            .expect("duplicate seed inserts");
        sqlx::query("update sessions set last_user_message_timestamp_ms=250 where id=$1")
            .bind(session_id)
            .execute(&store.pool)
            .await
            .expect("prior timestamp sets");

        let batch = vec![
            with_timestamp(
                user_message("entry_user_300", Some("entry_duplicate_user"), "300"),
                300,
            ),
            with_timestamp(
                assistant_message("entry_non_user", Some("entry_user_300"), "not a user"),
                900,
            ),
            with_timestamp(
                user_message("entry_user_100", Some("entry_non_user"), "100"),
                100,
            ),
            with_timestamp(
                user_message("entry_duplicate_user", None, "conflict"),
                1_000,
            ),
            with_timestamp(
                user_message("entry_user_400", Some("entry_user_100"), "400"),
                400,
            ),
        ];
        insert_entries(store, session_id, &batch)
            .await
            .expect("timestamp batch inserts");
        assert_eq!(
            sqlx::query_scalar::<_, Option<i64>>(
                "select last_user_message_timestamp_ms from sessions where id=$1",
            )
            .bind(session_id)
            .fetch_one(&store.pool)
            .await
            .expect("timestamp loads"),
            Some(400)
        );

        sqlx::query("update sessions set last_user_message_timestamp_ms=700 where id=$1")
            .bind(session_id)
            .execute(&store.pool)
            .await
            .expect("greater prior timestamp sets");
        insert_entries(
            store,
            session_id,
            &[with_timestamp(
                user_message("entry_user_600", Some("entry_user_400"), "600"),
                600,
            )],
        )
        .await
        .expect("lower user timestamp inserts");
        assert_eq!(
            sqlx::query_scalar::<_, Option<i64>>(
                "select last_user_message_timestamp_ms from sessions where id=$1",
            )
            .bind(session_id)
            .fetch_one(&store.pool)
            .await
            .expect("timestamp reloads"),
            Some(700)
        );

        db.cleanup().await;
    }

    #[tokio::test]
    async fn bounded_insert_statement_count_follows_batch_capacity() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let cases = [
            (0, 0),
            (1, 1),
            (TRANSCRIPT_INSERT_BATCH_CAPACITY, 1),
            (TRANSCRIPT_INSERT_BATCH_CAPACITY + 1, 2),
            (TRANSCRIPT_INSERT_BATCH_CAPACITY * 2 + 1, 3),
        ];

        for (case_index, (entry_count, expected_statements)) in cases.into_iter().enumerate() {
            let session_id = format!("bounded-insert-statements-{case_index}");
            create_session(store, &session_id).await;
            let entries = (0..entry_count)
                .map(|index| TranscriptStorageNode {
                    id: format!("entry_{case_index}_{index}"),
                    parent_id: (index > 0).then(|| format!("entry_{case_index}_{}", index - 1)),
                    timestamp_ms: index as u64,
                    item: TranscriptItem::AssistantMessage(AssistantMessage {
                        items: vec![AssistantItem::Text(index.to_string())],
                    }),
                    provider_replay: Vec::new(),
                })
                .collect::<Vec<_>>();
            let (result, counts) =
                with_transcript_query_counts(insert_entries(store, &session_id, &entries)).await;
            assert_eq!(result.expect("boundary batch inserts").len(), entry_count);
            assert_eq!(
                counts,
                TranscriptQueryCounts {
                    insert_statements: expected_statements,
                    entry_record_reads: 0,
                }
            );
        }

        db.cleanup().await;
    }

    #[tokio::test]
    async fn bounded_insert_is_concurrently_idempotent() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "bounded-insert-concurrent";
        create_session(store, session_id).await;
        let entries = vec![
            assistant_message("entry_a", None, "a"),
            assistant_message("entry_b", Some("entry_a"), "b"),
            assistant_message("entry_c", Some("entry_b"), "c"),
        ];

        let pool_a = store.pool.clone();
        let pool_b = store.pool.clone();
        let entries_a = entries.clone();
        let entries_b = entries.clone();
        let insert_a = async move {
            let mut tx = pool_a.begin().await?;
            let records = insert_entries_tx(&mut tx, session_id, &entries_a).await?;
            tx.commit().await?;
            Result::<Vec<TranscriptEntryRecord>>::Ok(records)
        };
        let insert_b = async move {
            let mut tx = pool_b.begin().await?;
            let records = insert_entries_tx(&mut tx, session_id, &entries_b).await?;
            tx.commit().await?;
            Result::<Vec<TranscriptEntryRecord>>::Ok(records)
        };
        let (records_a, records_b) = tokio::join!(insert_a, insert_b);
        let records_a = records_a.expect("first concurrent insert succeeds");
        let records_b = records_b.expect("second concurrent insert succeeds");
        assert_eq!(records_a.len() + records_b.len(), entries.len());
        assert!(records_a.is_empty() || records_b.is_empty());
        assert_eq!(
            store
                .transcript_entry_records(session_id, TranscriptEntryBodyMode::Full)
                .await
                .expect("concurrent entries load")
                .into_iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            vec![
                "entry_a".to_string(),
                "entry_b".to_string(),
                "entry_c".to_string(),
            ]
        );

        db.cleanup().await;
    }

    #[tokio::test]
    async fn bounded_insert_error_rolls_back_prior_batches() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "bounded-insert-rollback";
        create_session(store, session_id).await;
        let mut entries = (0..TRANSCRIPT_INSERT_BATCH_CAPACITY)
            .map(|index| assistant_message(&format!("entry_{index}"), None, "ok"))
            .collect::<Vec<_>>();
        entries.push(assistant_message("entry_bad\0", None, "bad"));

        let mut tx = store.pool.begin().await.expect("transaction starts");
        let error = insert_entries_tx(&mut tx, session_id, &entries)
            .await
            .expect_err("NUL id must fail");
        assert!(!error.to_string().is_empty());
        tx.rollback().await.expect("failed insert rolls back");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "select count(*) from transcript_entries where session_id=$1",
            )
            .bind(session_id)
            .fetch_one(&store.pool)
            .await
            .expect("entry count loads"),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, Option<i64>>(
                "select last_user_message_timestamp_ms from sessions where id=$1",
            )
            .bind(session_id)
            .fetch_one(&store.pool)
            .await
            .expect("timestamp loads"),
            None
        );

        db.cleanup().await;
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
                None,
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
        assert_eq!(turns.cards.len(), 1);
        let resumed_card = &turns.cards[0];
        assert_eq!(resumed_card.turn_id, Some(TurnId(7)));
        assert_eq!(resumed_card.start_timestamp_ms, turn_started_at_ms);
        assert_eq!(resumed_card.start_sequence, 1);
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
    async fn transcript_turns_includes_daemon_observations_in_turn_cards() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "turn-card-daemon-observation";
        create_session(store, session_id).await;

        let entries = vec![
            turn_started("entry_start", None, 1),
            daemon_observation("entry_daemon", Some("entry_start")),
            assistant_message("entry_assistant", Some("entry_daemon"), "ack"),
            turn_finished("entry_finish", Some("entry_assistant"), 1),
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
        assert_eq!(card.daemon_observations.len(), 1);
        assert_eq!(card.daemon_observations[0].id, "entry_daemon");
        match &card.daemon_observations[0].item {
            TranscriptItem::DaemonToolObservation(observation) => {
                assert_eq!(observation.tool_name, "inspect_delegation");
                assert_eq!(
                    observation.summary.as_deref(),
                    Some("Delegation delegation_1 completed")
                );
            }
            other => panic!("expected daemon observation, got {other:?}"),
        }

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
            .expect("card detail loads");
        assert!(detail
            .entries
            .iter()
            .any(|entry| matches!(entry.item, TranscriptItem::DaemonToolObservation(_))));

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
            .switch_active_leaf(
                session_id,
                Some("entry_a_finish"),
                true,
                None,
                None,
                None,
                None,
            )
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
    async fn switch_active_leaf_rejects_queued_input_under_session_lock() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "switch-active-queued-guard";
        create_session(store, session_id).await;

        store
            .persist_outputs(
                session_id,
                OutputBatch::new(
                    &[
                        turn_started("entry_a_start", None, 1),
                        user_message("entry_a_user", Some("entry_a_start"), "branch a"),
                        turn_finished("entry_a_finish", Some("entry_a_user"), 1),
                        turn_started("entry_b_start", None, 2),
                        user_message("entry_b_user", Some("entry_b_start"), "branch b"),
                        turn_finished("entry_b_finish", Some("entry_b_user"), 2),
                    ],
                    Some("entry_a_finish"),
                    &[],
                    &[],
                ),
            )
            .await
            .expect("transcript persists");
        store
            .enqueue_user_input(
                session_id,
                crate::InputPriority::FollowUp,
                &UserMessage::text("accepted before switch"),
                Some("race-client-input"),
                Some(Some("entry_a_finish")),
            )
            .await
            .expect("queued input enqueues");

        let switched = store
            .switch_active_leaf(
                session_id,
                Some("entry_b_finish"),
                false,
                Some(Some("entry_a_finish")),
                None,
                None,
                None,
            )
            .await;
        assert!(switched
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<crate::SourceMutationConflict>())
            .is_some());
        assert_eq!(
            store
                .active_leaf_id(session_id)
                .await
                .expect("active leaf loads")
                .as_deref(),
            Some("entry_a_finish")
        );
        assert_eq!(
            store
                .queue_state(session_id)
                .await
                .expect("queue state")
                .queued_inputs
                .len(),
            1
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
