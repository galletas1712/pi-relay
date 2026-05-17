const UNFINISHED_ACTION_STATUSES: &str = "'pending','blocked','running'";
const ACTIVE_QUEUED_INPUT_STATUSES: &str = "'queued','consuming'";

pub(super) const QUEUED_INPUT_DISPATCH_ORDER: &str = r#"
    case priority when 'steer' then 0 else 1 end,
    case
        when priority='steer'
        then coalesce((origin->>'promoted_at')::timestamptz, created_at)
        else created_at
    end,
    created_at
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
