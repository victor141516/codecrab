use std::{
    fs,
    io::{BufReader, Cursor, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use exif::{In, Reader as ExifReader, Tag};
use image::{
    DynamicImage, GenericImageView, ImageEncoder, ImageFormat, ImageReader,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
    imageops::FilterType,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
pub(crate) const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const DEFAULT_IMAGE_EDGE: u32 = 1_024;
const HIGH_DETAIL_IMAGE_EDGE: u32 = 2_048;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttachmentKind {
    File,
    Image,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct StoredRendition {
    pub path: PathBuf,
    pub mime_type: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Attachment {
    pub id: Uuid,
    pub sha256: String,
    pub display_name: String,
    pub mime_type: String,
    pub size: u64,
    pub kind: AttachmentKind,
    pub original: StoredRendition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<StoredRendition>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImageDetail {
    #[default]
    Preview,
    Original,
}

#[derive(Clone)]
pub(crate) struct AttachmentStore {
    root: PathBuf,
    data_prefix: PathBuf,
}

impl AttachmentStore {
    pub(crate) fn new(project_root: &Path) -> Self {
        Self {
            root: project_root.to_path_buf(),
            data_prefix: PathBuf::from(".codecrab").join("session-data"),
        }
    }

    pub(crate) fn no_project(data_root: &Path) -> Self {
        Self {
            root: data_root.to_path_buf(),
            data_prefix: PathBuf::from("session-data"),
        }
    }

    pub(crate) fn session_dir(&self, session_id: Uuid) -> PathBuf {
        self.root
            .join(&self.data_prefix)
            .join(session_id.to_string())
    }

    pub(crate) fn upload_temp_path(&self, session_id: Uuid) -> Result<PathBuf> {
        let dir = self.session_dir(session_id).join("uploads");
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(dir.join(format!("{}.tmp", Uuid::new_v4())))
    }

    pub(crate) fn find_by_hash<'a>(
        attachments: &'a [Attachment],
        sha256: &str,
    ) -> Option<&'a Attachment> {
        attachments
            .iter()
            .find(|attachment| attachment.sha256.eq_ignore_ascii_case(sha256))
    }

    pub(crate) fn path_is_supported_image(path: &Path) -> bool {
        let Ok(mut file) = fs::File::open(path) else {
            return false;
        };
        let mut signature = [0_u8; 32];
        let Ok(read) = file.read(&mut signature) else {
            return false;
        };
        image::guess_format(&signature[..read]).is_ok()
    }

    pub(crate) fn import_path(
        &self,
        session_id: Uuid,
        attachments: &[Attachment],
        path: &Path,
    ) -> Result<Attachment> {
        let metadata = fs::metadata(path)
            .with_context(|| format!("cannot inspect attachment {}", path.display()))?;
        if !metadata.is_file() {
            anyhow::bail!("attachment is not a file: {}", path.display());
        }
        if metadata.len() > MAX_ATTACHMENT_BYTES {
            anyhow::bail!(
                "attachment exceeds the {} MiB limit",
                MAX_ATTACHMENT_BYTES / 1024 / 1024
            );
        }
        let bytes =
            fs::read(path).with_context(|| format!("cannot read attachment {}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attachment");
        self.import_bytes(session_id, attachments, name, None, &bytes, None)
    }

    pub(crate) fn import_uploaded_file(
        &self,
        session_id: Uuid,
        attachments: &[Attachment],
        temp_path: &Path,
        display_name: &str,
        declared_mime: Option<&str>,
        expected_sha256: &str,
    ) -> Result<Attachment> {
        let bytes = fs::read(temp_path)
            .with_context(|| format!("cannot read uploaded attachment {}", temp_path.display()))?;
        self.import_bytes(
            session_id,
            attachments,
            display_name,
            declared_mime,
            &bytes,
            Some(expected_sha256),
        )
    }

    pub(crate) fn import_rgba(
        &self,
        session_id: Uuid,
        attachments: &[Attachment],
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<Attachment> {
        validate_dimensions(width, height)?;
        let expected = width as usize * height as usize * 4;
        if rgba.len() != expected {
            anyhow::bail!("clipboard image has invalid RGBA byte length");
        }
        let mut canonical = Vec::with_capacity(8 + rgba.len());
        canonical.extend_from_slice(&width.to_le_bytes());
        canonical.extend_from_slice(&height.to_le_bytes());
        canonical.extend_from_slice(rgba);
        let hash = sha256_hex(&canonical);
        if let Some(existing) = Self::find_by_hash(attachments, &hash) {
            return Ok(existing.clone());
        }
        let image = DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(width, height, rgba.to_vec())
                .context("clipboard image has invalid dimensions")?,
        );
        let original = encode_png(&image)?;
        self.store_image(
            session_id,
            "clipboard.png",
            "image/png",
            hash,
            original,
            image,
        )
    }

    fn import_bytes(
        &self,
        session_id: Uuid,
        attachments: &[Attachment],
        display_name: &str,
        declared_mime: Option<&str>,
        bytes: &[u8],
        expected_sha256: Option<&str>,
    ) -> Result<Attachment> {
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            anyhow::bail!(
                "attachment exceeds the {} MiB limit",
                MAX_ATTACHMENT_BYTES / 1024 / 1024
            );
        }
        let hash = sha256_hex(bytes);
        if let Some(expected) = expected_sha256 {
            validate_sha256(expected)?;
            if !hash.eq_ignore_ascii_case(expected) {
                anyhow::bail!("attachment SHA-256 does not match the uploaded bytes");
            }
        }
        if let Some(existing) = Self::find_by_hash(attachments, &hash) {
            return Ok(existing.clone());
        }
        let display_name = safe_display_name(display_name);
        if let Ok(format) = image::guess_format(bytes) {
            let image = decode_oriented_image(bytes, format)?;
            let mime = image_mime(format).to_owned();
            return self.store_image(
                session_id,
                &display_name,
                &mime,
                hash,
                bytes.to_vec(),
                image,
            );
        }
        let mime_type =
            normalize_mime(declared_mime).unwrap_or_else(|| "application/octet-stream".into());
        let relative = self.original_relative_path(session_id, &hash);
        self.publish_bytes(&relative, bytes)?;
        Ok(Attachment {
            id: Uuid::new_v4(),
            sha256: hash,
            display_name,
            mime_type: mime_type.clone(),
            size: bytes.len() as u64,
            kind: AttachmentKind::File,
            original: StoredRendition {
                path: relative,
                mime_type,
                size: bytes.len() as u64,
                width: None,
                height: None,
            },
            preview: None,
        })
    }

    fn store_image(
        &self,
        session_id: Uuid,
        display_name: &str,
        original_mime: &str,
        hash: String,
        original_bytes: Vec<u8>,
        image: DynamicImage,
    ) -> Result<Attachment> {
        let (width, height) = image.dimensions();
        validate_dimensions(width, height)?;
        let original_relative = self.original_relative_path(session_id, &hash);
        self.publish_bytes(&original_relative, &original_bytes)?;
        let preview_image = bounded(&image, DEFAULT_IMAGE_EDGE);
        let (preview_bytes, preview_mime, extension) = encode_model_image(&preview_image)?;
        let preview_relative = self
            .attachment_relative_dir(session_id, &hash)
            .join(format!("preview.{extension}"));
        self.publish_bytes(&preview_relative, &preview_bytes)?;
        let (preview_width, preview_height) = preview_image.dimensions();
        Ok(Attachment {
            id: Uuid::new_v4(),
            sha256: hash,
            display_name: safe_display_name(display_name),
            mime_type: original_mime.to_owned(),
            size: original_bytes.len() as u64,
            kind: AttachmentKind::Image,
            original: StoredRendition {
                path: original_relative,
                mime_type: original_mime.to_owned(),
                size: original_bytes.len() as u64,
                width: Some(width),
                height: Some(height),
            },
            preview: Some(StoredRendition {
                path: preview_relative,
                mime_type: preview_mime,
                size: preview_bytes.len() as u64,
                width: Some(preview_width),
                height: Some(preview_height),
            }),
        })
    }

    fn publish_bytes(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        let destination = self.root.join(relative);
        let parent = destination
            .parent()
            .context("attachment path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create attachment directory {}", parent.display()))?;
        if destination.exists() {
            return Ok(());
        }
        let temp = parent.join(format!(".{}.tmp", Uuid::new_v4()));
        fs::write(&temp, bytes).with_context(|| format!("cannot write {}", temp.display()))?;
        match fs::rename(&temp, &destination) {
            Ok(()) => Ok(()),
            Err(_) if destination.exists() => {
                let _ = fs::remove_file(&temp);
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_file(&temp);
                Err(error).with_context(|| format!("cannot publish {}", destination.display()))
            }
        }
    }

    pub(crate) fn visible_reference(&self, attachment: &Attachment) -> String {
        let path = attachment
            .preview
            .as_ref()
            .map(|preview| &preview.path)
            .unwrap_or(&attachment.original.path);
        format!("@{}", path.to_string_lossy().replace('\\', "/"))
    }

    pub(crate) fn image_data_url(
        &self,
        attachment: &Attachment,
        detail: ImageDetail,
    ) -> Result<String> {
        if attachment.kind != AttachmentKind::Image {
            anyhow::bail!("attachment {} is not an image", attachment.id);
        }
        let (bytes, mime) = match detail {
            ImageDetail::Preview => {
                let preview = attachment
                    .preview
                    .as_ref()
                    .context("image preview is missing")?;
                (
                    fs::read(self.root.join(&preview.path))?,
                    preview.mime_type.clone(),
                )
            }
            ImageDetail::Original => {
                let bytes = fs::read(self.root.join(&attachment.original.path))?;
                let format =
                    image::guess_format(&bytes).context("stored image format is unsupported")?;
                let image = decode_oriented_image(&bytes, format)?;
                let bounded = bounded(&image, HIGH_DETAIL_IMAGE_EDGE);
                let (bytes, mime, _) = encode_model_image(&bounded)?;
                (bytes, mime)
            }
        };
        Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
    }

    fn attachment_relative_dir(&self, session_id: Uuid, hash: &str) -> PathBuf {
        self.data_prefix
            .join(session_id.to_string())
            .join("attachments")
            .join(hash)
    }

    fn original_relative_path(&self, session_id: Uuid, hash: &str) -> PathBuf {
        self.attachment_relative_dir(session_id, hash)
            .join("original")
    }
}

pub(crate) fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("attachment SHA-256 must contain exactly 64 hexadecimal characters");
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn safe_display_name(value: &str) -> String {
    let file_name = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment");
    let cleaned = file_name
        .chars()
        .filter(|character| !character.is_control())
        .take(255)
        .collect::<String>();
    if cleaned.trim().is_empty() {
        "attachment".into()
    } else {
        cleaned
    }
}

fn normalize_mime(value: Option<&str>) -> Option<String> {
    let value = value?.trim().to_ascii_lowercase();
    (value.len() <= 255 && value.contains('/') && value.bytes().all(|byte| byte.is_ascii_graphic()))
        .then_some(value)
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        anyhow::bail!("image dimensions exceed the decoded-pixel limit");
    }
    Ok(())
}

fn decode_oriented_image(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage> {
    let dimensions = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .context("cannot inspect image dimensions")?;
    validate_dimensions(dimensions.0, dimensions.1)?;
    let reader = ImageReader::with_format(Cursor::new(bytes), format);
    let image = reader.decode().context("cannot decode image attachment")?;
    let orientation = ExifReader::new()
        .read_from_container(&mut BufReader::new(Cursor::new(bytes)))
        .ok()
        .and_then(|exif| exif.get_field(Tag::Orientation, In::PRIMARY).cloned())
        .and_then(|field| field.value.get_uint(0))
        .unwrap_or(1);
    Ok(apply_orientation(image, orientation))
}

fn apply_orientation(image: DynamicImage, orientation: u32) -> DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}

fn bounded(image: &DynamicImage, edge: u32) -> DynamicImage {
    let (width, height) = image.dimensions();
    if width <= edge && height <= edge {
        image.clone()
    } else {
        image.resize(edge, edge, FilterType::Lanczos3)
    }
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>> {
    let rgba = image.to_rgba8();
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(
            &rgba,
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .context("cannot encode PNG image")?;
    Ok(bytes)
}

fn encode_model_image(image: &DynamicImage) -> Result<(Vec<u8>, String, &'static str)> {
    if image.color().has_alpha() {
        Ok((encode_png(image)?, "image/png".into(), "png"))
    } else {
        let rgb = image.to_rgb8();
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 84)
            .write_image(
                &rgb,
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
            .context("cannot encode JPEG image")?;
        Ok((bytes, "image/jpeg".into(), "jpg"))
    }
}

fn image_mime(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        ImageFormat::Tiff => "image/tiff",
        ImageFormat::Bmp => "image/bmp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    #[test]
    fn same_session_deduplicates_and_other_sessions_store_separately() {
        let root = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(root.path());
        let first_session = Uuid::new_v4();
        let other_session = Uuid::new_v4();
        let bytes = b"hello";
        let first = store
            .import_bytes(
                first_session,
                &[],
                "hello.txt",
                Some("text/plain"),
                bytes,
                None,
            )
            .unwrap();
        let duplicate = store
            .import_bytes(
                first_session,
                std::slice::from_ref(&first),
                "renamed.txt",
                None,
                bytes,
                None,
            )
            .unwrap();
        let other = store
            .import_bytes(other_session, &[], "hello.txt", None, bytes, None)
            .unwrap();
        assert_eq!(first.id, duplicate.id);
        assert_ne!(first.id, other.id);
        assert_ne!(first.original.path, other.original.path);
    }

    #[test]
    fn image_preview_never_upscales_and_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(root.path());
        let small = RgbaImage::from_pixel(20, 10, Rgba([1, 2, 3, 255]));
        let attachment = store
            .import_rgba(Uuid::new_v4(), &[], 20, 10, small.as_raw())
            .unwrap();
        let preview = attachment.preview.unwrap();
        assert_eq!((preview.width, preview.height), (Some(20), Some(10)));

        let large = RgbaImage::from_pixel(2_048, 1_024, Rgba([1, 2, 3, 255]));
        let attachment = store
            .import_rgba(Uuid::new_v4(), &[], 2_048, 1_024, large.as_raw())
            .unwrap();
        let preview = attachment.preview.unwrap();
        assert_eq!((preview.width, preview.height), (Some(1_024), Some(512)));
    }

    #[test]
    fn upload_hash_mismatch_and_unsafe_name_are_handled() {
        let root = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(root.path());
        let temp = root.path().join("upload.tmp");
        fs::write(&temp, b"bytes").unwrap();
        assert!(
            store
                .import_uploaded_file(
                    Uuid::new_v4(),
                    &[],
                    &temp,
                    "../../bad.txt",
                    None,
                    &"0".repeat(64)
                )
                .is_err()
        );
        let hash = sha256_hex(b"bytes");
        let attachment = store
            .import_uploaded_file(Uuid::new_v4(), &[], &temp, "../../bad.txt", None, &hash)
            .unwrap();
        assert_eq!(attachment.display_name, "bad.txt");
    }

    #[test]
    fn orientation_is_applied_before_rendition_generation() {
        let mut source = RgbaImage::new(2, 1);
        source.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        source.put_pixel(1, 0, Rgba([0, 255, 0, 255]));

        let oriented = apply_orientation(DynamicImage::ImageRgba8(source), 6);

        assert_eq!(oriented.dimensions(), (1, 2));
        assert_eq!(oriented.get_pixel(0, 0), Rgba([255, 0, 0, 255]));
        assert_eq!(oriented.get_pixel(0, 1), Rgba([0, 255, 0, 255]));
    }

    #[test]
    fn invalid_mime_claims_and_oversized_local_files_are_rejected_safely() {
        let root = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(root.path());
        let session_id = Uuid::new_v4();
        let attachment = store
            .import_bytes(
                session_id,
                &[],
                "claim.bin",
                Some("text/plain; charset=utf-8"),
                b"content",
                None,
            )
            .unwrap();
        assert_eq!(attachment.mime_type, "application/octet-stream");
        assert_eq!(
            fs::read(root.path().join(&attachment.original.path)).unwrap(),
            b"content"
        );
        let published_dir = root.path().join(attachment.original.path.parent().unwrap());
        assert!(fs::read_dir(published_dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));

        let oversized = root.path().join("oversized.bin");
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_ATTACHMENT_BYTES + 1)
            .unwrap();
        let error = store.import_path(session_id, &[], &oversized).unwrap_err();
        assert!(format!("{error:#}").contains("limit"));
    }
}
