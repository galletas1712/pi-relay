use crate::SessionActivity;

use super::action_records::POST_COMPACTION_DISPATCH_KEY;

const UNFINISHED_ACTION_STATUSES: &str = "'pending','blocked','running'";
const ACTIVE_QUEUED_INPUT_STATUSES: &str = "'queued','consuming'";

pub(super) fn session_activity(running: bool, queued: bool) -> SessionActivity {
    if running {
        SessionActivity::Running
    } else if queued {
        SessionActivity::Queued
    } else {
        SessionActivity::Idle
    }
}

pub(super) const QUEUED_INPUT_DISPATCH_ORDER: &str = r#"
    case priority when 'steer' then 0 else 1 end,
    case
        when priority='steer'
        then coalesce((origin->>'promoted_at')::timestamptz, created_at)
        else null
    end,
    case
        when priority='follow_up'
        then follow_up_position
        else null
    end nulls last,
    created_at,
    id
"#;

pub(super) fn action_is_unfinished(alias: Option<&str>) -> String {
    status_is_one_of(alias, UNFINISHED_ACTION_STATUSES)
}

fn is_recoverable_post_compaction_dispatch(alias: Option<&str>) -> String {
    let prefix = alias.map(|alias| format!("{alias}.")).unwrap_or_default();
    format!(
        "{prefix}status in ('pending','running') and {prefix}kind='model' and {prefix}payload ? '{POST_COMPACTION_DISPATCH_KEY}'"
    )
}

/// The query that marks a single session's unfinished actions stale. Shared by
/// per-session recovery (`recover_session`) and `mark_unfinished_actions_stale`.
pub(super) fn stale_unfinished_actions_for_session() -> String {
    let unfinished_actions = action_is_unfinished(None);
    let post_compaction_dispatch = is_recoverable_post_compaction_dispatch(None);
    format!(
        "update actions set status='stale', payload=payload - '{POST_COMPACTION_DISPATCH_KEY}', updated_at=now() where session_id=$1 and {unfinished_actions} and not ({post_compaction_dispatch})"
    )
}

pub(super) fn stale_unfinished_actions() -> String {
    let unfinished_actions = action_is_unfinished(None);
    let post_compaction_dispatch = is_recoverable_post_compaction_dispatch(None);
    format!("{unfinished_actions} and not ({post_compaction_dispatch})")
}

pub(super) fn queued_input_is_active(alias: Option<&str>) -> String {
    status_is_one_of(alias, ACTIVE_QUEUED_INPUT_STATUSES)
}

pub(super) fn queued_input_is_editable(alias: Option<&str>) -> String {
    let column = qualified_status_column(alias);
    format!("{column}='queued'")
}

fn status_is_one_of(alias: Option<&str>, statuses: &str) -> String {
    let column = qualified_status_column(alias);
    format!("{column} in ({statuses})")
}

fn qualified_status_column(alias: Option<&str>) -> String {
    alias
        .map(|alias| format!("{alias}.status"))
        .unwrap_or_else(|| "status".to_string())
}

pub(super) async fn lock_session_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> anyhow::Result<()> {
    agent_perf::output_sql_statement();
    let locked = sqlx::query("select id from sessions where id=$1 for update")
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await?;
    if locked.is_none() {
        anyhow::bail!("session not found: {session_id}");
    }
    Ok(())
}

pub(super) async fn session_has_unfinished_actions_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> anyhow::Result<bool> {
    let unfinished_actions = action_is_unfinished(None);
    let query = format!(
        "select exists(select 1 from actions where session_id=$1 and {unfinished_actions})"
    );
    Ok(sqlx::query_scalar(&query)
        .bind(session_id)
        .fetch_one(&mut **tx)
        .await?)
}

pub(super) async fn session_has_active_queued_inputs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> anyhow::Result<bool> {
    let active_queue = queued_input_is_active(None);
    let query = format!(
        "select exists(select 1 from queued_inputs where session_id=$1 and {active_queue})"
    );
    Ok(sqlx::query_scalar(&query)
        .bind(session_id)
        .fetch_one(&mut **tx)
        .await?)
}

pub(super) async fn ensure_no_active_work_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> anyhow::Result<()> {
    if session_has_unfinished_actions_tx(tx, session_id).await?
        || session_has_active_queued_inputs_tx(tx, session_id).await?
    {
        return Err(crate::SourceMutationConflict.into());
    }
    Ok(())
}
