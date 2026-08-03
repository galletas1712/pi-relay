use crate::message::ContentBlock;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use thiserror::Error;

pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_IMAGES_PER_CONTENT: usize = 4;
pub const MAX_AGGREGATE_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const ALLOWED_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ImageValidationError {
    #[error("{0}")]
    Message(String),
}

impl ImageValidationError {
    fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

pub fn validate_durable_content(content: &[ContentBlock]) -> Result<(), ImageValidationError> {
    if content.is_empty() {
        return Err(ImageValidationError::msg("message content is empty"));
    }
    let image_count = content
        .iter()
        .filter(|block| matches!(block, ContentBlock::Image { .. }))
        .count();
    if image_count > MAX_IMAGES_PER_CONTENT {
        return Err(ImageValidationError::msg(format!(
            "at most {MAX_IMAGES_PER_CONTENT} images are allowed"
        )));
    }
    Ok(())
}

pub fn validate_inline_image(
    mime_type: &str,
    data: &str,
) -> Result<(&'static str, Vec<u8>), ImageValidationError> {
    let mime_type = normalize_mime_type(mime_type)?;
    let decoded = decode_base64_bounded(data)?;
    let sniffed = sniff_mime(&decoded).ok_or_else(|| {
        ImageValidationError::msg("image bytes are not a supported PNG/JPEG/GIF/WebP")
    })?;
    if sniffed != mime_type {
        return Err(ImageValidationError::msg(format!(
            "declared MIME type {mime_type} does not match image bytes ({sniffed})"
        )));
    }
    Ok((mime_type, decoded))
}

pub fn normalize_mime_type(mime_type: &str) -> Result<&'static str, ImageValidationError> {
    let lowered = mime_type.trim().to_ascii_lowercase();
    let normalized = match lowered.as_str() {
        "image/jpg" => "image/jpeg",
        other => other,
    };
    ALLOWED_MIME_TYPES
        .iter()
        .copied()
        .find(|allowed| *allowed == normalized)
        .ok_or_else(|| {
            ImageValidationError::msg(format!(
                "unsupported image MIME type `{mime_type}`; allowed: {}",
                ALLOWED_MIME_TYPES.join(", ")
            ))
        })
}

pub fn decode_base64_bounded(data: &str) -> Result<Vec<u8>, ImageValidationError> {
    // Reject clearly oversized payloads before allocating a full decode buffer.
    // Base64 expands ~4/3, so encoded length above this cannot fit the decoded cap.
    let max_encoded = MAX_IMAGE_BYTES
        .saturating_mul(4)
        .div_ceil(3)
        .saturating_add(8);
    if data.len() > max_encoded {
        return Err(ImageValidationError::msg(format!(
            "image exceeds {MAX_IMAGE_BYTES} decoded bytes"
        )));
    }
    let decoded = STANDARD
        .decode(data.trim().as_bytes())
        .map_err(|_| ImageValidationError::msg("image base64 is invalid"))?;
    if decoded.len() > MAX_IMAGE_BYTES {
        return Err(ImageValidationError::msg(format!(
            "image exceeds {MAX_IMAGE_BYTES} decoded bytes"
        )));
    }
    if decoded.is_empty() {
        return Err(ImageValidationError::msg("image data is empty"));
    }
    Ok(decoded)
}

pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

pub fn encode_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png_base64() -> String {
        // 1x1 transparent PNG
        encode_base64(&[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ])
    }

    #[test]
    fn validates_png_base64() {
        let (mime_type, bytes) =
            validate_inline_image("IMAGE/PNG", &tiny_png_base64()).expect("valid");
        assert_eq!(mime_type, "image/png");
        assert_eq!(sniff_mime(&bytes), Some("image/png"));
    }

    #[test]
    fn rejects_bad_base64_and_mime_mismatch() {
        assert!(validate_inline_image("image/png", "not-base64!!!").is_err());
        assert!(validate_inline_image("image/jpeg", &tiny_png_base64()).is_err());
    }
}
