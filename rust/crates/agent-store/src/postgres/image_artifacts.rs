use std::collections::{BTreeMap, BTreeSet};

use agent_vocab::{
    sniff_mime, validate_durable_content, validate_inline_image, ContentBlock, ImageArtifactId,
    InlineContentBlock, InlineToolResultMessage, ToolResultMessage, TranscriptItem,
    MAX_AGGREGATE_IMAGE_BYTES,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use thiserror::Error;

use super::PostgresAgentStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageArtifactMetadata {
    pub artifact_id: ImageArtifactId,
    pub mime_type: String,
    pub byte_length: usize,
}

pub(super) async fn require_transcript_item_refs_tx(
    tx: &mut Transaction<'_, Postgres>,
    item: &TranscriptItem,
) -> Result<(), ImageArtifactError> {
    match item {
        TranscriptItem::UserMessage(message) => require_content_refs_tx(tx, &message.content).await,
        TranscriptItem::ToolResult(result) => require_content_refs_tx(tx, &result.content).await,
        _ => Ok(()),
    }
}

pub(super) async fn require_tool_result_refs_tx(
    tx: &mut Transaction<'_, Postgres>,
    result: &ToolResultMessage,
) -> Result<(), ImageArtifactError> {
    require_content_refs_tx(tx, &result.content).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageArtifact {
    pub metadata: ImageArtifactMetadata,
    pub data: Vec<u8>,
}

impl ImageArtifact {
    pub fn base64(&self) -> String {
        STANDARD.encode(&self.data)
    }
}

#[derive(Debug, Error)]
pub enum ImageArtifactError {
    #[error("invalid image: {0}")]
    InvalidImage(String),
    #[error("invalid image artifact reference: {0}")]
    InvalidReference(String),
    #[error("missing image artifact {artifact_id}")]
    Missing { artifact_id: ImageArtifactId },
    #[error("corrupt image artifact {artifact_id}: {reason}")]
    Corrupt {
        artifact_id: ImageArtifactId,
        reason: String,
    },
    #[error("image artifact hash collision for {artifact_id}")]
    Collision { artifact_id: ImageArtifactId },
    #[error("aggregate image bytes exceed {MAX_AGGREGATE_IMAGE_BYTES}")]
    AggregateTooLarge,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl PostgresAgentStore {
    /// Validate, decode and store one immutable browser image upload.
    pub async fn put_inline_image(
        &self,
        mime_type: &str,
        data: &str,
    ) -> Result<ImageArtifactMetadata, ImageArtifactError> {
        let (mime_type, bytes) = validate_inline_image(mime_type, data)
            .map_err(|error| ImageArtifactError::InvalidImage(error.to_string()))?;
        let mut tx = self.pool.begin().await?;
        let artifact = put_bytes_tx(&mut tx, mime_type, bytes).await?;
        tx.commit().await?;
        Ok(artifact.metadata)
    }

    /// Admit complete durable user content and verify every referenced row.
    pub async fn admit_user_message(
        &self,
        message: agent_vocab::UserMessage,
    ) -> Result<agent_vocab::UserMessage, ImageArtifactError> {
        validate_durable_content(&message.content)
            .map_err(|error| ImageArtifactError::InvalidReference(error.to_string()))?;
        let mut tx = self.pool.begin().await?;
        require_content_refs_tx(&mut tx, &message.content).await?;
        tx.commit().await?;
        Ok(message)
    }

    /// Ingest one daemon-finalized transient result, storing valid images and
    /// replacing invalid image blocks in place with useful notes.
    pub async fn ingest_tool_result(
        &self,
        result: InlineToolResultMessage,
    ) -> Result<ToolResultMessage, ImageArtifactError> {
        let mut tx = self.pool.begin().await?;
        let mut content = Vec::with_capacity(result.content.len());
        let mut image_count = 0usize;
        let mut aggregate_bytes = 0usize;
        for block in result.content {
            match block {
                InlineContentBlock::Text { text } => content.push(ContentBlock::Text { text }),
                InlineContentBlock::Image { mime_type, data } => {
                    let validated = validate_inline_image(&mime_type, &data)
                        .map_err(|error| error.to_string())
                        .and_then(|(mime_type, bytes)| {
                            if image_count >= agent_vocab::MAX_IMAGES_PER_CONTENT {
                                return Err(format!(
                                    "at most {} images are allowed",
                                    agent_vocab::MAX_IMAGES_PER_CONTENT
                                ));
                            }
                            if aggregate_bytes.saturating_add(bytes.len())
                                > MAX_AGGREGATE_IMAGE_BYTES
                            {
                                return Err(format!(
                                    "aggregate image bytes exceed {MAX_AGGREGATE_IMAGE_BYTES}"
                                ));
                            }
                            Ok((mime_type, bytes))
                        });
                    match validated {
                        Ok((mime_type, bytes)) => {
                            image_count += 1;
                            aggregate_bytes += bytes.len();
                            let artifact = put_bytes_tx(&mut tx, mime_type, bytes).await?;
                            content.push(ContentBlock::image(artifact.metadata.artifact_id));
                        }
                        Err(error) => {
                            content.push(ContentBlock::text(format!("[image omitted: {error}]")))
                        }
                    }
                }
            }
        }
        if content.is_empty() {
            content.push(ContentBlock::text("[tool returned no content]"));
        }
        tx.commit().await?;
        Ok(ToolResultMessage {
            tool_call_id: result.tool_call_id,
            tool_name: result.tool_name,
            content,
            status: result.status,
        })
    }

    /// Batch-load and integrity-check all artifacts used by one provider request.
    pub async fn load_image_artifacts(
        &self,
        artifact_ids: impl IntoIterator<Item = ImageArtifactId>,
    ) -> Result<BTreeMap<ImageArtifactId, ImageArtifact>, ImageArtifactError> {
        let ids = artifact_ids.into_iter().collect::<BTreeSet<_>>();
        load_artifacts_pool(&self.pool, &ids).await
    }

    /// Load and integrity-check one image for the browser read RPC.
    pub async fn image_artifact(
        &self,
        artifact_id: &ImageArtifactId,
    ) -> Result<ImageArtifact, ImageArtifactError> {
        let mut artifacts =
            load_artifacts_pool(&self.pool, &BTreeSet::from([artifact_id.clone()])).await?;
        artifacts
            .remove(artifact_id)
            .ok_or_else(|| ImageArtifactError::Missing {
                artifact_id: artifact_id.clone(),
            })
    }
}

pub(super) async fn require_content_refs_tx(
    tx: &mut Transaction<'_, Postgres>,
    content: &[ContentBlock],
) -> Result<(), ImageArtifactError> {
    validate_durable_content(content)
        .map_err(|error| ImageArtifactError::InvalidReference(error.to_string()))?;
    let ids = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { .. } => None,
            ContentBlock::Image { artifact_id } => Some(artifact_id.clone()),
        })
        .collect::<BTreeSet<_>>();
    let artifacts = load_artifacts_tx(tx, &ids).await?;
    let aggregate = content
        .iter()
        .try_fold(0usize, |total, block| match block {
            ContentBlock::Text { .. } => Ok(total),
            ContentBlock::Image { artifact_id } => artifacts
                .get(artifact_id)
                .map(|artifact| total.saturating_add(artifact.metadata.byte_length))
                .ok_or_else(|| ImageArtifactError::Missing {
                    artifact_id: artifact_id.clone(),
                }),
        })?;
    if aggregate > MAX_AGGREGATE_IMAGE_BYTES {
        return Err(ImageArtifactError::AggregateTooLarge);
    }
    Ok(())
}

async fn put_bytes_tx(
    tx: &mut Transaction<'_, Postgres>,
    mime_type: &str,
    bytes: Vec<u8>,
) -> Result<ImageArtifact, ImageArtifactError> {
    let artifact_id = id_for_bytes(&bytes);
    let inserted = sqlx::query(
        r#"
        insert into image_artifacts (id, mime_type, data)
        values ($1, $2, $3)
        on conflict (id) do nothing
        returning id
        "#,
    )
    .bind(artifact_id.as_str())
    .bind(mime_type)
    .bind(&bytes)
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    let artifact = load_artifact(&mut **tx, &artifact_id).await?;
    if artifact.data != bytes || artifact.metadata.mime_type != mime_type {
        return Err(if inserted {
            ImageArtifactError::Corrupt {
                artifact_id,
                reason: "inserted row does not match uploaded bytes".to_string(),
            }
        } else {
            ImageArtifactError::Collision { artifact_id }
        });
    }
    Ok(artifact)
}

async fn load_artifacts_pool(
    pool: &sqlx::PgPool,
    ids: &BTreeSet<ImageArtifactId>,
) -> Result<BTreeMap<ImageArtifactId, ImageArtifact>, ImageArtifactError> {
    let rows = fetch_artifact_rows(pool, ids).await?;
    verify_loaded_artifacts(ids, rows)
}

async fn load_artifacts_tx(
    tx: &mut Transaction<'_, Postgres>,
    ids: &BTreeSet<ImageArtifactId>,
) -> Result<BTreeMap<ImageArtifactId, ImageArtifact>, ImageArtifactError> {
    let rows = fetch_artifact_rows(&mut **tx, ids).await?;
    verify_loaded_artifacts(ids, rows)
}

async fn fetch_artifact_rows<'e, E>(
    executor: E,
    ids: &BTreeSet<ImageArtifactId>,
) -> Result<Vec<sqlx::postgres::PgRow>, ImageArtifactError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let id_strings = ids
        .iter()
        .map(|id| id.as_str().to_string())
        .collect::<Vec<_>>();
    Ok(sqlx::query(
        r#"
        select id, mime_type, data, byte_length
        from image_artifacts
        where id = any($1)
        order by id
        "#,
    )
    .bind(&id_strings)
    .fetch_all(executor)
    .await?)
}

fn verify_loaded_artifacts(
    ids: &BTreeSet<ImageArtifactId>,
    rows: Vec<sqlx::postgres::PgRow>,
) -> Result<BTreeMap<ImageArtifactId, ImageArtifact>, ImageArtifactError> {
    let mut artifacts = BTreeMap::new();
    for row in rows {
        let artifact = artifact_from_row(row)?;
        artifacts.insert(artifact.metadata.artifact_id.clone(), artifact);
    }
    for artifact_id in ids {
        if !artifacts.contains_key(artifact_id) {
            return Err(ImageArtifactError::Missing {
                artifact_id: artifact_id.clone(),
            });
        }
    }
    Ok(artifacts)
}

async fn load_artifact<'e, E>(
    executor: E,
    artifact_id: &ImageArtifactId,
) -> Result<ImageArtifact, ImageArtifactError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let row =
        sqlx::query("select id, mime_type, data, byte_length from image_artifacts where id=$1")
            .bind(artifact_id.as_str())
            .fetch_optional(executor)
            .await?
            .ok_or_else(|| ImageArtifactError::Missing {
                artifact_id: artifact_id.clone(),
            })?;
    artifact_from_row(row)
}

fn artifact_from_row(row: sqlx::postgres::PgRow) -> Result<ImageArtifact, ImageArtifactError> {
    let raw_id: String = row.get("id");
    let artifact_id =
        ImageArtifactId::parse(raw_id).map_err(ImageArtifactError::InvalidReference)?;
    let mime_type: String = row.get("mime_type");
    let data: Vec<u8> = row.get("data");
    let byte_length: i32 = row.get("byte_length");
    let corrupt = |reason: String| ImageArtifactError::Corrupt {
        artifact_id: artifact_id.clone(),
        reason,
    };
    let expected_length =
        usize::try_from(byte_length).map_err(|_| corrupt("negative byte length".to_string()))?;
    if expected_length != data.len() {
        return Err(corrupt(format!(
            "stored length {expected_length} does not match {} bytes",
            data.len()
        )));
    }
    if sniff_mime(&data) != Some(mime_type.as_str()) {
        return Err(corrupt(format!(
            "stored MIME {mime_type} does not match image signature"
        )));
    }
    if id_for_bytes(&data) != artifact_id {
        return Err(corrupt("stored bytes do not match artifact id".to_string()));
    }
    Ok(ImageArtifact {
        metadata: ImageArtifactMetadata {
            artifact_id,
            mime_type,
            byte_length: data.len(),
        },
        data,
    })
}

fn id_for_bytes(bytes: &[u8]) -> ImageArtifactId {
    let digest = Sha256::digest(bytes);
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    ImageArtifactId::from_sha256_hex(&hex).expect("SHA-256 hex is a valid artifact id")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(90_000);

    #[test]
    fn hash_ids_are_stable_and_lowercase() {
        let id = id_for_bytes(b"hello");
        assert_eq!(
            id.as_str(),
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
    #[tokio::test]
    async fn upload_dedup_get_admission_and_tool_ingestion() {
        let Ok(admin_url) = std::env::var("PI_RELAY_TEST_DATABASE_URL") else {
            eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let name = format!(
            "pi_relay_artifact_test_{}_{}",
            std::process::id(),
            TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = sqlx::PgPool::connect(&admin_url)
            .await
            .expect("connect test administrator database");
        sqlx::query(&format!(r#"create database "{name}""#))
            .execute(&admin)
            .await
            .expect("create isolated artifact test database");
        admin.close().await;
        let database_url = database_url_with_name(&admin_url, &name);
        let store = PostgresAgentStore::connect(&database_url)
            .await
            .expect("connect isolated artifact database");
        store.migrate().await.expect("install real schema");

        let bytes = tiny_png();
        let png = agent_vocab::encode_base64(&bytes);
        let first = store
            .put_inline_image("image/png", &png)
            .await
            .expect("upload image");
        let second = store
            .put_inline_image("IMAGE/PNG", &png)
            .await
            .expect("deduplicate image");
        assert_eq!(first, second);
        let count: i64 = sqlx::query_scalar("select count(*) from image_artifacts")
            .fetch_one(&store.pool)
            .await
            .expect("count artifacts");
        assert_eq!(count, 1);
        assert_eq!(
            store
                .image_artifact(&first.artifact_id)
                .await
                .expect("read image")
                .data,
            bytes
        );
        store
            .admit_user_message(agent_vocab::UserMessage::from_parts(vec![
                ContentBlock::image(first.artifact_id.clone()),
            ]))
            .await
            .expect("admit existing ref");
        let missing =
            ImageArtifactId::parse(format!("sha256:{}", "f".repeat(64))).expect("valid missing id");
        assert!(matches!(
            store
                .admit_user_message(agent_vocab::UserMessage::from_parts(vec![
                    ContentBlock::image(missing)
                ]))
                .await,
            Err(ImageArtifactError::Missing { .. })
        ));

        let durable = store
            .ingest_tool_result(InlineToolResultMessage::success_content(
                agent_vocab::ToolCallId::new("call"),
                "capture",
                vec![
                    InlineContentBlock::text("before"),
                    InlineContentBlock::image("image/png", png),
                    InlineContentBlock::image("image/png", "invalid"),
                    InlineContentBlock::text("after"),
                ],
            ))
            .await
            .expect("ingest tool result");
        assert!(matches!(
            &durable.content[..],
            [
                ContentBlock::Text { .. },
                ContentBlock::Image { .. },
                ContentBlock::Text { .. },
                ContentBlock::Text { .. }
            ]
        ));
        assert!(durable.content[2].display_text().contains("image omitted"));
        assert!(
            sqlx::query("update image_artifacts set mime_type='image/gif' where id=$1")
                .bind(first.artifact_id.as_str())
                .execute(&store.pool)
                .await
                .is_err()
        );
        sqlx::query("alter table image_artifacts disable trigger image_artifacts_immutable")
            .execute(&store.pool)
            .await
            .expect("disable immutability only in isolated corruption test");
        sqlx::query("update image_artifacts set mime_type='image/gif' where id=$1")
            .bind(first.artifact_id.as_str())
            .execute(&store.pool)
            .await
            .expect("corrupt isolated fixture");
        sqlx::query("alter table image_artifacts enable trigger image_artifacts_immutable")
            .execute(&store.pool)
            .await
            .expect("restore immutability trigger");
        assert!(matches!(
            store.image_artifact(&first.artifact_id).await,
            Err(ImageArtifactError::Corrupt { .. })
        ));

        store.close().await;
        let admin = sqlx::PgPool::connect(&admin_url)
            .await
            .expect("reconnect test administrator database");
        sqlx::query(&format!(r#"drop database "{name}""#))
            .execute(&admin)
            .await
            .expect("drop only isolated artifact test database");
        admin.close().await;
    }

    fn tiny_png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ]
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
}
