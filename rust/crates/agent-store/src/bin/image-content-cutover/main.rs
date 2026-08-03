use std::{process::ExitCode, str::FromStr};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use sqlx::{
    postgres::{PgConnectOptions, PgConnection},
    Connection, PgPool, Postgres, Row, Transaction,
};
use url::Url;

mod image_cutover;

use image_cutover::{
    cutover_event, cutover_queued_input, cutover_tool_action_result, cutover_transcript_item,
    CutoverValue, PendingArtifact,
};

const TEST_DATABASE_PREFIX: &str = "pi_relay_cutover_test_";

#[derive(Default, Clone, Copy)]
struct Report {
    convertible: u64,
    canonical_valid: u64,
    opaque: u64,
    invalid: u64,
}

impl Report {
    fn record(&mut self, result: &Result<CutoverValue, String>, context: &str) {
        match result {
            Ok(value) if value.changed => self.convertible += 1,
            Ok(value) if value.applicable => self.canonical_valid += 1,
            Ok(_) => self.opaque += 1,
            Err(error) => {
                self.invalid += 1;
                eprintln!("{context}: {error}");
            }
        }
    }

    fn add(&mut self, other: Self) {
        self.convertible += other.convertible;
        self.canonical_valid += other.canonical_valid;
        self.opaque += other.opaque;
        self.invalid += other.invalid;
    }

    fn print(&self, scope: &str) {
        println!(
            "{scope} convertible={} canonical_valid={} opaque={} invalid={}",
            self.convertible, self.canonical_valid, self.opaque, self.invalid
        );
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(2),
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<u64> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or_else(usage)?;
    match mode.as_str() {
        "check" | "report" | "apply" => {
            if args.next().as_deref() != Some("--expected-database") {
                return Err(usage());
            }
            let expected_database = args.next().ok_or_else(usage)?;
            let database_url = args.next().ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            run_cutover(&database_url, &expected_database, mode == "apply").await
        }
        "test-create" => {
            let admin_url = args.next().ok_or_else(usage)?;
            let database_name = args.next().ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            create_test_database(&admin_url, &database_name).await?;
            println!("{}", database_url_for(&admin_url, &database_name)?);
            Ok(0)
        }
        "test-init" => {
            let database_url = args.next().ok_or_else(usage)?;
            let database_name = args.next().ok_or_else(usage)?;
            let sentinel = args.next().ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            initialize_test_database(&database_url, &database_name, &sentinel).await?;
            Ok(0)
        }
        "test-drop" => {
            let admin_url = args.next().ok_or_else(usage)?;
            let database_name = args.next().ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            drop_test_database(&admin_url, &database_name).await?;
            Ok(0)
        }
        _ => Err(usage()),
    }
}

async fn run_cutover(database_url: &str, expected_database: &str, apply: bool) -> Result<u64> {
    anyhow::ensure!(
        !expected_database.is_empty(),
        "expected database name must not be empty"
    );
    let options = PgConnectOptions::from_str(database_url).context("invalid database URL")?;
    let mut connection = PgConnection::connect_with(&options).await?;
    let current: String = sqlx::query_scalar("select current_database()")
        .fetch_one(&mut connection)
        .await?;
    anyhow::ensure!(
        current == expected_database,
        "refusing cutover: current_database() is {current}, expected {expected_database}"
    );
    let mut tx = connection.begin().await?;
    create_artifact_table(&mut tx).await?;
    sqlx::query(
        "lock table public.image_artifacts, public.transcript_entries, public.actions, \
         public.queued_inputs, public.events in share row exclusive mode",
    )
    .execute(&mut *tx)
    .await?;
    let mut total = Report::default();
    let mut transcript = Report::default();
    scan_transcript(&mut tx, apply, &mut transcript).await?;
    transcript.print("transcript_entries");
    total.add(transcript);
    let mut actions = Report::default();
    scan_actions(&mut tx, apply, &mut actions).await?;
    actions.print("actions");
    total.add(actions);
    let mut queue = Report::default();
    scan_queue(&mut tx, apply, &mut queue).await?;
    queue.print("queued_inputs");
    total.add(queue);
    let mut events = Report::default();
    scan_events(&mut tx, apply, &mut events).await?;
    events.print("events");
    total.add(events);
    total.print("total");
    if total.invalid > 0 {
        tx.rollback().await?;
        return Ok(total.invalid);
    }
    if apply {
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    Ok(0)
}

async fn create_test_database(admin_url: &str, database_name: &str) -> Result<()> {
    validate_test_database_name(database_name)?;
    let options = PgConnectOptions::from_str(admin_url).context("invalid admin database URL")?;
    let admin_database = options.get_database().unwrap_or_default();
    anyhow::ensure!(
        admin_database != database_name,
        "admin URL must not name the generated child database"
    );
    let pool = PgPool::connect_with(options).await?;
    let exists: bool =
        sqlx::query_scalar("select exists(select 1 from pg_database where datname=$1)")
            .bind(database_name)
            .fetch_one(&pool)
            .await?;
    anyhow::ensure!(!exists, "generated test database already exists");
    sqlx::query(&format!("create database \"{database_name}\""))
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

async fn initialize_test_database(
    database_url: &str,
    database_name: &str,
    sentinel: &str,
) -> Result<()> {
    validate_test_database_name(database_name)?;
    anyhow::ensure!(!sentinel.is_empty(), "test sentinel must not be empty");
    let pool = PgPool::connect(database_url).await?;
    let current: String = sqlx::query_scalar("select current_database()")
        .fetch_one(&pool)
        .await?;
    anyhow::ensure!(
        current == database_name,
        "refusing test initialization: current_database() is {current}, expected {database_name}"
    );
    pool.close().await;
    let store = agent_store::PostgresAgentStore::connect(database_url).await?;
    store.migrate().await?;
    let pool = PgPool::connect(database_url).await?;
    sqlx::query(
        "create table public.pi_relay_cutover_test_sentinel (
           token text primary key
         )",
    )
    .execute(&pool)
    .await?;
    sqlx::query("insert into public.pi_relay_cutover_test_sentinel(token) values ($1)")
        .bind(sentinel)
        .execute(&pool)
        .await?;
    pool.close().await;
    store.close().await;
    Ok(())
}

async fn drop_test_database(admin_url: &str, database_name: &str) -> Result<()> {
    validate_test_database_name(database_name)?;
    let options = PgConnectOptions::from_str(admin_url).context("invalid admin database URL")?;
    anyhow::ensure!(
        options.get_database() != Some(database_name),
        "admin URL must not name the generated child database"
    );
    let pool = PgPool::connect_with(options).await?;
    sqlx::query(
        "select pg_terminate_backend(pid)
         from pg_stat_activity
         where datname=$1 and pid <> pg_backend_pid()",
    )
    .bind(database_name)
    .execute(&pool)
    .await?;
    sqlx::query(&format!("drop database if exists \"{database_name}\""))
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

fn database_url_for(admin_url: &str, database_name: &str) -> Result<String> {
    validate_test_database_name(database_name)?;
    let mut url = Url::parse(admin_url).context("invalid admin database URL")?;
    anyhow::ensure!(
        matches!(url.scheme(), "postgres" | "postgresql"),
        "admin URL must use postgres or postgresql"
    );
    url.set_path(&format!("/{database_name}"));
    Ok(url.into())
}

fn validate_test_database_name(database_name: &str) -> Result<()> {
    anyhow::ensure!(
        database_name.starts_with(TEST_DATABASE_PREFIX)
            && database_name.len() > TEST_DATABASE_PREFIX.len()
            && database_name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
        "test database name must use prefix {TEST_DATABASE_PREFIX} and lowercase ASCII/digits/underscore only"
    );
    anyhow::ensure!(
        !matches!(
            database_name,
            "postgres" | "template0" | "template1" | "pi_relay"
        ),
        "refusing default or deployment database name"
    );
    Ok(())
}

async fn scan_transcript(
    tx: &mut Transaction<'_, Postgres>,
    apply: bool,
    report: &mut Report,
) -> Result<()> {
    let mut cursor: Option<(String, String)> = None;
    loop {
        let row = sqlx::query(
            r#"
            select session_id, id, item
            from public.transcript_entries
            where $1::text is null or (session_id, id) > ($1, $2)
            order by session_id, id
            limit 1
            "#,
        )
        .bind(cursor.as_ref().map(|cursor| cursor.0.as_str()))
        .bind(cursor.as_ref().map(|cursor| cursor.1.as_str()))
        .fetch_optional(&mut **tx)
        .await?;
        let Some(row) = row else { break };
        let session_id: String = row.get("session_id");
        let id: String = row.get("id");
        let mut value: Value = row.get("item");
        let context = format!("public.transcript_entries[{session_id}/{id}].item");
        let mut result = cutover_transcript_item(&mut value);
        admit_result(tx, &mut result).await;
        report.record(&result, &context);
        match result {
            Ok(result) if apply && result.changed => {
                sqlx::query(
                    "update public.transcript_entries set item=$3 \
                     where session_id=$1 and id=$2",
                )
                .bind(&session_id)
                .bind(&id)
                .bind(value)
                .execute(&mut **tx)
                .await?;
            }
            Err(error) if apply => return Err(anyhow!("{context}: {error}")),
            _ => {}
        }
        cursor = Some((session_id, id));
    }
    Ok(())
}

async fn scan_actions(
    tx: &mut Transaction<'_, Postgres>,
    apply: bool,
    report: &mut Report,
) -> Result<()> {
    let mut cursor: Option<String> = None;
    loop {
        let row = sqlx::query(
            r#"
            select id, status, result
            from public.actions
            where kind='tool' and result is not null
              and ($1::text is null or id > $1)
            order by id
            limit 1
            "#,
        )
        .bind(cursor.as_deref())
        .fetch_optional(&mut **tx)
        .await?;
        let Some(row) = row else { break };
        let id: String = row.get("id");
        let status: String = row.get("status");
        let mut value: Value = row.get("result");
        let context = format!("public.actions[{id}].result");
        let mut result = cutover_tool_action_result(&status, &mut value);
        admit_result(tx, &mut result).await;
        report.record(&result, &context);
        match result {
            Ok(result) if apply && result.changed => {
                sqlx::query("update public.actions set result=$2 where id=$1")
                    .bind(&id)
                    .bind(value)
                    .execute(&mut **tx)
                    .await?;
            }
            Err(error) if apply => return Err(anyhow!("{context}: {error}")),
            _ => {}
        }
        cursor = Some(id);
    }
    Ok(())
}

async fn scan_queue(
    tx: &mut Transaction<'_, Postgres>,
    apply: bool,
    report: &mut Report,
) -> Result<()> {
    let mut cursor: Option<String> = None;
    loop {
        let row = sqlx::query(
            r#"
            select id, client_input_id, content
            from public.queued_inputs
            where $1::text is null or id > $1
            order by id
            limit 1
            "#,
        )
        .bind(cursor.as_deref())
        .fetch_optional(&mut **tx)
        .await?;
        let Some(row) = row else { break };
        let id: String = row.get("id");
        let mut value: Value = row.get("content");
        let context = format!("public.queued_inputs[{id}].content");
        let mut result = cutover_queued_input(&mut value);
        admit_result(tx, &mut result).await;
        report.record(&result, &context);
        match result {
            Ok(result) if apply && result.changed => {
                sqlx::query("update public.queued_inputs set content=$2 where id=$1")
                    .bind(&id)
                    .bind(value)
                    .execute(&mut **tx)
                    .await?;
            }
            Err(error) if apply => return Err(anyhow!("{context}: {error}")),
            _ => {}
        }
        cursor = Some(id);
    }
    Ok(())
}

async fn scan_events(
    tx: &mut Transaction<'_, Postgres>,
    apply: bool,
    report: &mut Report,
) -> Result<()> {
    let mut cursor = 0_i64;
    loop {
        let row = sqlx::query(
            r#"
            select id, type, payload
            from public.events
            where id > $1
            order by id
            limit 1
            "#,
        )
        .bind(cursor)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(row) = row else { break };
        let id: i64 = row.get("id");
        let event_type: String = row.get("type");
        let mut value: Value = row.get("payload");
        let context = format!("public.events[{id}].payload");
        let mut result = cutover_event(&event_type, &mut value);
        admit_result(tx, &mut result).await;
        report.record(&result, &context);
        match result {
            Ok(result) if apply && result.changed => {
                sqlx::query("update public.events set payload=$2 where id=$1")
                    .bind(id)
                    .bind(value)
                    .execute(&mut **tx)
                    .await?;
            }
            Err(error) if apply => return Err(anyhow!("{context}: {error}")),
            _ => {}
        }
        cursor = id;
    }
    Ok(())
}

async fn create_artifact_table(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::raw_sql(
        r#"
        create table if not exists public.image_artifacts (
            id text primary key
                constraint image_artifacts_id_format
                check (id ~ '^sha256:[0-9a-f]{64}$'),
            mime_type text not null
                constraint image_artifacts_mime_type
                check (mime_type in ('image/png', 'image/jpeg', 'image/gif', 'image/webp')),
            data bytea not null,
            byte_length integer generated always as (octet_length(data)) stored,
            created_at timestamptz not null default now(),
            constraint image_artifacts_size check (byte_length between 1 and 5242880)
        );
        create or replace function public.reject_image_artifact_mutation()
        returns trigger language plpgsql as $$
        begin
            raise exception 'image artifacts are immutable';
        end
        $$;
        drop trigger if exists image_artifacts_immutable on public.image_artifacts;
        create trigger image_artifacts_immutable
        before update or delete on public.image_artifacts
        for each row execute function public.reject_image_artifact_mutation();
        "#,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn admit_result(
    tx: &mut Transaction<'_, Postgres>,
    result: &mut Result<CutoverValue, String>,
) {
    let Ok(value) = result else {
        return;
    };
    if let Err(error) = admit_artifacts(tx, value).await {
        *result = Err(format!("{error:#}"));
    }
}

async fn admit_artifacts(tx: &mut Transaction<'_, Postgres>, value: &CutoverValue) -> Result<()> {
    for artifact in value.artifacts.values() {
        insert_and_verify_artifact(tx, artifact).await?;
    }
    for ids in &value.content_image_sets {
        let mut total = 0usize;
        for id in ids {
            let row = sqlx::query(
                "select mime_type, data, byte_length from public.image_artifacts where id=$1",
            )
            .bind(id.as_str())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| anyhow!("missing image artifact {id}"))?;
            let mime_type: String = row.get("mime_type");
            let data: Vec<u8> = row.get("data");
            let byte_length: i32 = row.get("byte_length");
            verify_artifact(id, &mime_type, &data, byte_length)?;
            total = total.saturating_add(data.len());
        }
        anyhow::ensure!(
            total <= agent_vocab::MAX_AGGREGATE_IMAGE_BYTES,
            "aggregate image bytes exceed {}",
            agent_vocab::MAX_AGGREGATE_IMAGE_BYTES
        );
    }
    Ok(())
}

async fn insert_and_verify_artifact(
    tx: &mut Transaction<'_, Postgres>,
    artifact: &PendingArtifact,
) -> Result<()> {
    sqlx::query(
        "insert into public.image_artifacts (id, mime_type, data) values ($1,$2,$3) \
         on conflict (id) do nothing",
    )
    .bind(artifact.artifact_id.as_str())
    .bind(&artifact.mime_type)
    .bind(&artifact.data)
    .execute(&mut **tx)
    .await?;
    let row =
        sqlx::query("select mime_type, data, byte_length from public.image_artifacts where id=$1")
            .bind(artifact.artifact_id.as_str())
            .fetch_one(&mut **tx)
            .await?;
    let mime_type: String = row.get("mime_type");
    let data: Vec<u8> = row.get("data");
    let byte_length: i32 = row.get("byte_length");
    verify_artifact(&artifact.artifact_id, &mime_type, &data, byte_length)?;
    anyhow::ensure!(
        mime_type == artifact.mime_type && data == artifact.data,
        "hash collision for image artifact {}",
        artifact.artifact_id
    );
    Ok(())
}

fn verify_artifact(
    id: &agent_vocab::ImageArtifactId,
    mime_type: &str,
    data: &[u8],
    byte_length: i32,
) -> Result<()> {
    anyhow::ensure!(
        usize::try_from(byte_length).ok() == Some(data.len()),
        "corrupt image artifact {id}: byte length mismatch"
    );
    anyhow::ensure!(
        agent_vocab::sniff_mime(data) == Some(mime_type),
        "corrupt image artifact {id}: MIME mismatch"
    );
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    anyhow::ensure!(
        id.as_str() == format!("sha256:{digest}"),
        "corrupt image artifact {id}: hash mismatch"
    );
    Ok(())
}

fn usage() -> anyhow::Error {
    anyhow!(
        "usage: image-content-cutover <check|report|apply> --expected-database DATABASE_NAME DATABASE_URL\n\
         image-content-cutover test-create ADMIN_URL DATABASE_NAME\n\
         image-content-cutover test-init DATABASE_URL DATABASE_NAME SENTINEL\n\
         image-content-cutover test-drop ADMIN_URL DATABASE_NAME"
    )
}
