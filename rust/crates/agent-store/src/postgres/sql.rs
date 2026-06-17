use crate::SessionActivity;

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
    let locked = sqlx::query("select id from sessions where id=$1 for update")
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await?;
    if locked.is_none() {
        anyhow::bail!("session not found: {session_id}");
    }
    Ok(())
}
