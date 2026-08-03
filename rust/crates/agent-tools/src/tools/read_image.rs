use std::path::{Component, Path, PathBuf};

use agent_vocab::{
    encode_base64, sniff_mime, InlineContentBlock, InlineToolResultMessage, ToolCall,
    ToolDefinition, MAX_IMAGE_BYTES,
};
use async_trait::async_trait;
#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_std::fs::{Dir, File, OpenOptions};
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;

use crate::context::ToolContext;
use crate::error::ToolResult;
use crate::registry::AgentTool;

#[derive(Debug, Clone, Copy)]
pub struct ReadImageTool;

#[derive(Debug, Deserialize)]
struct ReadImageArgs {
    path: String,
}

#[async_trait]
impl AgentTool for ReadImageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "ReadImage",
            "Read a PNG, JPEG, GIF, or WebP image from the session workspace and return it as \
             vision-capable image content. Use after capturing a screenshot (or any workspace \
             image) when you need to inspect the pixels."
                .to_string(),
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative path to a PNG, JPEG, GIF, or WebP image."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> ToolResult<InlineToolResultMessage> {
        let args: ReadImageArgs = serde_json::from_str(&call.args_json)?;
        match read_image(ctx, &args.path).await {
            Ok(content) => Ok(InlineToolResultMessage::success_content(
                call.id.clone(),
                &call.tool_name,
                content,
            )),
            Err(message) => Ok(InlineToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                message,
            )),
        }
    }
}

async fn read_image(ctx: &ToolContext, raw_path: &str) -> Result<Vec<InlineContentBlock>, String> {
    read_image_impl(ctx, raw_path, || {}).await
}

async fn read_image_impl(
    ctx: &ToolContext,
    raw_path: &str,
    before_open: impl FnOnce(),
) -> Result<Vec<InlineContentBlock>, String> {
    let relative = normalize_relative_path(raw_path)?;
    let expected_mime = mime_for_extension(&relative)?;
    let workspace = ctx.workspace_dir();
    before_open();
    let open_relative = relative.clone();
    let file =
        tokio::task::spawn_blocking(move || open_validated_image(&workspace, &open_relative))
            .await
            .map_err(|error| format!("failed to open image: {error}"))??;
    let file = tokio::fs::File::from_std(file.into_std());
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("failed to read image: {error}"))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!("image exceeds {MAX_IMAGE_BYTES} bytes"));
    }
    let mime_type = sniff_mime(&bytes)
        .ok_or_else(|| "file is not a supported PNG/JPEG/GIF/WebP image".to_string())?;
    if mime_type != expected_mime {
        return Err(format!(
            "image extension declares {expected_mime} but file signature is {mime_type}"
        ));
    }

    Ok(vec![
        InlineContentBlock::text(format!(
            "Read image {} ({mime_type}, {} bytes).",
            relative.display(),
            bytes.len()
        )),
        InlineContentBlock::image(mime_type, encode_base64(&bytes)),
    ])
}

fn open_validated_image(workspace: &Dir, relative: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.nonblock(true);

    let file = workspace
        .open_with(relative, &options)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                format!("image not found: {}", relative.display())
            }
            std::io::ErrorKind::PermissionDenied
                if error.to_string().contains("outside of the filesystem") =>
            {
                "image path escapes the session workspace".to_string()
            }
            _ => format!("failed to open image: {error}"),
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("failed to stat image: {error}"))?;
    if !metadata.is_file() {
        return Err("path is not a regular file".to_string());
    }
    if metadata.len() > MAX_IMAGE_BYTES as u64 {
        return Err(format!(
            "image exceeds {MAX_IMAGE_BYTES} bytes (got {} bytes)",
            metadata.len()
        ));
    }
    Ok(file)
}

fn mime_for_extension(path: &Path) -> Result<&'static str, String> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Ok("image/png"),
        Some("jpg" | "jpeg") => Ok("image/jpeg"),
        Some("gif") => Ok("image/gif"),
        Some("webp") => Ok("image/webp"),
        _ => Err("image path must end in .png, .jpg, .jpeg, .gif, or .webp".to_string()),
    }
}

fn normalize_relative_path(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("path must be a nonempty workspace-relative path".to_string());
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err("path must be workspace-relative, not absolute".to_string());
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                return Err("path must not contain `..`".to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("path must be workspace-relative".to_string());
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err("path must be a nonempty workspace-relative path".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    #[cfg(target_os = "linux")]
    use std::sync::mpsc;
    #[cfg(target_os = "linux")]
    use std::time::Duration;

    use agent_vocab::{decode_base64_bounded, ToolResultStatus};
    use cap_std::ambient_authority;

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempWorkspace {
        root: PathBuf,
        workspace: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "pi-relay-read-image-test-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let workspace = root.join("workspace");
            fs::create_dir_all(&workspace).expect("create disposable workspace");
            Self { root, workspace }
        }

        fn context(&self) -> ToolContext {
            ToolContext::new(
                &self.workspace,
                Dir::open_ambient_dir(&self.workspace, ambient_authority())
                    .expect("open disposable workspace"),
            )
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_symlink_cannot_escape_the_captured_workspace() {
        let temp = TempWorkspace::new();
        let original = temp.workspace.join("pixel.png");
        let checked = temp.workspace.join("checked.png");
        let outside = temp.root.join("outside.png");
        fs::write(&original, tiny_png()).expect("write original fixture");
        fs::write(&outside, tiny_png()).expect("write outside fixture");
        let ctx = temp.context();

        let result = read_image_impl(&ctx, "pixel.png", || {
            fs::rename(&original, &checked).expect("move validated fixture");
            std::os::unix::fs::symlink(&outside, &original)
                .expect("replace target with outside symlink");
        })
        .await;

        assert_eq!(
            result.expect_err("outside replacement must be rejected"),
            "image path escapes the session workspace"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replacement_fifo_is_rejected_without_waiting_for_a_writer() {
        let temp = TempWorkspace::new();
        let original = temp.workspace.join("pixel.png");
        fs::write(&original, tiny_png()).expect("write original fixture");
        let workspace = temp.context().workspace_dir();
        fs::remove_file(&original).expect("remove validated fixture");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &original,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .expect("replace target with FIFO");

        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let result = open_validated_image(&workspace, Path::new("pixel.png"));
            let _ = send.send(result);
        });

        let result = receive
            .recv_timeout(Duration::from_secs(2))
            .expect("capability open must not block on a FIFO");
        assert_eq!(
            result.expect_err("FIFO must not be read"),
            "path is not a regular file"
        );
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
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

    fn call(path: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            tool_name: "ReadImage".to_string(),
            args_json: json!({"path":path}).to_string(),
        }
    }

    async fn execute(path: &str, ctx: &ToolContext) -> InlineToolResultMessage {
        ReadImageTool
            .execute(&call(path), ctx)
            .await
            .expect("ReadImage returns a typed tool result")
    }

    #[tokio::test]
    async fn reads_ordered_text_and_image_content_from_disposable_workspace() {
        let temp = TempWorkspace::new();
        let bytes = tiny_png();
        fs::write(temp.workspace.join("pixel.png"), &bytes).expect("write PNG fixture");

        let result = execute("pixel.png", &temp.context()).await;

        assert_eq!(result.status, ToolResultStatus::Success);
        assert_eq!(result.content.len(), 2);
        assert!(matches!(
            &result.content[0],
            InlineContentBlock::Text { text }
                if text == &format!("Read image pixel.png (image/png, {} bytes).", bytes.len())
        ));
        let InlineContentBlock::Image { mime_type, data } = &result.content[1] else {
            panic!("second block is not image content");
        };
        assert_eq!(mime_type, "image/png");
        assert_eq!(
            decode_base64_bounded(data).expect("returned image base64 decodes"),
            bytes
        );
    }

    #[tokio::test]
    async fn rejects_traversal_absolute_and_outside_workspace_paths() {
        let temp = TempWorkspace::new();
        let outside = temp.root.join("outside.png");
        fs::write(&outside, tiny_png()).expect("write outside fixture");
        for (path, expected) in [
            ("../outside.png", "must not contain `..`"),
            (
                outside.to_str().expect("UTF-8 temp path"),
                "workspace-relative",
            ),
        ] {
            let result = execute(path, &temp.context()).await;
            assert_eq!(result.status, ToolResultStatus::Error);
            assert!(result.display_text().contains(expected));
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, temp.workspace.join("linked.png"))
                .expect("create outside symlink");
            let result = execute("linked.png", &temp.context()).await;
            assert_eq!(result.status, ToolResultStatus::Error);
            assert!(result
                .display_text()
                .contains("escapes the session workspace"));
        }
    }

    #[tokio::test]
    async fn rejects_oversized_file_from_sparse_disposable_fixture() {
        let temp = TempWorkspace::new();
        let path = temp.workspace.join("oversized.png");
        let file = fs::File::create(path).expect("create sparse oversized fixture");
        file.set_len(MAX_IMAGE_BYTES as u64 + 1)
            .expect("size sparse fixture");

        let result = execute("oversized.png", &temp.context()).await;

        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.display_text().contains("image exceeds"));
    }

    #[tokio::test]
    async fn rejects_malformed_and_extension_signature_mismatch() {
        let temp = TempWorkspace::new();
        fs::write(temp.workspace.join("malformed.png"), b"not an image")
            .expect("write malformed fixture");
        fs::write(temp.workspace.join("mismatch.jpg"), tiny_png())
            .expect("write mismatched fixture");

        let malformed = execute("malformed.png", &temp.context()).await;
        let mismatch = execute("mismatch.jpg", &temp.context()).await;

        assert_eq!(malformed.status, ToolResultStatus::Error);
        assert!(malformed.display_text().contains("not a supported"));
        assert_eq!(mismatch.status, ToolResultStatus::Error);
        assert!(mismatch
            .display_text()
            .contains("extension declares image/jpeg"));
    }
}
