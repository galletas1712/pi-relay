use anyhow::{anyhow, Result};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};

use crate::{TranscriptEntryRecord, TurnCardRecord, TurnCardStatus};

#[derive(Debug, Default)]
pub(super) struct TurnCardPage {
    pub(super) next_before_entry_id: Option<String>,
    pub(super) cards: Vec<TurnCardRecord>,
}

pub(super) async fn active_branch_turn_card_page_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    before_entry_id: Option<&str>,
    limit: i64,
) -> Result<TurnCardPage> {
    let rows = active_branch_turn_card_rows_tx(tx, session_id, before_entry_id, limit).await?;
    let next_before_entry_id = rows
        .first()
        .and_then(TurnCardRow::display_parent_id)
        .map(ToOwned::to_owned);
    let cards = turn_cards_from_rows(&rows);
    Ok(TurnCardPage {
        next_before_entry_id,
        cards,
    })
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
                     when t.item->>'type' = 'turn_started' then 1
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
                         when parent.item->>'type' = 'turn_started' then 1
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
                       when item->>'type' = 'turn_started' then 1
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
               card_rows.item #>> '{turn_started_at_ms}' as compaction_turn_started_at_ms
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
    start_sequence: Option<i64>,
}

fn turn_cards_from_rows(rows: &[TurnCardRow]) -> Vec<TurnCardRecord> {
    let mut cards = Vec::new();
    let mut current: Option<MutableTurnCard> = None;
    let mut compaction_resume_anchor: Option<CompactionResumeAnchor> = None;

    for row in rows {
        if row.item_type == "compaction_summary" {
            compaction_resume_anchor = Some(CompactionResumeAnchor {
                turn_id: row.turn_id,
                start_timestamp_ms: row.compaction_turn_started_at_ms,
                start_sequence: row.compaction_turn_started_at_ms.map(|_| row.sequence),
            });
            if row.compaction_turn_started_at_ms.is_none() {
                close_open_turn(&mut cards, &mut current);
            } else if let Some(turn) = current.as_mut() {
                append_entry_to_turn_card(turn, row);
            }
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
        let start_sequence = anchor
            .and_then(|anchor| anchor.start_sequence)
            .unwrap_or(row.sequence);
        let turn = current.get_or_insert_with(|| MutableTurnCard {
            turn_id: row.turn_id.or(anchor.and_then(|anchor| anchor.turn_id)),
            start_entry_id: None,
            boundary_entry_id: None,
            active_leaf_id: row.id.clone(),
            start_sequence,
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
        "compaction_summary" => {
            if let Some(turn_id) = row.turn_id {
                card.turn_id = Some(turn_id);
            }
            if let Some(start_timestamp_ms) = row.compaction_turn_started_at_ms {
                card.start_timestamp_ms = start_timestamp_ms;
            }
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
