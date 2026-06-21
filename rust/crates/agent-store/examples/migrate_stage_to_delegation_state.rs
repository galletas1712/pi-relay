//! TEMPORARY one-time migration for the hard `stage` -> `delegation` rename.
//!
//! This helper is intentionally not wired into normal daemon startup. Run it
//! once against an existing database before restarting code that has removed the
//! old `stage` vocabulary, then delete/abandon this temporary branch.
//!
//! The rename is intentionally hard: no long-term compatibility aliases are
//! added for old table names, RPC methods, or provider-visible tool names. The
//! migration updates the durable state the post-rename daemon needs, and fails
//! closed when old partial fan-out state cannot be inferred safely.
//!
//! The migration preserves existing row IDs, including IDs with a `stage_`
//! prefix. The post-rename runtime treats delegation IDs as opaque `text`; only
//! newly-created IDs use the `delegation_` prefix. Preserving IDs avoids a
//! risky global primary-key rewrite and keeps historical handoff paths stable.
//!
//! See `rust/docs/temp-stage-to-delegation-migration.md` for the operator
//! runbook, idempotency/rollback notes, and the exact data that is deliberately
//! not rewritten.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    database_url: Option<String>,
    apply: bool,
    no_backups: bool,
    handoff_root: Option<PathBuf>,
}

impl Config {
    fn parse_from<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut database_url = None;
        let mut apply = false;
        let mut dry_run = false;
        let mut no_backups = false;
        let mut handoff_root = None;

        let mut args = args.into_iter().map(Into::into).peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--database-url" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--database-url requires a value"))?;
                    database_url = Some(value);
                }
                "--apply" => apply = true,
                "--dry-run" => dry_run = true,
                "--no-backups" => no_backups = true,
                "--handoff-root" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--handoff-root requires a value"))?;
                    handoff_root = Some(PathBuf::from(value));
                }
                "--migrate-handoff-dirs" => bail!(
                    "--migrate-handoff-dirs is intentionally unsupported: this migration preserves \
                     existing stage_* IDs as opaque delegation IDs, so .pi-handoff/stage_* \
                     directory names must remain unchanged"
                ),
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                other => bail!("unknown argument: {other}"),
            }
        }

        if apply && dry_run {
            bail!("choose either --apply or --dry-run, not both");
        }

        Ok(Self {
            database_url,
            apply,
            no_backups,
            handoff_root,
        })
    }

    fn database_url(&self) -> Result<String> {
        self.database_url
            .clone()
            .or_else(|| env::var("PI_RELAY_DATABASE_URL").ok())
            .or_else(|| env::var("DATABASE_URL").ok())
            .ok_or_else(|| {
                anyhow!(
                    "missing database URL; pass --database-url or set PI_RELAY_DATABASE_URL/DATABASE_URL"
                )
            })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct JsonRewriteStats {
    tool_names: usize,
    object_keys: usize,
    string_values: usize,
    embedded_json_strings: usize,
}

impl JsonRewriteStats {
    fn add(&mut self, other: &Self) {
        self.tool_names += other.tool_names;
        self.object_keys += other.object_keys;
        self.string_values += other.string_values;
        self.embedded_json_strings += other.embedded_json_strings;
    }

    fn changed(&self) -> bool {
        self.tool_names > 0
            || self.object_keys > 0
            || self.string_values > 0
            || self.embedded_json_strings > 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TextRewriteStats {
    replacements: BTreeMap<&'static str, usize>,
}

impl TextRewriteStats {
    fn record(&mut self, label: &'static str, count: usize) {
        if count > 0 {
            *self.replacements.entry(label).or_insert(0) += count;
        }
    }

    fn add(&mut self, other: &Self) {
        for (key, value) in &other.replacements {
            *self.replacements.entry(key).or_insert(0) += value;
        }
    }

    fn changed(&self) -> bool {
        !self.replacements.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MigrationStats {
    schema_operations: Vec<String>,
    transcript_item_rows: usize,
    transcript_provider_replay_rows: usize,
    action_payload_rows: usize,
    action_result_rows: usize,
    queued_content_rows: usize,
    queued_origin_rows: usize,
    queued_client_input_id_rows: usize,
    event_type_rows: usize,
    event_payload_rows: usize,
    session_metadata_rows: usize,
    session_system_prompt_rows: usize,
    handoff_manifests: usize,
    handoff_stage_dirs_preserved: usize,
    json_rewrites: JsonRewriteStats,
    text_rewrites: TextRewriteStats,
}

impl MigrationStats {
    fn add_json(&mut self, stats: &JsonRewriteStats) {
        self.json_rewrites.add(stats);
    }

    fn add_text(&mut self, stats: &TextRewriteStats) {
        self.text_rewrites.add(stats);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse_from(env::args().skip(1))?;
    println!("stage -> delegation one-time migration");
    println!(
        "mode: {}",
        if config.apply {
            "APPLY (will mutate database/files)"
        } else {
            "dry-run (no mutations; pass --apply to mutate)"
        }
    );
    println!("backup guidance: stop pi-agentd first and take a pg_dump before applying.");
    if config.apply && config.no_backups {
        println!("table backup copies: disabled by --no-backups");
    } else if config.apply {
        println!(
            "table backup copies: will create *_stage_to_delegation_backup_<timestamp> tables"
        );
    }
    println!("ID policy: preserving existing IDs, including stage_* prefixes, as opaque text.");
    if let Some(root) = &config.handoff_root {
        println!("handoff root: {}", root.display());
        println!(
            "handoff directory rename: disabled; existing stage_* IDs and directories are preserved"
        );
    } else {
        println!("handoff root: not provided; filesystem artifacts will not be scanned.");
    }

    let database_url = config.database_url()?;
    let pool = PgPool::connect(&database_url)
        .await
        .context("connect to postgres")?;
    let mut tx = pool.begin().await?;
    sqlx::query("set lock_timeout = '5s'")
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;

    let mut stats = MigrationStats::default();
    if config.apply && !config.no_backups {
        create_table_backups(
            &mut tx,
            &mut stats,
            &[
                "sessions",
                "stages",
                "delegations",
                "transcript_entries",
                "actions",
                "queued_inputs",
                "events",
            ],
        )
        .await?;
    }
    migrate_schema(&mut tx, &config, &mut stats).await?;
    migrate_persisted_json(&mut tx, &config, &mut stats).await?;
    migrate_session_prompts(&mut tx, &config, &mut stats).await?;

    if config.apply {
        tx.commit().await.context("commit migration transaction")?;
    } else {
        tx.rollback()
            .await
            .context("rollback dry-run transaction")?;
    }
    pool.close().await;

    if let Some(root) = &config.handoff_root {
        migrate_handoff_artifacts(root, &config, &mut stats)
            .with_context(|| format!("migrate handoff artifacts under {}", root.display()))?;
    }

    print_summary(&stats, config.apply);
    Ok(())
}

async fn ensure_expected_subagents_shape(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &Config,
    stats: &mut MigrationStats,
) -> Result<()> {
    let has_column = column_exists(tx, "delegations", "expected_subagents").await?;
    if !has_column {
        validate_expected_subagents_inference(tx, "delegations").await?;
        stats.schema_operations.push(
            "add delegations.expected_subagents inferred from kind and existing child sessions"
                .to_string(),
        );
        if config.apply {
            sqlx::query("alter table delegations add column expected_subagents integer null")
                .execute(&mut **tx)
                .await
                .context("add nullable delegations.expected_subagents")?;
            populate_missing_delegation_expected_subagents(tx).await?;
            sqlx::query("alter table delegations alter column expected_subagents set default 1")
                .execute(&mut **tx)
                .await
                .context("set default on delegations.expected_subagents")?;
            sqlx::query("alter table delegations alter column expected_subagents set not null")
                .execute(&mut **tx)
                .await
                .context("set delegations.expected_subagents not null")?;
        }
        return Ok(());
    }

    let null_count: i64 = sqlx::query_scalar(
        "select count(*)::bigint from delegations where expected_subagents is null",
    )
    .fetch_one(&mut **tx)
    .await
    .context("count null delegations.expected_subagents")?;
    if null_count > 0 {
        validate_expected_subagents_inference(tx, "delegations").await?;
        stats.schema_operations.push(format!(
            "populate {null_count} null delegations.expected_subagents values by inference"
        ));
        if config.apply {
            populate_missing_delegation_expected_subagents(tx).await?;
        }
    }

    let under_counted = under_counted_readonly_fanouts(tx).await?;
    if under_counted > 0 {
        stats.schema_operations.push(format!(
            "repair {under_counted} readonly_fanout delegations whose expected_subagents is lower than the existing child-session count"
        ));
        if config.apply {
            repair_under_counted_readonly_fanouts(tx).await?;
        }
    }

    if config.apply {
        sqlx::query("alter table delegations alter column expected_subagents set default 1")
            .execute(&mut **tx)
            .await
            .context("set default on delegations.expected_subagents")?;
        sqlx::query("alter table delegations alter column expected_subagents set not null")
            .execute(&mut **tx)
            .await
            .context("set delegations.expected_subagents not null")?;
    }
    Ok(())
}

async fn under_counted_readonly_fanouts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<i64> {
    let child_count = child_session_count_expr(tx, "d").await?;
    let sql = format!(
        r#"
        select count(*)::bigint
        from delegations d
        where d.kind = 'readonly_fanout'
          and d.expected_subagents is not null
          and ({child_count}) > d.expected_subagents
        "#
    );
    sqlx::query_scalar(&sql)
        .fetch_one(&mut **tx)
        .await
        .context("count under-counted readonly_fanout delegations")
}

async fn repair_under_counted_readonly_fanouts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<()> {
    let child_count = child_session_count_expr(tx, "d").await?;
    let sql = format!(
        r#"
        update delegations d
        set expected_subagents = ({child_count})
        where d.kind = 'readonly_fanout'
          and d.expected_subagents is not null
          and ({child_count}) > d.expected_subagents
        "#
    );
    sqlx::query(&sql)
        .execute(&mut **tx)
        .await
        .context("repair under-counted readonly_fanout delegations")?;
    Ok(())
}

async fn populate_missing_delegation_expected_subagents(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<()> {
    let expected_expr = expected_subagents_expr(tx, "delegations", Some("d")).await?;
    let sql = format!(
        r#"
        update delegations d
        set expected_subagents = {expected_expr}
        where d.expected_subagents is null
        "#
    );
    sqlx::query(&sql)
        .execute(&mut **tx)
        .await
        .context("populate missing delegations.expected_subagents")?;
    Ok(())
}

async fn detect_client_input_id_rewrite_conflicts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    stage_rows: &[PgRow],
) -> Result<()> {
    for row in stage_rows {
        let id: String = row.get("id");
        let old: String = row.get("client_input_id");
        let new = rewrite_client_input_id_token(&old);
        if new == old {
            continue;
        }
        let conflict: Option<String> = sqlx::query_scalar(
            r#"
            select other.id
            from queued_inputs current
            join queued_inputs other
              on other.session_id = current.session_id
             and other.client_input_id = $3
             and other.id <> current.id
            where current.id = $1
              and current.client_input_id = $2
            limit 1
            "#,
        )
        .bind(&id)
        .bind(&old)
        .bind(&new)
        .fetch_optional(&mut **tx)
        .await
        .with_context(|| format!("check client_input_id conflict for queued input {id}"))?;
        if let Some(conflict_id) = conflict {
            bail!(
                "cannot rewrite queued_inputs.client_input_id for {id}: target {new:?} already exists on queued input {conflict_id}"
            );
        }
    }
    Ok(())
}

async fn migrate_event_types(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &Config,
    stats: &mut MigrationStats,
) -> Result<()> {
    if !table_exists(tx, "events").await? || !column_exists(tx, "events", "type").await? {
        return Ok(());
    }
    let rows = sqlx::query("select id, type from events where type like 'stage.%'")
        .fetch_all(&mut **tx)
        .await
        .context("read events.type")?;
    for row in rows {
        let id: i64 = row.get("id");
        let old: String = row.get("type");
        let new = rewrite_rpc_token(&old);
        if new == old {
            continue;
        }
        stats.event_type_rows += 1;
        stats.text_rewrites.record("events.type stage.*", 1);
        if config.apply {
            sqlx::query("update events set type=$2 where id=$1")
                .bind(id)
                .bind(&new)
                .execute(&mut **tx)
                .await
                .with_context(|| format!("update event {id} type"))?;
        }
    }
    Ok(())
}

async fn copy_stage_id_into_delegation_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<()> {
    sqlx::query(
        r#"
        update sessions
        set delegation_id = stage_id
        where delegation_id is null
          and stage_id is not null
        "#,
    )
    .execute(&mut **tx)
    .await
    .context("copy sessions.stage_id into empty delegation_id")?;
    Ok(())
}

async fn copy_missing_stage_rows_into_delegations(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<()> {
    if !column_exists(tx, "delegations", "expected_subagents").await? {
        sqlx::query("alter table delegations add column expected_subagents integer null")
            .execute(&mut **tx)
            .await
            .context("add nullable delegations.expected_subagents before copying stages")?;
    }
    let expected_expr = expected_subagents_expr(tx, "stages", Some("s")).await?;
    let sql = format!(
        r#"
        insert into delegations (
            id, parent_session_id, workflow, label, kind, status, attempt_id,
            created_at, updated_at, expected_subagents
        )
        select
            s.id, s.parent_session_id, s.workflow, s.label, s.kind, s.status, s.attempt_id,
            s.created_at, s.updated_at, {expected_expr}
        from stages s
        where not exists (
            select 1 from delegations d where d.id = s.id
        )
        "#,
    );
    sqlx::query(&sql)
        .execute(&mut **tx)
        .await
        .context("copy missing stages rows into delegations")?;
    Ok(())
}

async fn expected_subagents_expr(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    alias: Option<&str>,
) -> Result<String> {
    let exists = column_exists(tx, table, "expected_subagents").await?;
    let reference = alias.unwrap_or(table);
    let inferred = inferred_expected_subagents_expr(tx, table, reference).await?;
    if exists {
        Ok(format!(
            "coalesce({reference}.expected_subagents, {inferred})"
        ))
    } else {
        validate_expected_subagents_inference(tx, table).await?;
        Ok(inferred)
    }
}

async fn inferred_expected_subagents_expr(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    reference: &str,
) -> Result<String> {
    let child_count = child_session_count_expr(tx, reference).await?;
    Ok(match table {
        "stages" | "delegations" => format!(
            r#"
            case
              when {reference}.kind = 'full' then 1
              when {reference}.kind = 'readonly_fanout' then
                case when ({child_count}) > 0 then ({child_count}) else 1 end
              else 1
            end
            "#
        ),
        other => bail!("cannot infer expected_subagents for unexpected table {other}"),
    })
}

async fn child_session_count_expr(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    delegation_reference: &str,
) -> Result<String> {
    let mut predicates = Vec::new();
    if column_exists(tx, "sessions", "stage_id").await? {
        predicates.push(format!("child.stage_id = {delegation_reference}.id"));
    }
    if column_exists(tx, "sessions", "delegation_id").await? {
        predicates.push(format!("child.delegation_id = {delegation_reference}.id"));
    }
    if predicates.is_empty() {
        Ok("0".to_string())
    } else {
        Ok(format!(
            "select count(distinct child.id)::integer from sessions child where {}",
            predicates.join(" or ")
        ))
    }
}

async fn validate_expected_subagents_inference(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
) -> Result<()> {
    let child_count = child_session_count_expr(tx, table).await?;

    let ambiguous_sql = format!(
        r#"
        select id
        from {table}
        where kind = 'readonly_fanout'
          and status = 'running'
          and ({child_count}) = 0
        order by created_at, id
        limit 5
        "#
    );
    let ambiguous: Vec<String> = sqlx::query_scalar(&ambiguous_sql)
        .fetch_all(&mut **tx)
        .await
        .with_context(|| format!("check ambiguous running fan-out rows in {table}"))?;
    if !ambiguous.is_empty() {
        bail!(
            "cannot infer expected_subagents for running readonly_fanout rows without child sessions in {table}: {}. Stop the daemon, inspect these old stages, and either let spawning finish before migration or manually add/populate expected_subagents before rerunning.",
            ambiguous.join(", ")
        );
    }

    let corrupt_full_sql = format!(
        r#"
        select id
        from {table}
        where kind = 'full'
          and ({child_count}) > 1
        order by created_at, id
        limit 5
        "#
    );
    let corrupt_full: Vec<String> = sqlx::query_scalar(&corrupt_full_sql)
        .fetch_all(&mut **tx)
        .await
        .with_context(|| format!("check multi-child full rows in {table}"))?;
    if !corrupt_full.is_empty() {
        bail!(
            "cannot safely migrate full rows with more than one child session in {table}: {}. A full delegation must have exactly one subagent; inspect/repair the old rows before rerunning.",
            corrupt_full.join(", ")
        );
    }

    Ok(())
}

fn print_usage() {
    println!(
        r#"Usage:
  cargo run -p agent-store --example migrate_stage_to_delegation_state -- [options]

Options:
  --database-url URL        Postgres URL. Defaults to PI_RELAY_DATABASE_URL, then DATABASE_URL.
  --apply                   Mutate database/files. Without this flag the script is a dry-run.
  --dry-run                 Explicit dry-run (default).
  --no-backups              Skip table backup copies on --apply.
  --handoff-root PATH       Scan PATH/.pi-handoff for structured index.json manifests only.
  --help                    Show this help.

Recommended wrapper:
  rust/scripts/migrate-stage-to-delegation-state.sh [same options]

Runbook:
  1. Stop pi-agentd. This migration is not safe against a live daemon.
  2. Take a pg_dump backup.
  3. Run dry-run (default) and inspect the summary/conflicts.
  4. Run with --apply.
  5. Restart the post-rename daemon.

Policy:
  - Temporary one-time migration for the hard stage->delegation vocabulary rename;
    abandon/remove this branch/script after existing state has been migrated.
  - No compatibility aliases are added.
  - Existing stage_* IDs are preserved as opaque delegation IDs, so
    .pi-handoff/stage_* directory names are also preserved.
  - Arbitrary transcript/user/tool output text and handoff markdown are NOT
    rewritten. Only known structured DB/API/tool/RPC fields and handoff
    index.json manifests are migrated.
  - Old schemas without expected_subagents infer fan-out counts from existing
    child sessions and fail closed for ambiguous running zero-child fan-outs.

Detailed rationale and rollback guidance:
  rust/docs/temp-stage-to-delegation-migration.md
"#
    );
}

fn print_summary(stats: &MigrationStats, applied: bool) {
    println!();
    println!(
        "{} summary:",
        if applied {
            "applied migration"
        } else {
            "dry-run"
        }
    );
    if stats.schema_operations.is_empty() {
        println!("  schema: already in delegation shape or no old stage schema present");
    } else {
        println!("  schema operations:");
        for operation in &stats.schema_operations {
            println!("    - {operation}");
        }
    }
    println!("  JSON/text DB rows changed:");
    println!(
        "    transcript_entries.item: {}",
        stats.transcript_item_rows
    );
    println!(
        "    transcript_entries.provider_replay: {}",
        stats.transcript_provider_replay_rows
    );
    println!("    actions.payload: {}", stats.action_payload_rows);
    println!("    actions.result: {}", stats.action_result_rows);
    println!("    queued_inputs.content: {}", stats.queued_content_rows);
    println!("    queued_inputs.origin: {}", stats.queued_origin_rows);
    println!(
        "    queued_inputs.client_input_id: {}",
        stats.queued_client_input_id_rows
    );
    println!("    events.type: {}", stats.event_type_rows);
    println!("    events.payload: {}", stats.event_payload_rows);
    println!("    sessions.metadata: {}", stats.session_metadata_rows);
    println!(
        "    sessions.system_prompt: {}",
        stats.session_system_prompt_rows
    );
    println!("  JSON rewrite counts: {:?}", stats.json_rewrites);
    println!(
        "  text rewrite counts: {:?}",
        stats.text_rewrites.replacements
    );
    println!("  handoff manifests changed: {}", stats.handoff_manifests);
    println!(
        "  handoff stage_* dirs preserved: {}",
        stats.handoff_stage_dirs_preserved
    );
}

async fn migrate_schema(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &Config,
    stats: &mut MigrationStats,
) -> Result<()> {
    let has_stages = table_exists(tx, "stages").await?;
    let has_delegations = table_exists(tx, "delegations").await?;
    let has_stage_id = column_exists(tx, "sessions", "stage_id").await?;
    let has_delegation_id = column_exists(tx, "sessions", "delegation_id").await?;

    if !has_stages && !has_stage_id {
        if has_delegations {
            ensure_delegation_schema_shape(tx, config, stats).await?;
        }
        return Ok(());
    }

    detect_schema_conflicts(
        tx,
        has_stages,
        has_delegations,
        has_stage_id,
        has_delegation_id,
    )
    .await?;

    if has_stages && !column_exists(tx, "stages", "expected_subagents").await? {
        validate_expected_subagents_inference(tx, "stages").await?;
    }

    if config.apply && has_stages && has_delegations {
        copy_missing_stage_rows_into_delegations(tx).await?;
    }

    if has_stage_id && has_delegation_id {
        stats
            .schema_operations
            .push("sessions has both stage_id and delegation_id with matching data; dropping old stage_id".to_string());
        if config.apply {
            copy_stage_id_into_delegation_id(tx).await?;
            drop_column_constraints(tx, "sessions", "stage_id").await?;
            sqlx::query("alter table sessions drop column stage_id")
                .execute(&mut **tx)
                .await
                .context("drop sessions.stage_id")?;
        }
    } else if has_stage_id {
        stats
            .schema_operations
            .push("rename sessions.stage_id -> sessions.delegation_id".to_string());
        if config.apply {
            rename_column_if_needed(tx, "sessions", "stage_id", "delegation_id").await?;
        }
    }

    if has_stages && has_delegations {
        stats
            .schema_operations
            .push("stages and delegations both exist with matching rows; dropping empty/duplicate old stages".to_string());
        if config.apply {
            drop_table_constraints_referencing(tx, "stages").await?;
            sqlx::query("drop table stages")
                .execute(&mut **tx)
                .await
                .context("drop old stages table")?;
        }
    } else if has_stages {
        stats
            .schema_operations
            .push("rename stages -> delegations".to_string());
        if config.apply {
            drop_table_constraints_referencing(tx, "stages").await?;
            sqlx::query("alter table stages rename to delegations")
                .execute(&mut **tx)
                .await
                .context("rename stages table")?;
        }
    }

    ensure_delegation_schema_shape(tx, config, stats).await?;
    Ok(())
}

async fn ensure_delegation_schema_shape(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &Config,
    stats: &mut MigrationStats,
) -> Result<()> {
    if !table_exists(tx, "delegations").await? {
        return Ok(());
    }

    if !column_exists(tx, "sessions", "delegation_id").await? {
        stats
            .schema_operations
            .push("add sessions.delegation_id".to_string());
        if config.apply {
            sqlx::query("alter table sessions add column delegation_id text null")
                .execute(&mut **tx)
                .await
                .context("add sessions.delegation_id")?;
        }
    }

    ensure_expected_subagents_shape(tx, config, stats).await?;

    rename_constraints(tx, config, stats).await?;

    if column_exists(tx, "sessions", "delegation_id").await?
        && !fk_exists(tx, "sessions", "sessions_delegation_id_fkey").await?
    {
        stats
            .schema_operations
            .push("add sessions.delegation_id foreign key to delegations(id)".to_string());
        if config.apply {
            sqlx::query(
                r#"
                alter table sessions
                add constraint sessions_delegation_id_fkey
                foreign key (delegation_id) references delegations(id)
                "#,
            )
            .execute(&mut **tx)
            .await
            .context("add sessions.delegation_id foreign key")?;
        }
    }

    let old_index = index_exists(tx, "stages_parent_created_idx").await?;
    let new_index = index_exists(tx, "delegations_parent_created_idx").await?;
    if old_index && !new_index {
        stats.schema_operations.push(
            "rename index stages_parent_created_idx -> delegations_parent_created_idx".to_string(),
        );
        if config.apply {
            sqlx::query(
                "alter index stages_parent_created_idx rename to delegations_parent_created_idx",
            )
            .execute(&mut **tx)
            .await
            .context("rename stages_parent_created_idx")?;
        }
    } else if !new_index {
        stats
            .schema_operations
            .push("create delegations_parent_created_idx".to_string());
        if config.apply {
            sqlx::query(
                "create index delegations_parent_created_idx on delegations(parent_session_id, created_at, id)",
            )
            .execute(&mut **tx)
            .await
            .context("create delegations_parent_created_idx")?;
        }
    }
    Ok(())
}

async fn detect_schema_conflicts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    has_stages: bool,
    has_delegations: bool,
    has_stage_id: bool,
    has_delegation_id: bool,
) -> Result<()> {
    if has_stages && has_delegations {
        let stage_expected = expected_subagents_expr(tx, "stages", None).await?;
        let delegation_expected = expected_subagents_expr(tx, "delegations", None).await?;
        let old_count: i64 = sqlx::query_scalar("select count(*)::bigint from stages")
            .fetch_one(&mut **tx)
            .await
            .context("count stages")?;
        let new_count: i64 = sqlx::query_scalar("select count(*)::bigint from delegations")
            .fetch_one(&mut **tx)
            .await
            .context("count delegations")?;
        let diff_sql = format!(
            r#"
            select count(*)::bigint
            from (
                (select id, parent_session_id, workflow, label, kind, status, attempt_id,
                    created_at, updated_at,
                    {stage_expected} as expected_subagents
                 from stages
                 except
                 select id, parent_session_id, workflow, label, kind, status, attempt_id,
                    created_at, updated_at,
                    {delegation_expected} as expected_subagents
                 from delegations)
                union all
                (select id, parent_session_id, workflow, label, kind, status, attempt_id,
                    created_at, updated_at,
                    {delegation_expected} as expected_subagents
                 from delegations
                 except
                 select id, parent_session_id, workflow, label, kind, status, attempt_id,
                    created_at, updated_at,
                    {stage_expected} as expected_subagents
                 from stages)
            ) diff
            "#,
        );
        let diff: i64 = sqlx::query_scalar(&diff_sql)
            .fetch_one(&mut **tx)
            .await
            .context("compare stages/delegations rows")?;
        if old_count > 0 && new_count > 0 && diff > 0 {
            let stage_expected = expected_subagents_expr(tx, "stages", Some("s")).await?;
            let delegation_expected = expected_subagents_expr(tx, "delegations", Some("d")).await?;
            let overlap_sql = format!(
                r#"
                select count(*)::bigint
                from stages s
                join delegations d using (id)
                where (s.parent_session_id, s.workflow, s.label, s.kind, s.status, s.attempt_id,
                       s.created_at, s.updated_at, {stage_expected})
                   is distinct from
                      (d.parent_session_id, d.workflow, d.label, d.kind, d.status, d.attempt_id,
                       d.created_at, d.updated_at, {delegation_expected})
                "#,
            );
            let overlap_conflicts: i64 = sqlx::query_scalar(&overlap_sql)
                .fetch_one(&mut **tx)
                .await
                .context("compare overlapping stages/delegations rows")?;
            if overlap_conflicts > 0 {
                bail!(
                    "both stages and delegations contain conflicting rows for the same IDs; refusing to merge automatically"
                );
            }
        }
    }

    if has_stage_id && has_delegation_id {
        let conflicts: i64 = sqlx::query_scalar(
            r#"
            select count(*)::bigint
            from sessions
            where stage_id is distinct from delegation_id
              and stage_id is not null
              and delegation_id is not null
            "#,
        )
        .fetch_one(&mut **tx)
        .await
        .context("compare sessions stage_id/delegation_id")?;
        if conflicts > 0 {
            bail!("sessions has both stage_id and delegation_id with conflicting non-null values");
        }
    }

    Ok(())
}

async fn create_table_backups(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    stats: &mut MigrationStats,
    tables: &[&str],
) -> Result<()> {
    let suffix: String =
        sqlx::query_scalar("select to_char(clock_timestamp(), 'YYYYMMDD_HH24MISS')")
            .fetch_one(&mut **tx)
            .await
            .context("backup timestamp")?;
    for table in tables {
        if table_exists(tx, table).await? {
            let backup = format!("{table}_stage_to_delegation_backup_{suffix}");
            let sql = format!("create table {backup} as table {table}");
            sqlx::query(&sql)
                .execute(&mut **tx)
                .await
                .with_context(|| format!("create backup table {backup}"))?;
            stats
                .schema_operations
                .push(format!("created backup table {backup}"));
        }
    }
    Ok(())
}

async fn rename_constraints(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &Config,
    stats: &mut MigrationStats,
) -> Result<()> {
    for (table, old, new) in [
        ("delegations", "stages_pkey", "delegations_pkey"),
        (
            "delegations",
            "stages_parent_session_id_fkey",
            "delegations_parent_session_id_fkey",
        ),
        (
            "sessions",
            "sessions_stage_id_fkey",
            "sessions_delegation_id_fkey",
        ),
    ] {
        if constraint_exists(tx, table, old).await? && !constraint_exists(tx, table, new).await? {
            stats
                .schema_operations
                .push(format!("rename constraint {old} -> {new}"));
            if config.apply {
                let sql = format!("alter table {table} rename constraint {old} to {new}");
                sqlx::query(&sql)
                    .execute(&mut **tx)
                    .await
                    .with_context(|| format!("rename constraint {old}"))?;
            }
        }
    }
    Ok(())
}

async fn drop_column_constraints(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    column: &str,
) -> Result<()> {
    let rows = sqlx::query(
        r#"
        select conname
        from pg_constraint c
        join pg_class t on t.oid = c.conrelid
        join pg_namespace n on n.oid = t.relnamespace
        where n.nspname = current_schema()
          and t.relname = $1
          and exists (
            select 1
            from unnest(c.conkey) attnum
            join pg_attribute a on a.attrelid = t.oid and a.attnum = attnum
            where a.attname = $2
          )
        "#,
    )
    .bind(table)
    .bind(column)
    .fetch_all(&mut **tx)
    .await
    .with_context(|| format!("find constraints on {table}.{column}"))?;
    for row in rows {
        let constraint: String = row.get("conname");
        let sql = format!("alter table {table} drop constraint {constraint}");
        sqlx::query(&sql)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("drop constraint {constraint}"))?;
    }
    Ok(())
}

async fn drop_table_constraints_referencing(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    referenced_table: &str,
) -> Result<()> {
    let rows = sqlx::query(
        r#"
        select source.relname as table_name, c.conname
        from pg_constraint c
        join pg_class source on source.oid = c.conrelid
        join pg_class target on target.oid = c.confrelid
        join pg_namespace n on n.oid = source.relnamespace
        where n.nspname = current_schema()
          and c.contype = 'f'
          and target.relname = $1
        "#,
    )
    .bind(referenced_table)
    .fetch_all(&mut **tx)
    .await
    .with_context(|| format!("find constraints referencing {referenced_table}"))?;
    for row in rows {
        let table_name: String = row.get("table_name");
        let constraint: String = row.get("conname");
        let sql = format!("alter table {table_name} drop constraint {constraint}");
        sqlx::query(&sql)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("drop constraint {constraint}"))?;
    }
    Ok(())
}

async fn rename_column_if_needed(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    old: &str,
    new: &str,
) -> Result<()> {
    if column_exists(tx, table, old).await? && !column_exists(tx, table, new).await? {
        let sql = format!("alter table {table} rename column {old} to {new}");
        sqlx::query(&sql)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("rename {table}.{old}"))?;
    }
    Ok(())
}

async fn table_exists(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, table: &str) -> Result<bool> {
    sqlx::query_scalar(
        r#"
        select exists(
            select 1
            from information_schema.tables
            where table_schema = current_schema()
              and table_name = $1
        )
        "#,
    )
    .bind(table)
    .fetch_one(&mut **tx)
    .await
    .with_context(|| format!("check table {table}"))
}

async fn column_exists(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    column: &str,
) -> Result<bool> {
    sqlx::query_scalar(
        r#"
        select exists(
            select 1
            from information_schema.columns
            where table_schema = current_schema()
              and table_name = $1
              and column_name = $2
        )
        "#,
    )
    .bind(table)
    .bind(column)
    .fetch_one(&mut **tx)
    .await
    .with_context(|| format!("check column {table}.{column}"))
}

async fn index_exists(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, index: &str) -> Result<bool> {
    sqlx::query_scalar("select to_regclass($1)::text is not null")
        .bind(index)
        .fetch_one(&mut **tx)
        .await
        .with_context(|| format!("check index {index}"))
}

async fn constraint_exists(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    constraint: &str,
) -> Result<bool> {
    sqlx::query_scalar(
        r#"
        select exists(
            select 1
            from pg_constraint c
            join pg_class t on t.oid = c.conrelid
            join pg_namespace n on n.oid = t.relnamespace
            where n.nspname = current_schema()
              and t.relname = $1
              and c.conname = $2
        )
        "#,
    )
    .bind(table)
    .bind(constraint)
    .fetch_one(&mut **tx)
    .await
    .with_context(|| format!("check constraint {constraint} on {table}"))
}

async fn fk_exists(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    constraint: &str,
) -> Result<bool> {
    sqlx::query_scalar(
        r#"
        select exists(
            select 1
            from pg_constraint c
            join pg_class t on t.oid = c.conrelid
            join pg_namespace n on n.oid = t.relnamespace
            where n.nspname = current_schema()
              and t.relname = $1
              and c.conname = $2
              and c.contype = 'f'
        )
        "#,
    )
    .bind(table)
    .bind(constraint)
    .fetch_one(&mut **tx)
    .await
    .with_context(|| format!("check foreign key {constraint} on {table}"))
}

async fn migrate_persisted_json(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &Config,
    stats: &mut MigrationStats,
) -> Result<()> {
    migrate_json_column(
        tx,
        config,
        stats,
        "transcript_entries",
        &["session_id", "id"],
        "item",
        |stats| stats.transcript_item_rows += 1,
    )
    .await?;
    migrate_json_column(
        tx,
        config,
        stats,
        "transcript_entries",
        &["session_id", "id"],
        "provider_replay",
        |stats| stats.transcript_provider_replay_rows += 1,
    )
    .await?;
    migrate_json_column(tx, config, stats, "actions", &["id"], "payload", |stats| {
        stats.action_payload_rows += 1
    })
    .await?;
    migrate_json_column(tx, config, stats, "actions", &["id"], "result", |stats| {
        stats.action_result_rows += 1
    })
    .await?;
    migrate_json_column(
        tx,
        config,
        stats,
        "queued_inputs",
        &["id"],
        "content",
        |stats| stats.queued_content_rows += 1,
    )
    .await?;
    migrate_json_column(
        tx,
        config,
        stats,
        "queued_inputs",
        &["id"],
        "origin",
        |stats| stats.queued_origin_rows += 1,
    )
    .await?;
    migrate_generated_completion_steer_content(tx, config, stats).await?;
    migrate_json_column(tx, config, stats, "events", &["id"], "payload", |stats| {
        stats.event_payload_rows += 1
    })
    .await?;
    migrate_json_column(
        tx,
        config,
        stats,
        "sessions",
        &["id"],
        "metadata",
        |stats| stats.session_metadata_rows += 1,
    )
    .await?;
    migrate_client_input_ids(tx, config, stats).await?;
    migrate_event_types(tx, config, stats).await?;
    Ok(())
}

async fn migrate_client_input_ids(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &Config,
    stats: &mut MigrationStats,
) -> Result<()> {
    if !table_exists(tx, "queued_inputs").await?
        || !column_exists(tx, "queued_inputs", "client_input_id").await?
    {
        return Ok(());
    }
    let rows = sqlx::query(
        "select id, client_input_id from queued_inputs where client_input_id like 'stage-steer:%'",
    )
    .fetch_all(&mut **tx)
    .await
    .context("read queued_inputs.client_input_id")?;
    detect_client_input_id_rewrite_conflicts(tx, &rows).await?;
    for row in rows {
        let id: String = row.get("id");
        let old: String = row.get("client_input_id");
        let new = rewrite_token(&old);
        if new == old {
            continue;
        }
        stats.queued_client_input_id_rows += 1;
        stats
            .text_rewrites
            .record("queued_inputs.client_input_id stage-steer", 1);
        if config.apply {
            sqlx::query("update queued_inputs set client_input_id=$2 where id=$1")
                .bind(&id)
                .bind(&new)
                .execute(&mut **tx)
                .await
                .with_context(|| format!("update queued input {id} client_input_id"))?;
        }
    }
    Ok(())
}

async fn migrate_generated_completion_steer_content(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &Config,
    stats: &mut MigrationStats,
) -> Result<()> {
    if !table_exists(tx, "queued_inputs").await?
        || !column_exists(tx, "queued_inputs", "client_input_id").await?
        || !column_exists(tx, "queued_inputs", "content").await?
    {
        return Ok(());
    }
    let rows = sqlx::query(
        r#"
        select id, content
        from queued_inputs
        where client_input_id like 'stage-steer:%'
           or client_input_id like 'delegation-steer:%'
        "#,
    )
    .fetch_all(&mut **tx)
    .await
    .context("read generated delegation completion steers")?;

    for row in rows {
        let id: String = row.get("id");
        let mut content: Value = row.get("content");
        let replacements = rewrite_generated_completion_steer_value(&mut content);
        if replacements == 0 {
            continue;
        }
        stats.queued_content_rows += 1;
        stats.text_rewrites.record(
            "queued_inputs.content generated completion steer",
            replacements,
        );
        if config.apply {
            sqlx::query("update queued_inputs set content=$2 where id=$1")
                .bind(&id)
                .bind(&content)
                .execute(&mut **tx)
                .await
                .with_context(|| format!("update generated completion steer {id}"))?;
        }
    }
    Ok(())
}

fn rewrite_generated_completion_steer_value(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => map
            .values_mut()
            .map(rewrite_generated_completion_steer_value)
            .sum(),
        Value::Array(items) => items
            .iter_mut()
            .map(rewrite_generated_completion_steer_value)
            .sum(),
        Value::String(text) => {
            let rewritten = rewrite_generated_completion_steer_text(text);
            if rewritten == *text {
                0
            } else {
                *text = rewritten;
                1
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

fn rewrite_generated_completion_steer_text(input: &str) -> String {
    let mut output = input.to_string();
    if let Some(rest) = output.strip_prefix("Stage ") {
        output = format!("Delegation {rest}");
    }
    output = output.replace("stage handoff", "delegation handoff");
    output
}

async fn migrate_json_column<F>(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &Config,
    stats: &mut MigrationStats,
    table: &str,
    key_columns: &[&str],
    json_column: &str,
    mut row_counter: F,
) -> Result<()>
where
    F: FnMut(&mut MigrationStats),
{
    if !table_exists(tx, table).await? || !column_exists(tx, table, json_column).await? {
        return Ok(());
    }

    let key_select = key_columns.join(", ");
    let sql =
        format!("select {key_select}, {json_column} from {table} where {json_column} is not null");
    let rows = sqlx::query(&sql)
        .fetch_all(&mut **tx)
        .await
        .with_context(|| format!("read {table}.{json_column}"))?;

    for row in rows {
        let mut value: Value = row.get(json_column);
        let rewrite_stats = rewrite_json_value(&mut value)
            .with_context(|| format!("rewrite {table}.{json_column} JSON"))?;
        if !rewrite_stats.changed() {
            continue;
        }
        stats.add_json(&rewrite_stats);
        row_counter(stats);
        if config.apply {
            update_json_row(tx, table, key_columns, json_column, &row, &value).await?;
        }
    }
    Ok(())
}

async fn update_json_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    key_columns: &[&str],
    json_column: &str,
    row: &PgRow,
    value: &Value,
) -> Result<()> {
    let assignments = format!("{json_column} = $1");
    let predicates = key_columns
        .iter()
        .enumerate()
        .map(|(index, key)| format!("{key} = ${}", index + 2))
        .collect::<Vec<_>>()
        .join(" and ");
    let sql = format!("update {table} set {assignments} where {predicates}");
    let mut query = sqlx::query(&sql).bind(value);
    for key in key_columns {
        if *key == "id" && table == "events" {
            query = query.bind(row.get::<i64, _>(*key));
        } else {
            query = query.bind(row.get::<String, _>(*key));
        }
    }
    query
        .execute(&mut **tx)
        .await
        .with_context(|| format!("update {table}.{json_column}"))?;
    Ok(())
}

async fn migrate_session_prompts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &Config,
    stats: &mut MigrationStats,
) -> Result<()> {
    if !table_exists(tx, "sessions").await?
        || !column_exists(tx, "sessions", "system_prompt").await?
    {
        return Ok(());
    }

    let rows = sqlx::query("select id, system_prompt from sessions")
        .fetch_all(&mut **tx)
        .await
        .context("read sessions.system_prompt")?;
    for row in rows {
        let id: String = row.get("id");
        let prompt: String = row.get("system_prompt");
        let (rewritten, text_stats) = rewrite_system_prompt(&prompt);
        if !text_stats.changed() {
            continue;
        }
        stats.session_system_prompt_rows += 1;
        stats.add_text(&text_stats);
        if config.apply {
            sqlx::query("update sessions set system_prompt=$2 where id=$1")
                .bind(&id)
                .bind(&rewritten)
                .execute(&mut **tx)
                .await
                .with_context(|| format!("update system_prompt for session {id}"))?;
        }
    }
    Ok(())
}

fn rewrite_json_value(value: &mut Value) -> Result<JsonRewriteStats> {
    rewrite_json_value_with_id_keys(value, true)
}

fn rewrite_json_value_with_id_keys(
    value: &mut Value,
    rewrite_id_keys: bool,
) -> Result<JsonRewriteStats> {
    let mut stats = JsonRewriteStats::default();
    rewrite_json_value_inner(value, &mut stats, rewrite_id_keys)?;
    Ok(stats)
}

fn rewrite_json_value_inner(
    value: &mut Value,
    stats: &mut JsonRewriteStats,
    rewrite_id_keys: bool,
) -> Result<()> {
    match value {
        Value::Object(map) => {
            if rewrite_id_keys {
                rewrite_object_keys(map, stats)?;
            }
            rewrite_contextual_object_values(map, stats)?;
            for child in map.values_mut() {
                rewrite_json_value_inner(child, stats, rewrite_id_keys)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_json_value_inner(item, stats, rewrite_id_keys)?;
            }
        }
        Value::String(_) | Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn rewrite_object_keys(map: &mut Map<String, Value>, stats: &mut JsonRewriteStats) -> Result<()> {
    for (old, new) in [("stage_id", "delegation_id"), ("stageId", "delegationId")] {
        rewrite_known_object_key(map, stats, old, new)?;
    }
    Ok(())
}

fn rewrite_known_object_key(
    map: &mut Map<String, Value>,
    stats: &mut JsonRewriteStats,
    old: &'static str,
    new: &'static str,
) -> Result<()> {
    let Some(old_value) = map.get(old).cloned() else {
        return Ok(());
    };

    if let Some(new_value) = map.get(new).cloned() {
        if !old_value.is_null() && !new_value.is_null() && old_value != new_value {
            bail!(
                "conflicting JSON keys {old:?} and {new:?}: old value {old_value:?}, new value {new_value:?}"
            );
        }
        if new_value.is_null() && !old_value.is_null() {
            map.insert(new.to_string(), old_value);
        }
        map.remove(old);
        stats.object_keys += 1;
        return Ok(());
    }

    map.remove(old);
    map.insert(new.to_string(), old_value);
    stats.object_keys += 1;
    Ok(())
}

fn rewrite_contextual_object_values(
    map: &mut Map<String, Value>,
    stats: &mut JsonRewriteStats,
) -> Result<()> {
    for key in ["tool_name", "canonical_name", "prompt_alias"] {
        let Some(Value::String(text)) = map.get_mut(key) else {
            continue;
        };
        let rewritten = rewrite_model_tool_token(text);
        if rewritten != *text {
            *text = rewritten;
            stats.tool_names += 1;
        }
    }

    if object_type_is_rpc_context(map) {
        if let Some(Value::String(text)) = map.get_mut("type") {
            let rewritten = rewrite_rpc_token(text);
            if rewritten != *text {
                *text = rewritten;
                stats.tool_names += 1;
            }
        }
    }

    if object_name_is_tool_context(map) {
        if let Some(Value::String(text)) = map.get_mut("name") {
            let rewritten = rewrite_model_tool_token(text);
            if rewritten != *text {
                *text = rewritten;
                stats.tool_names += 1;
            }
        }
    }

    for key in ["method", "rpc"] {
        let Some(Value::String(text)) = map.get_mut(key) else {
            continue;
        };
        let rewritten = rewrite_rpc_token(text);
        if rewritten != *text {
            *text = rewritten;
            stats.tool_names += 1;
        }
    }

    if let Some(Value::String(text)) = map.get_mut("client_input_id") {
        let rewritten = rewrite_client_input_id_token(text);
        if rewritten != *text {
            *text = rewritten;
            stats.tool_names += 1;
        }
    }

    if object_is_provider_replay_item(map) {
        rewrite_json_string_field(map, stats, "raw_json")?;
    }

    if object_has_delegation_tool_name(map) {
        for key in ["args_json", "arguments"] {
            rewrite_json_string_field(map, stats, key)?;
        }
        rewrite_tool_result_output(map, stats)?;
    }

    if object_has_delegation_context(map) {
        rewrite_object_keys(map, stats)?;
    }

    Ok(())
}

fn rewrite_json_string_field(
    map: &mut Map<String, Value>,
    stats: &mut JsonRewriteStats,
    key: &'static str,
) -> Result<()> {
    let Some(Value::String(text)) = map.get_mut(key) else {
        return Ok(());
    };
    let Ok(mut embedded) = serde_json::from_str::<Value>(text) else {
        return Ok(());
    };
    let nested = rewrite_json_value_with_id_keys(&mut embedded, true)?;
    if !nested.changed() {
        return Ok(());
    }
    *text = serde_json::to_string(&embedded)
        .with_context(|| format!("serialize rewritten JSON string field {key}"))?;
    stats.embedded_json_strings += 1;
    stats.add(&nested);
    Ok(())
}

fn rewrite_tool_result_output(
    map: &mut Map<String, Value>,
    stats: &mut JsonRewriteStats,
) -> Result<()> {
    let Some(Value::String(text)) = map.get_mut("output") else {
        return Ok(());
    };
    if let Ok(mut embedded) = serde_json::from_str::<Value>(text) {
        let nested = rewrite_json_value_with_id_keys(&mut embedded, true)?;
        if nested.changed() {
            *text = serde_json::to_string(&embedded)
                .context("serialize rewritten delegation tool output JSON")?;
            stats.embedded_json_strings += 1;
            stats.add(&nested);
        }
        return Ok(());
    }

    let mut rewritten = text.replace("\"stage_id\"", "\"delegation_id\"");
    rewritten = rewritten.replace("\"stageId\"", "\"delegationId\"");
    if rewritten != *text {
        *text = rewritten;
        stats.string_values += 1;
    }
    Ok(())
}

fn object_has_delegation_tool_name(map: &Map<String, Value>) -> bool {
    ["tool_name", "canonical_name", "prompt_alias"]
        .iter()
        .filter_map(|key| map.get(*key).and_then(Value::as_str))
        .any(is_old_or_new_delegation_tool_name)
        || object_name_is_tool_context(map)
}

fn object_is_provider_replay_item(map: &Map<String, Value>) -> bool {
    map.contains_key("raw_json") && map.get("provider").and_then(Value::as_str).is_some()
}

fn object_name_is_tool_context(map: &Map<String, Value>) -> bool {
    map.get("name")
        .and_then(Value::as_str)
        .is_some_and(is_old_or_new_delegation_tool_name)
        && (map.contains_key("arguments")
            || map.contains_key("input_schema")
            || map.contains_key("description")
            || map.get("type").and_then(Value::as_str) == Some("function_call")
            || map.get("type").and_then(Value::as_str) == Some("tool_use"))
}

fn object_type_is_rpc_context(map: &Map<String, Value>) -> bool {
    map.get("type")
        .and_then(Value::as_str)
        .is_some_and(is_old_or_new_delegation_rpc_name)
        && (map.contains_key("stage_id")
            || map.contains_key("delegation_id")
            || map.contains_key("stageId")
            || map.contains_key("delegationId")
            || map.contains_key("params")
            || map.contains_key("payload")
            || map.contains_key("method")
            || map.contains_key("rpc"))
}

fn object_has_delegation_context(map: &Map<String, Value>) -> bool {
    object_has_delegation_tool_name(map)
        || ["method", "rpc"]
            .iter()
            .filter_map(|key| map.get(*key).and_then(Value::as_str))
            .any(is_old_or_new_delegation_rpc_name)
        || object_type_is_rpc_context(map)
}

fn is_old_or_new_delegation_tool_name(value: &str) -> bool {
    matches!(
        value,
        "delegate_writing_task"
            | "delegate_readonly_tasks"
            | "inspect_delegation"
            | "cancel_delegation"
            | "stage.full"
            | "stage.start_full"
            | "stage_start_full"
            | "stage_full"
            | "stage.fanout"
            | "stage.start_ro_fan"
            | "stage.start_readonly_fanout"
            | "stage_start_readonly_fanout"
            | "stage_start_ro_fan"
            | "stage_fanout"
            | "stage.status"
            | "stage_status"
            | "stage.cancel"
            | "stage_cancel"
    )
}

fn is_old_or_new_delegation_rpc_name(value: &str) -> bool {
    matches!(
        value,
        "delegation.start_full"
            | "delegation.start_readonly_fanout"
            | "delegation.status"
            | "delegation.cancel"
            | "delegation.list"
            | "delegation.read_handoff_file"
            | "stage.full"
            | "stage.start_full"
            | "stage.fanout"
            | "stage.start_ro_fan"
            | "stage.start_readonly_fanout"
            | "stage.status"
            | "stage.cancel"
            | "stage.list"
            | "stage.read_handoff_file"
    )
}

fn rewrite_model_tool_token(value: &str) -> String {
    match value {
        "stage.full" | "stage.start_full" | "stage_start_full" | "stage_full" => {
            "delegate_writing_task".to_string()
        }
        "stage.fanout" | "stage_start_readonly_fanout" | "stage_start_ro_fan" | "stage_fanout" => {
            "delegate_readonly_tasks".to_string()
        }
        "stage.start_ro_fan" | "stage.start_readonly_fanout" => {
            "delegate_readonly_tasks".to_string()
        }
        "stage.status" | "stage_status" => "inspect_delegation".to_string(),
        "stage.cancel" | "stage_cancel" => "cancel_delegation".to_string(),
        other => other.to_string(),
    }
}

fn rewrite_rpc_token(value: &str) -> String {
    match value {
        "stage.full" | "stage.start_full" => "delegation.start_full".to_string(),
        "stage.fanout" | "stage.start_ro_fan" | "stage.start_readonly_fanout" => {
            "delegation.start_readonly_fanout".to_string()
        }
        "stage.status" => "delegation.status".to_string(),
        "stage.cancel" => "delegation.cancel".to_string(),
        "stage.list" => "delegation.list".to_string(),
        "stage.read_handoff_file" => "delegation.read_handoff_file".to_string(),
        other => other.to_string(),
    }
}

fn rewrite_client_input_id_token(value: &str) -> String {
    if let Some(suffix) = value.strip_prefix("stage-steer:") {
        format!("delegation-steer:{suffix}")
    } else {
        value.to_string()
    }
}

fn rewrite_token(value: &str) -> String {
    let model = rewrite_model_tool_token(value);
    if model != value {
        return model;
    }
    let rpc = rewrite_rpc_token(value);
    if rpc != value {
        return rpc;
    }
    rewrite_client_input_id_token(value)
}

const CURRENT_SUBAGENT_DELEGATION_SECTION: &str = r#"## Subagent delegation

Delegate work to subagents through delegation tool calls. Do not use the Python REPL
to orchestrate subagents.

Two kinds of subagent:

- **read-only (RO)** — for investigation, review, analysis, and running
  builds/tests to gather information. RO subagents run in a private throwaway copy
  of the workspace; nothing they write reaches your workspace. Use
  `delegate_readonly_tasks` to run several in parallel.
- **full** — for making changes. A full subagent edits your workspace in place.
  Use `delegate_writing_task`. There is exactly one full subagent at a time.

Rules:

- Launch at most one delegation per turn, then end your turn. Do not poll or loop —
  you will be notified.
- When a delegation finishes you receive a short message pointing at a handoff
  directory. Read its `index.json` first, then each subagent's
  `final_message.md`; open `transcript.md` only if you need detail.
- Give each subagent a self-contained task: it starts with fresh context and only
  knows what you put in its prompt (and any handoff/workspace paths you cite).
- While a full subagent is running, supervise and read — do not edit the workspace
  yourself until it returns.
- If a running full subagent needs a correction, clarification, or additional
  information, prefer `steer_subagent` over cancelling and restarting. Use the
  subagent session id shown by `inspect_delegation`.
- Cancellation is terminal. Use `cancel_delegation` when you intend to abandon
  the current subagent/delegation. Cancellation does not roll back workspace
  edits or remote-state side effects; inspect the transcript-only paths returned
  by cancellation before deciding follow-up work.
- Never mix RO and full work in one delegation.
- To run a known pattern (e.g. implement → review → test), `LoadSkill` the matching
  workflow skill and follow its delegation state machine, branching on the typed
  outcomes in `index.json`, with your own judgment (skip, re-run, escalate, stop).
"#;

fn rewrite_system_prompt(input: &str) -> (String, TextRewriteStats) {
    let mut output = input.to_string();
    let mut stats = TextRewriteStats::default();

    if let Some((start, end)) = subagent_delegation_section_bounds(&output) {
        output.replace_range(start..end, CURRENT_SUBAGENT_DELEGATION_SECTION);
        stats.record("system_prompt subagent delegation section", 1);
    }

    for (label, old, new) in [
        ("stage.full", "stage.full", "delegate_writing_task"),
        (
            "stage.start_full",
            "stage.start_full",
            "delegation.start_full",
        ),
        ("stage.fanout", "stage.fanout", "delegate_readonly_tasks"),
        (
            "stage.start_ro_fan",
            "stage.start_ro_fan",
            "delegation.start_readonly_fanout",
        ),
        (
            "stage.start_readonly_fanout",
            "stage.start_readonly_fanout",
            "delegation.start_readonly_fanout",
        ),
        ("stage.status", "stage.status", "inspect_delegation"),
        ("stage.cancel", "stage.cancel", "cancel_delegation"),
        ("stage.list", "stage.list", "delegation.list"),
        (
            "stage.read_handoff_file",
            "stage.read_handoff_file",
            "delegation.read_handoff_file",
        ),
        (
            "stage_start_full",
            "stage_start_full",
            "delegate_writing_task",
        ),
        ("stage_full", "stage_full", "delegate_writing_task"),
        (
            "stage_start_readonly_fanout",
            "stage_start_readonly_fanout",
            "delegate_readonly_tasks",
        ),
        (
            "stage_start_ro_fan",
            "stage_start_ro_fan",
            "delegate_readonly_tasks",
        ),
        ("stage_fanout", "stage_fanout", "delegate_readonly_tasks"),
        ("stage_status", "stage_status", "inspect_delegation"),
        ("stage_cancel", "stage_cancel", "cancel_delegation"),
        ("stage_id", "stage_id", "delegation_id"),
        ("stageId", "stageId", "delegationId"),
        ("stage-steer", "stage-steer:", "delegation-steer:"),
        (
            "through stage tool calls",
            "through stage tool calls",
            "through delegation tool calls",
        ),
        (
            "one stage per turn",
            "one stage per turn",
            "one delegation per turn",
        ),
        (
            "When a stage finishes",
            "When a stage finishes",
            "When a delegation finishes",
        ),
        (
            "stage state machine",
            "stage state machine",
            "delegation state machine",
        ),
        (
            "the stage handoff directory",
            "the stage handoff directory",
            "the delegation handoff directory",
        ),
        ("stage belongs", "stage belongs", "delegation belongs"),
        ("stage id", "stage id", "delegation id"),
        (
            "Never mix RO and full work in one stage.",
            "Never mix RO and full work in one stage.",
            "Never mix RO and full work in one delegation.",
        ),
        (
            "Cancel an in-flight stage",
            "Cancel an in-flight stage",
            "Cancel an in-flight delegation",
        ),
        ("Inspect a stage", "Inspect a stage", "Inspect a delegation"),
        (
            "stage tool/API names",
            "stage tool/API names",
            "delegation tool/API names",
        ),
        ("stages table", "stages table", "delegations table"),
        (
            "sessions.stage_id",
            "sessions.stage_id",
            "sessions.delegation_id",
        ),
    ] {
        let count = output.matches(old).count();
        if count > 0 {
            output = output.replace(old, new);
            stats.record(label, count);
        }
    }
    (output, stats)
}

fn subagent_delegation_section_bounds(input: &str) -> Option<(usize, usize)> {
    let start = input.find("## Subagent delegation")?;
    let after_heading = start + "## Subagent delegation".len();
    let end = input[after_heading..]
        .find("\n## ")
        .map(|offset| after_heading + offset)
        .unwrap_or(input.len());
    Some((start, end))
}

fn migrate_handoff_artifacts(
    root: &Path,
    config: &Config,
    stats: &mut MigrationStats,
) -> Result<()> {
    let handoff_root = if root.file_name().and_then(|name| name.to_str()) == Some(".pi-handoff") {
        root.to_path_buf()
    } else {
        root.join(".pi-handoff")
    };
    if !handoff_root.exists() {
        println!(
            "handoff root {} does not exist; skipping",
            handoff_root.display()
        );
        return Ok(());
    }

    visit_files(&handoff_root, &mut |path| {
        if path.file_name().and_then(|name| name.to_str()) == Some("index.json") {
            migrate_handoff_manifest(path, config, stats)?;
        }
        Ok(())
    })?;

    for entry in fs::read_dir(&handoff_root).with_context(|| {
        format!(
            "read immediate handoff directories under {}",
            handoff_root.display()
        )
    })? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("stage_") {
            stats.handoff_stage_dirs_preserved += 1;
        }
    }

    if stats.handoff_stage_dirs_preserved > 0 {
        println!(
            "found {} .pi-handoff/stage_* dirs; leaving names unchanged because IDs are preserved",
            stats.handoff_stage_dirs_preserved
        );
    }

    Ok(())
}

fn visit_files(dir: &Path, visit: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            visit_files(&path, visit)?;
        } else if file_type.is_file() {
            visit(&path)?;
        }
    }
    Ok(())
}

fn migrate_handoff_manifest(
    path: &Path,
    config: &Config,
    stats: &mut MigrationStats,
) -> Result<()> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut value: Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let rewrite_stats = rewrite_json_value_with_id_keys(&mut value, true)?;
    if !rewrite_stats.changed() {
        return Ok(());
    }
    stats.handoff_manifests += 1;
    stats.add_json(&rewrite_stats);
    if config.apply {
        let body = serde_json::to_string_pretty(&value)?;
        fs::write(path, format!("{body}\n"))
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(90_000);

    #[test]
    fn rewrites_tool_calls_and_args_recursively() {
        let mut value = json!({
            "type": "tool_call_started",
            "tool_call": {
                "tool_name": "stage_status",
                "args_json": "{\"stage_id\":\"stage_123\"}"
            },
            "result": {
                "tool_name": "stage.cancel",
                "output": "{\"stage_id\":\"stage_123\",\"ok\":true}"
            }
        });

        let stats = rewrite_json_value(&mut value).unwrap();

        assert!(stats.changed());
        assert_eq!(value["tool_call"]["tool_name"], "inspect_delegation");
        assert_eq!(
            value["tool_call"]["args_json"],
            "{\"delegation_id\":\"stage_123\"}"
        );
        assert_eq!(value["result"]["tool_name"], "cancel_delegation");
        assert_eq!(
            value["result"]["output"],
            "{\"delegation_id\":\"stage_123\",\"ok\":true}"
        );
    }

    #[test]
    fn rewrites_provider_replay_raw_json_strings() {
        let mut value = json!([{
            "provider": "openai",
            "raw_json": "{\"type\":\"function_call\",\"name\":\"stage_cancel\",\"arguments\":\"{\\\"stage_id\\\":\\\"stage_abc\\\"}\"}"
        }]);

        rewrite_json_value(&mut value).unwrap();

        let raw_json = value[0]["raw_json"].as_str().unwrap();
        let raw: Value = serde_json::from_str(raw_json).unwrap();
        assert_eq!(raw["name"], "cancel_delegation");
        assert_eq!(raw["arguments"], "{\"delegation_id\":\"stage_abc\"}");
    }

    #[test]
    fn distinguishes_model_tool_names_from_rpc_methods() {
        let mut value = json!({
            "tool_name": "stage.cancel",
            "method": "stage.cancel",
            "payload": {
                "type": "stage.status",
                "stage_id": "stage_abc"
            }
        });

        rewrite_json_value(&mut value).unwrap();

        assert_eq!(value["tool_name"], "cancel_delegation");
        assert_eq!(value["method"], "delegation.cancel");
        assert_eq!(value["payload"]["type"], "delegation.status");
        assert_eq!(value["payload"]["delegation_id"], "stage_abc");
    }

    #[test]
    fn embedded_json_strings_preserve_rpc_context() {
        let mut value = json!({
            "provider": "openai",
            "raw_json": "{\"method\":\"stage.status\",\"params\":{\"stage_id\":\"stage_abc\"}}"
        });

        rewrite_json_value(&mut value).unwrap();

        let raw: Value = serde_json::from_str(value["raw_json"].as_str().unwrap()).unwrap();
        assert_eq!(raw["method"], "delegation.status");
        assert_eq!(raw["params"]["delegation_id"], "stage_abc");
    }

    #[test]
    fn preserves_unrelated_user_text_and_json_keys() {
        let mut value = json!({
            "type": "user_message",
            "content": [
                {
                    "type": "text",
                    "text": "Stage lighting notes: keep stage and screen aligned."
                }
            ],
            "stage": "prod",
            "stages": ["dev", "prod"],
            "raw_json": "{\"stage_id\":\"not-an-orchestration-payload\"}",
            "note": "handoff for stage_abc"
        });
        let original = value.clone();

        let stats = rewrite_json_value(&mut value).unwrap();

        assert!(!stats.changed());
        assert_eq!(value, original);
    }

    #[test]
    fn preserves_stage_prefixed_ids_as_values_in_structured_payloads() {
        let mut value = json!({
            "method": "stage.status",
            "params": {
                "stage_id": "stage_abc"
            },
            "text": "handoff for stage_abc"
        });

        rewrite_json_value(&mut value).unwrap();

        assert_eq!(value["method"], "delegation.status");
        assert_eq!(value["params"]["delegation_id"], "stage_abc");
        assert_eq!(value["text"], "handoff for stage_abc");
    }

    #[test]
    fn rewrites_old_prompt_vocabulary() {
        let (rewritten, stats) = rewrite_system_prompt(
            "Delegate work through stage tool calls. Launch at most one stage per turn. Inspect a stage with stage_status and stage_id.",
        );

        assert!(stats.changed());
        assert!(rewritten.contains("through delegation tool calls"));
        assert!(rewritten.contains("one delegation per turn"));
        assert!(rewritten.contains("inspect_delegation"));
        assert!(rewritten.contains("delegation_id"));
    }

    #[test]
    fn rewrites_realistic_old_subagent_delegation_prompt_section() {
        let old_prompt = r#"# Instructions

## Subagent delegation

Delegate work to subagents through stage tool calls. Do not use the Python REPL
to orchestrate subagents.

Rules:

- Launch at most one stage per turn, then end your turn.
- When a stage finishes you receive a short message pointing at a handoff
  directory.
- Never mix RO and full work in one stage.
- Call stage.full, stage.fanout, stage.status, or stage.cancel.

## Skills

Use skills when relevant.
"#;

        let (rewritten, stats) = rewrite_system_prompt(old_prompt);

        assert!(stats.changed());
        assert!(rewritten.contains("through delegation tool calls"));
        assert!(rewritten.contains("Never mix RO and full work in one delegation."));
        assert!(rewritten.contains("delegate_writing_task"));
        assert!(rewritten.contains("delegate_readonly_tasks"));
        assert!(rewritten.contains("inspect_delegation"));
        assert!(rewritten.contains("cancel_delegation"));
        assert!(!rewritten.contains("through stage tool calls"));
        assert!(!rewritten.contains("one stage per turn"));
        assert!(!rewritten.contains("Never mix RO and full work in one stage."));
        assert!(!rewritten.contains("stage.full"));
        assert!(rewritten.contains("## Skills"));
    }

    #[test]
    fn parses_args_defaults_to_dry_run() {
        let config = Config::parse_from(["--database-url", "postgres://example/db"]).unwrap();

        assert_eq!(
            config.database_url.as_deref(),
            Some("postgres://example/db")
        );
        assert!(!config.apply);
        assert!(!config.no_backups);
    }

    #[test]
    fn rejects_apply_and_dry_run_together() {
        assert!(Config::parse_from(["--apply", "--dry-run"]).is_err());
    }

    #[test]
    fn rewrite_token_maps_all_old_surfaces() {
        assert_eq!(
            rewrite_model_tool_token("stage.full"),
            "delegate_writing_task"
        );
        assert_eq!(
            rewrite_model_tool_token("stage.start_full"),
            "delegate_writing_task"
        );
        assert_eq!(rewrite_rpc_token("stage.full"), "delegation.start_full");
        assert_eq!(
            rewrite_rpc_token("stage.start_readonly_fanout"),
            "delegation.start_readonly_fanout"
        );
        assert_eq!(rewrite_token("stage_status"), "inspect_delegation");
        assert_eq!(rewrite_token("stage_cancel"), "cancel_delegation");
        assert_eq!(
            rewrite_token("stage-steer:stage_1:attempt"),
            "delegation-steer:stage_1:attempt"
        );
    }

    #[test]
    fn object_key_null_merge_preserves_non_null_old_value() {
        let mut value = json!({
            "tool_name": "stage.status",
            "stage_id": "stage_abc",
            "delegation_id": null
        });

        let stats = rewrite_json_value(&mut value).unwrap();

        assert!(stats.changed());
        assert!(value.get("stage_id").is_none());
        assert_eq!(value["delegation_id"], "stage_abc");
    }

    #[test]
    fn object_key_conflicting_non_null_values_abort() {
        let mut value = json!({
            "tool_name": "stage.status",
            "stage_id": "old",
            "delegation_id": "new"
        });

        let error = rewrite_json_value(&mut value).unwrap_err();

        assert!(format!("{error:#}").contains("conflicting JSON keys"));
    }

    #[test]
    fn rewrites_generated_completion_steer_text_only_when_scoped_by_queue_row() {
        let mut value = json!({
            "content": [
                {
                    "type": "text",
                    "text": "Stage stage_7 (reviewer fan-out) finished: 2 ok, 0 failed. Read /tmp/.pi-handoff/stage_7/index.json."
                }
            ]
        });

        assert_eq!(rewrite_generated_completion_steer_value(&mut value), 1);
        assert_eq!(
            value["content"][0]["text"],
            "Delegation stage_7 (reviewer fan-out) finished: 2 ok, 0 failed. Read /tmp/.pi-handoff/stage_7/index.json."
        );
    }

    #[test]
    fn handoff_manifest_rewrite_uses_delegation_key() {
        let mut value = json!({
            "stage_id": "stage_abc",
            "kind": "full",
            "status": "done"
        });

        rewrite_json_value_with_id_keys(&mut value, true).unwrap();

        assert!(value.get("stage_id").is_none());
        assert_eq!(value["delegation_id"], "stage_abc");
    }

    #[test]
    fn dry_run_handoff_manifest_counts_without_writing_or_renaming_dir() {
        let base = env::temp_dir().join(format!(
            "pi-relay-migrate-stage-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let dir = base.join(".pi-handoff").join("stage_abc");
        fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("index.json");
        fs::write(&manifest, r#"{"stage_id":"stage_abc","status":"done"}"#).unwrap();
        let final_message = dir.join("final_message.md");
        fs::write(
            &final_message,
            "Stage lighting notes: keep stage and screen aligned.\n",
        )
        .unwrap();

        let config = Config {
            database_url: None,
            apply: false,
            no_backups: false,
            handoff_root: Some(base.clone()),
        };
        let mut stats = MigrationStats::default();

        migrate_handoff_artifacts(&base, &config, &mut stats).unwrap();

        assert_eq!(stats.handoff_manifests, 1);
        assert_eq!(stats.handoff_stage_dirs_preserved, 1);
        assert_eq!(
            fs::read_to_string(&manifest).unwrap(),
            r#"{"stage_id":"stage_abc","status":"done"}"#
        );
        assert_eq!(
            fs::read_to_string(&final_message).unwrap(),
            "Stage lighting notes: keep stage and screen aligned.\n"
        );
        assert!(dir.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn apply_handoff_manifest_preserves_dir_name() {
        let base = env::temp_dir().join(format!(
            "pi-relay-migrate-stage-apply-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let dir = base.join(".pi-handoff").join("stage_abc");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("index.json"),
            r#"{"stage_id":"stage_abc","status":"done"}"#,
        )
        .unwrap();
        fs::write(
            dir.join("transcript.md"),
            "Stage lighting notes: keep stage and screen aligned.\n",
        )
        .unwrap();

        let config = Config {
            database_url: None,
            apply: true,
            no_backups: false,
            handoff_root: Some(base.clone()),
        };
        let mut stats = MigrationStats::default();

        migrate_handoff_artifacts(&base, &config, &mut stats).unwrap();

        assert!(dir.exists());
        assert!(!base.join(".pi-handoff").join("delegation_abc").exists());
        let rewritten = fs::read_to_string(dir.join("index.json")).unwrap();
        assert!(rewritten.contains("\"delegation_id\": \"stage_abc\""));
        assert_eq!(
            fs::read_to_string(dir.join("transcript.md")).unwrap(),
            "Stage lighting notes: keep stage and screen aligned.\n"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn postgres_fixture_migrates_old_schema_and_is_idempotent() {
        let Some((admin_url, database_url, database_name)) = create_test_database().await else {
            eprintln!("skipping postgres migration fixture; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };

        let pool = PgPool::connect(&database_url)
            .await
            .expect("connect test db");
        create_old_shape_fixture(&pool).await;

        let dry_run_config = Config {
            database_url: Some(database_url.clone()),
            apply: false,
            no_backups: true,
            handoff_root: None,
        };
        let mut dry_stats = MigrationStats::default();
        {
            let mut tx = pool.begin().await.expect("begin dry run");
            migrate_schema(&mut tx, &dry_run_config, &mut dry_stats)
                .await
                .expect("dry-run schema migration");
            migrate_persisted_json(&mut tx, &dry_run_config, &mut dry_stats)
                .await
                .expect("dry-run JSON migration");
            migrate_session_prompts(&mut tx, &dry_run_config, &mut dry_stats)
                .await
                .expect("dry-run prompt migration");
            tx.rollback().await.expect("rollback dry run");
        }
        assert!(dry_stats
            .schema_operations
            .iter()
            .any(|operation| operation.contains("rename stages")));
        assert_eq!(dry_stats.transcript_item_rows, 1);
        assert_eq!(dry_stats.transcript_provider_replay_rows, 1);
        assert_eq!(dry_stats.queued_client_input_id_rows, 1);
        assert_eq!(dry_stats.event_type_rows, 1);

        let apply_config = Config {
            database_url: Some(database_url.clone()),
            apply: true,
            no_backups: true,
            handoff_root: None,
        };
        let mut apply_stats = MigrationStats::default();
        {
            let mut tx = pool.begin().await.expect("begin apply");
            migrate_schema(&mut tx, &apply_config, &mut apply_stats)
                .await
                .expect("apply schema migration");
            migrate_persisted_json(&mut tx, &apply_config, &mut apply_stats)
                .await
                .expect("apply JSON migration");
            migrate_session_prompts(&mut tx, &apply_config, &mut apply_stats)
                .await
                .expect("apply prompt migration");
            tx.commit().await.expect("commit apply");
        }

        assert!(!table_exists_in_pool(&pool, "stages").await);
        assert!(table_exists_in_pool(&pool, "delegations").await);
        assert!(column_exists_in_pool(&pool, "sessions", "delegation_id").await);
        assert!(!column_exists_in_pool(&pool, "sessions", "stage_id").await);

        let delegation_id: String =
            sqlx::query_scalar("select delegation_id from sessions where id='child'")
                .fetch_one(&pool)
                .await
                .expect("delegation_id");
        assert_eq!(delegation_id, "stage_abc");

        let item: Value =
            sqlx::query_scalar("select item from transcript_entries where session_id='parent'")
                .fetch_one(&pool)
                .await
                .expect("transcript item");
        assert_eq!(item["items"][0]["tool_name"], "inspect_delegation");
        assert_eq!(
            item["items"][0]["args_json"],
            "{\"delegation_id\":\"stage_abc\"}"
        );

        let provider_replay: Value = sqlx::query_scalar(
            "select provider_replay from transcript_entries where session_id='parent'",
        )
        .fetch_one(&pool)
        .await
        .expect("provider replay");
        let raw: Value =
            serde_json::from_str(provider_replay[0]["raw_json"].as_str().unwrap()).unwrap();
        assert_eq!(raw["name"], "cancel_delegation");

        let client_input_id: String = sqlx::query_scalar(
            "select client_input_id from queued_inputs where id='input_completion'",
        )
        .fetch_one(&pool)
        .await
        .expect("client_input_id");
        assert_eq!(client_input_id, "delegation-steer:stage_abc:attempt_1");

        let queued_content: Value =
            sqlx::query_scalar("select content from queued_inputs where id='input_completion'")
                .fetch_one(&pool)
                .await
                .expect("queued content");
        assert_eq!(
            queued_content["content"][0]["text"],
            "Delegation stage_abc finished"
        );

        let event_type: String = sqlx::query_scalar("select type from events where id=1")
            .fetch_one(&pool)
            .await
            .expect("event type");
        assert_eq!(event_type, "delegation.status");

        let prompt: String =
            sqlx::query_scalar("select system_prompt from sessions where id='parent'")
                .fetch_one(&pool)
                .await
                .expect("system prompt");
        assert!(prompt.contains("delegation tool calls"));
        assert!(!prompt.contains("stage tool calls"));

        let mut rerun_stats = MigrationStats::default();
        {
            let mut tx = pool.begin().await.expect("begin rerun");
            migrate_schema(&mut tx, &apply_config, &mut rerun_stats)
                .await
                .expect("rerun schema migration");
            migrate_persisted_json(&mut tx, &apply_config, &mut rerun_stats)
                .await
                .expect("rerun JSON migration");
            migrate_session_prompts(&mut tx, &apply_config, &mut rerun_stats)
                .await
                .expect("rerun prompt migration");
            tx.commit().await.expect("commit rerun");
        }
        assert_eq!(rerun_stats.transcript_item_rows, 0);
        assert_eq!(rerun_stats.queued_client_input_id_rows, 0);
        assert_eq!(rerun_stats.event_type_rows, 0);
        assert!(rerun_stats.schema_operations.is_empty());

        pool.close().await;
        drop_test_database(&admin_url, &database_name).await;
    }

    #[tokio::test]
    async fn postgres_fixture_infers_pre_expected_readonly_fanout_child_count() {
        let Some((admin_url, database_url, database_name)) = create_test_database().await else {
            eprintln!("skipping postgres migration fixture; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };

        let pool = PgPool::connect(&database_url)
            .await
            .expect("connect test db");
        create_pre_expected_fanout_fixture(&pool, 2).await;

        let config = Config {
            database_url: Some(database_url.clone()),
            apply: true,
            no_backups: true,
            handoff_root: None,
        };
        {
            let mut tx = pool.begin().await.expect("begin apply");
            migrate_schema(&mut tx, &config, &mut MigrationStats::default())
                .await
                .expect("apply schema migration");
            tx.commit().await.expect("commit apply");
        }

        let expected_subagents: i32 = sqlx::query_scalar(
            "select expected_subagents from delegations where id='stage_fanout'",
        )
        .fetch_one(&pool)
        .await
        .expect("expected_subagents");
        assert_eq!(expected_subagents, 2);

        pool.close().await;
        drop_test_database(&admin_url, &database_name).await;
    }

    #[tokio::test]
    async fn postgres_fixture_fails_closed_for_running_pre_expected_fanout_with_no_children() {
        let Some((admin_url, database_url, database_name)) = create_test_database().await else {
            eprintln!("skipping postgres migration fixture; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };

        let pool = PgPool::connect(&database_url)
            .await
            .expect("connect test db");
        create_pre_expected_fanout_fixture(&pool, 0).await;

        let config = Config {
            database_url: Some(database_url.clone()),
            apply: false,
            no_backups: true,
            handoff_root: None,
        };
        let error = {
            let mut tx = pool.begin().await.expect("begin dry-run");
            let error = migrate_schema(&mut tx, &config, &mut MigrationStats::default())
                .await
                .expect_err("ambiguous fanout should fail");
            tx.rollback().await.expect("rollback dry run");
            error
        };
        assert!(format!("{error:#}").contains("cannot infer expected_subagents"));

        pool.close().await;
        drop_test_database(&admin_url, &database_name).await;
    }

    async fn create_old_shape_fixture(pool: &PgPool) {
        sqlx::raw_sql(
            r#"
            create table sessions (
                id text primary key,
                project_id uuid null,
                outer_cwd text not null,
                workspaces jsonb not null default '[]'::jsonb,
                created_at timestamptz not null default now(),
                updated_at timestamptz not null default now(),
                active_leaf_id text null,
                system_prompt text not null,
                provider_config jsonb not null,
                metadata jsonb not null default '{}'::jsonb,
                parent_session_id text null references sessions(id) on delete set null,
                last_user_message_timestamp_ms bigint null,
                session_revision bigint not null default 0,
                queue_revision bigint not null default 0,
                transcript_revision bigint not null default 0,
                subagent_type text null
            );
            create table stages (
                id text primary key,
                parent_session_id text not null references sessions(id) on delete cascade,
                workflow text null,
                label text null,
                kind text not null,
                status text not null,
                attempt_id text not null,
                created_at timestamptz not null default now(),
                updated_at timestamptz not null default now(),
                expected_subagents integer not null default 1
            );
            create index stages_parent_created_idx on stages(parent_session_id, created_at, id);
            alter table sessions add column stage_id text null references stages(id);
            create table transcript_entries (
                session_id text not null references sessions(id) on delete cascade,
                id text not null,
                parent_id text null,
                timestamp_ms bigint not null,
                item jsonb not null,
                provider_replay jsonb not null default '[]'::jsonb,
                turn_id bigint null,
                sequence bigserial not null,
                primary key (session_id, id)
            );
            create table queued_inputs (
                id text primary key,
                session_id text not null references sessions(id) on delete cascade,
                priority text not null,
                content jsonb not null,
                origin jsonb null,
                status text not null,
                created_at timestamptz not null default now(),
                updated_at timestamptz not null default now(),
                follow_up_position integer null,
                client_input_id text null
            );
            create unique index queued_inputs_client_input_idx
                on queued_inputs(session_id, client_input_id)
                where client_input_id is not null;
            create table actions (
                id text primary key,
                session_id text not null references sessions(id) on delete cascade,
                turn_id bigint null,
                action_id bigint not null,
                attempt_id text not null,
                kind text not null,
                status text not null,
                payload jsonb not null,
                result jsonb null,
                created_at timestamptz not null default now(),
                updated_at timestamptz not null default now()
            );
            create table events (
                id bigserial primary key,
                session_id text not null references sessions(id) on delete cascade,
                type text not null,
                payload jsonb not null,
                created_at timestamptz not null default now()
            );
            insert into sessions (id, outer_cwd, system_prompt, provider_config, metadata)
            values
                ('parent', '/tmp', 'Delegate work through stage tool calls. Launch at most one stage per turn.', '{"kind":"openai","model":"gpt-5"}', '{"stage_id":"stage_abc"}'),
                ('child', '/tmp', '', '{"kind":"openai","model":"gpt-5"}', '{}');
            insert into stages (id, parent_session_id, kind, status, attempt_id, expected_subagents)
            values ('stage_abc', 'parent', 'full', 'running', 'attempt_1', 1);
            update sessions set parent_session_id='parent', subagent_type='full', stage_id='stage_abc'
            where id='child';
            insert into transcript_entries (session_id, id, timestamp_ms, item, provider_replay)
            values (
                'parent',
                'entry_assistant',
                1,
                '{"type":"assistant_message","items":[{"type":"tool_call","id":"call_1","tool_name":"stage_status","args_json":"{\"stage_id\":\"stage_abc\"}"}]}',
                jsonb_build_array(jsonb_build_object(
                    'provider', 'openai',
                    'raw_json', '{"type":"function_call","name":"stage_cancel","arguments":"{\"stage_id\":\"stage_abc\"}"}'
                ))
            );
            insert into actions (id, session_id, action_id, attempt_id, kind, status, payload, result)
            values (
                'action_1',
                'parent',
                1,
                'attempt_action',
                'tool',
                'pending',
                '{"id":"call_1","tool_name":"stage_status","args_json":"{\"stage_id\":\"stage_abc\"}"}',
                null
            );
            insert into queued_inputs (id, session_id, priority, content, status, client_input_id, origin)
            values (
                'input_completion',
                'parent',
                'steer',
                '{"content":[{"type":"text","text":"Stage stage_abc finished"}]}',
                'queued',
                'stage-steer:stage_abc:attempt_1',
                '{"client_input_id":"stage-steer:stage_abc:attempt_1"}'
            );
            insert into events (session_id, type, payload)
            values ('parent', 'stage.status', '{"stage_id":"stage_abc","method":"stage.status"}');
            "#,
        )
        .execute(pool)
        .await
        .expect("create old-shape fixture");
    }

    async fn create_pre_expected_fanout_fixture(pool: &PgPool, child_count: usize) {
        sqlx::raw_sql(
            r#"
            create table sessions (
                id text primary key,
                outer_cwd text not null,
                workspaces jsonb not null default '[]'::jsonb,
                created_at timestamptz not null default now(),
                updated_at timestamptz not null default now(),
                active_leaf_id text null,
                system_prompt text not null,
                provider_config jsonb not null,
                metadata jsonb not null default '{}'::jsonb,
                parent_session_id text null references sessions(id) on delete set null,
                session_revision bigint not null default 0,
                queue_revision bigint not null default 0,
                transcript_revision bigint not null default 0,
                subagent_type text null
            );
            create table stages (
                id text primary key,
                parent_session_id text not null references sessions(id) on delete cascade,
                workflow text null,
                label text null,
                kind text not null,
                status text not null,
                attempt_id text not null,
                created_at timestamptz not null default now(),
                updated_at timestamptz not null default now()
            );
            create index stages_parent_created_idx on stages(parent_session_id, created_at, id);
            alter table sessions add column stage_id text null references stages(id);
            insert into sessions (id, outer_cwd, system_prompt, provider_config, metadata)
            values ('parent', '/tmp', '', '{"kind":"openai","model":"gpt-5"}', '{}');
            insert into stages (id, parent_session_id, kind, status, attempt_id)
            values ('stage_fanout', 'parent', 'readonly_fanout', 'running', 'attempt_1');
            "#,
        )
        .execute(pool)
        .await
        .expect("create pre-expected fanout fixture");

        for index in 0..child_count {
            let child_id = format!("child_{index}");
            sqlx::query(
                r#"
                insert into sessions (
                    id, outer_cwd, system_prompt, provider_config, metadata,
                    parent_session_id, subagent_type, stage_id
                )
                values ($1, '/tmp', '', '{"kind":"openai","model":"gpt-5"}', '{}',
                    'parent', 'read_only', 'stage_fanout')
                "#,
            )
            .bind(child_id)
            .execute(pool)
            .await
            .expect("insert fanout child");
        }
    }

    async fn create_test_database() -> Option<(String, String, String)> {
        let admin_url = env::var("PI_RELAY_TEST_DATABASE_URL").ok()?;
        let name = format!(
            "pi_relay_stage_migration_test_{}_{}",
            std::process::id(),
            TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = PgPool::connect(&admin_url)
            .await
            .expect("connect to PI_RELAY_TEST_DATABASE_URL");
        sqlx::query(&format!(r#"create database "{name}""#))
            .execute(&admin)
            .await
            .expect("create isolated test database");
        admin.close().await;
        let database_url = database_url_with_name(&admin_url, &name);
        Some((admin_url, database_url, name))
    }

    async fn drop_test_database(admin_url: &str, name: &str) {
        if let Ok(admin) = PgPool::connect(admin_url).await {
            let _ = sqlx::query(&format!(r#"drop database if exists "{name}""#))
                .execute(&admin)
                .await;
            admin.close().await;
        }
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

    async fn table_exists_in_pool(pool: &PgPool, table: &str) -> bool {
        sqlx::query_scalar(
            r#"
            select exists(
                select 1
                from information_schema.tables
                where table_schema = current_schema()
                  and table_name = $1
            )
            "#,
        )
        .bind(table)
        .fetch_one(pool)
        .await
        .expect("table exists query")
    }

    async fn column_exists_in_pool(pool: &PgPool, table: &str, column: &str) -> bool {
        sqlx::query_scalar(
            r#"
            select exists(
                select 1
                from information_schema.columns
                where table_schema = current_schema()
                  and table_name = $1
                  and column_name = $2
            )
            "#,
        )
        .bind(table)
        .bind(column)
        .fetch_one(pool)
        .await
        .expect("column exists query")
    }
}
