use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};

use crate::staging::open_absolute_dir_nofollow;

const MAX_ASSET_ENTRIES: usize = 10_000;
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ASSET_DEPTH: usize = 32;

#[derive(Clone)]
pub(crate) struct StaticAssets {
    files: Arc<HashMap<String, Asset>>,
}

#[derive(Clone)]
struct Asset {
    bytes: Arc<[u8]>,
    content_type: HeaderValue,
}

impl StaticAssets {
    pub(crate) fn stage(web_root: &Path) -> anyhow::Result<Self> {
        let absolute = if web_root.is_absolute() {
            web_root.to_path_buf()
        } else {
            std::env::current_dir()?.join(web_root)
        };
        let source = open_absolute_dir_nofollow(&absolute).map_err(|error| {
            anyhow::anyhow!(
                "open web root {} through no-follow handles: {error}",
                absolute.display()
            )
        })?;
        let mut loader = AssetLoader {
            files: HashMap::new(),
            entries: 0,
            bytes: 0,
        };
        loader.load_dir(&source, Path::new(""), 0)?;
        if !loader.files.contains_key("index.html") {
            anyhow::bail!("web root must contain a regular index.html");
        }
        Ok(Self {
            files: Arc::new(loader.files),
        })
    }

    pub(crate) fn index_response(&self, method: &Method) -> Response {
        self.asset_response("index.html", method)
            .unwrap_or_else(|| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }

    pub(crate) fn response(&self, uri: &Uri, method: &Method) -> Response {
        if method != Method::GET && method != Method::HEAD {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        let Some(path) = normalized_request_path(uri.path()) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let path = if path.is_empty() { "index.html" } else { &path };
        self.asset_response(path, method)
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
    }

    fn asset_response(&self, path: &str, method: &Method) -> Option<Response> {
        let asset = self.files.get(path)?;
        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(Bytes::from_owner(asset.bytes.clone()))
        };
        Some(
            (
                [
                    (header::CONTENT_TYPE, asset.content_type.clone()),
                    (
                        header::CONTENT_LENGTH,
                        HeaderValue::from_str(&asset.bytes.len().to_string()).ok()?,
                    ),
                ],
                body,
            )
                .into_response(),
        )
    }
}

struct AssetLoader {
    files: HashMap<String, Asset>,
    entries: usize,
    bytes: u64,
}

impl AssetLoader {
    fn load_dir(&mut self, source: &Dir, relative: &Path, depth: usize) -> anyhow::Result<()> {
        if depth > MAX_ASSET_DEPTH {
            anyhow::bail!("web root exceeds the maximum directory depth");
        }
        let mut entries = source.entries()?.collect::<io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                anyhow::bail!("web root contains a non-UTF-8 file name");
            };
            let child_relative = relative.join(name);
            let metadata = source.symlink_metadata(name)?;
            if metadata.file_type().is_symlink() {
                anyhow::bail!(
                    "web root contains a symbolic link: {}",
                    child_relative.display()
                );
            }
            self.entries += 1;
            if self.entries > MAX_ASSET_ENTRIES {
                anyhow::bail!("web root contains too many entries");
            }
            if metadata.is_dir() {
                let child = cap_fs_ext::DirExt::open_dir_nofollow(source, name)?;
                self.load_dir(&child, &child_relative, depth + 1)?;
                continue;
            }
            if !metadata.is_file() {
                anyhow::bail!(
                    "web root contains a non-regular file: {}",
                    child_relative.display()
                );
            }

            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let mut file = source.open_with(name, &options)?;
            let opened = file.metadata()?;
            if !opened.is_file() {
                anyhow::bail!(
                    "web asset changed type while staging: {}",
                    child_relative.display()
                );
            }
            self.bytes = self
                .bytes
                .checked_add(opened.len())
                .ok_or_else(|| anyhow::anyhow!("web root is too large"))?;
            if self.bytes > MAX_ASSET_BYTES {
                anyhow::bail!("web root is too large");
            }
            let capacity = usize::try_from(opened.len())
                .map_err(|_| anyhow::anyhow!("web asset is too large"))?;
            let mut bytes = Vec::with_capacity(capacity);
            file.by_ref()
                .take(opened.len().saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != opened.len() {
                anyhow::bail!(
                    "web asset changed length while staging: {}",
                    child_relative.display()
                );
            }
            let key = path_key(&child_relative)?;
            let content_type = HeaderValue::from_str(
                mime_guess::from_path(&child_relative)
                    .first_or_octet_stream()
                    .as_ref(),
            )?;
            self.files.insert(
                key,
                Asset {
                    bytes: bytes.into(),
                    content_type,
                },
            );
        }
        Ok(())
    }
}

fn path_key(path: &Path) -> anyhow::Result<String> {
    let mut segments = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(segment) = component else {
            anyhow::bail!("web root contains an invalid path");
        };
        segments.push(
            segment
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("web root contains a non-UTF-8 path"))?,
        );
    }
    Ok(segments.join("/"))
}

fn normalized_request_path(path: &str) -> Option<String> {
    let encoded = path.strip_prefix('/')?;
    let mut decoded = Vec::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(*bytes.get(index + 1)?)?;
            let low = hex(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = std::str::from_utf8(&decoded).ok()?;
    let mut segments = Vec::new();
    for segment in decoded.split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment == "."
            || segment == ".."
            || segment.contains('\\')
            || segment.chars().any(char::is_control)
        {
            return None;
        }
        segments.push(segment);
    }
    Some(segments.join("/"))
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
