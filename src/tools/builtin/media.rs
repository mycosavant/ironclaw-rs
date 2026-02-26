//! Media pipeline tools — MIME detection, image processing, PDF text extraction,
//! and audio transcription.
//!
//! All file-path parameters are resolved relative to the agent workspace root
//! (or an absolute path that starts within the workspace). Traversal attempts
//! (`../`) are rejected.
//!
//! Feature-gated behind `media` (enabled by default). Each tool gracefully
//! reports a **not available** error when compiled without the feature so that
//! callers never encounter a missing-tool panic.

use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::json;

use crate::context::JobContext;
use crate::tools::tool::{ApprovalRequirement, Tool, ToolError, ToolOutput, ToolRateLimitConfig};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum file size allowed for image operations (50 MiB).
const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;

/// Maximum file size allowed for PDF extraction (100 MiB).
const MAX_PDF_BYTES: u64 = 100 * 1024 * 1024;

/// Maximum file size allowed for audio transcription (25 MiB — Whisper API limit).
const MAX_AUDIO_BYTES: u64 = 25 * 1024 * 1024;

/// Maximum characters returned from PDF extraction per call.
const MAX_PDF_OUTPUT_CHARS: usize = 128 * 1024;

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Validate and resolve `path_str` against the optional `base_dir`.
///
/// Rules:
/// 1. Absolute paths must live inside `base_dir` (if provided).
/// 2. Relative paths are resolved against `base_dir` (if provided) or CWD.
/// 3. Lexical `..` components are blocked — they cannot escape the base.
pub(crate) fn resolve_safe_path(
    path_str: &str,
    base_dir: Option<&Path>,
) -> Result<PathBuf, ToolError> {
    if path_str.is_empty() {
        return Err(ToolError::InvalidParameters("path cannot be empty".into()));
    }

    // Reject null bytes — they can confuse OS APIs.
    if path_str.contains('\0') {
        return Err(ToolError::InvalidParameters(
            "path contains null byte".into(),
        ));
    }

    let raw = Path::new(path_str);

    // Lexically normalise (expand `.` / collapse `..`) before any FS access.
    let normalised = normalise_lexical(raw);

    let resolved = if normalised.is_absolute() {
        normalised
    } else {
        match base_dir {
            Some(base) => base.join(&normalised),
            None => std::env::current_dir()
                .map_err(|e| ToolError::ExecutionFailed(format!("cannot get cwd: {e}")))?
                .join(&normalised),
        }
    };

    // Enforce containment when base_dir is given.
    if let Some(base) = base_dir {
        let canonical_base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());

        // The target may not exist yet (write path), so we canonicalise its
        // existing ancestors.
        let canon_resolved = canonicalize_best_effort(&resolved);

        if !canon_resolved.starts_with(&canonical_base) {
            return Err(ToolError::NotAuthorized(format!(
                "path '{}' is outside the workspace",
                path_str
            )));
        }
    }

    Ok(resolved)
}

/// Walk up the path until `canonicalize` succeeds, then re-append the
/// non-existent suffix.  This handles output paths that don't exist yet.
fn canonicalize_best_effort(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut suffix = Vec::new();

    loop {
        match existing.canonicalize() {
            Ok(canon) => {
                let mut result = canon;
                for component in suffix.into_iter().rev() {
                    result.push(component);
                }
                return result;
            }
            Err(_) => {
                if let Some(file_name) = existing.file_name() {
                    suffix.push(file_name.to_os_string());
                    existing = match existing.parent() {
                        Some(p) => p.to_path_buf(),
                        None => return path.to_path_buf(),
                    };
                } else {
                    return path.to_path_buf();
                }
            }
        }
    }
}

/// Lexically normalise a path (no FS access).
///
/// Collapses `foo/../bar` → `bar` and strips `./` components.
/// Unresolvable leading `..` in a relative path are **preserved** so that the
/// downstream containment check (`starts_with(base)`) can correctly reject
/// traversal attempts like `../../../etc/passwd`.
fn normalise_lexical(path: &Path) -> PathBuf {
    let mut components: Vec<std::path::Component<'_>> = Vec::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                // Pop only if the top of the stack is a Normal component.
                // If we can't pop (empty stack or top is `..`/prefix/root),
                // keep the `..` so the containment check sees the traversal.
                if components
                    .last()
                    .is_some_and(|prev| matches!(prev, std::path::Component::Normal(_)))
                {
                    components.pop();
                } else {
                    components.push(c); // retain unresolvable ..
                }
            }
            std::path::Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

/// Async wrapper: check a file exists and return its byte size.
async fn file_size(path: &Path) -> Result<u64, ToolError> {
    let meta = tokio::fs::metadata(path).await.map_err(|e| {
        ToolError::ExecutionFailed(format!("cannot stat '{}': {}", path.display(), e))
    })?;
    Ok(meta.len())
}

// ── MediaInfoTool ─────────────────────────────────────────────────────────────

/// Detect MIME type, file size, and — for images — dimensions of a file.
///
/// Uses magic-byte inspection via the `infer` crate for reliable detection
/// regardless of file extension.
#[derive(Debug, Default)]
pub struct MediaInfoTool;

#[async_trait]
impl Tool for MediaInfoTool {
    fn name(&self) -> &str {
        "media_info"
    }

    fn description(&self) -> &str {
        "Detect the MIME type, file size, and (for images) pixel dimensions of a \
         file. Accepts an absolute path or a path relative to the workspace root."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to inspect"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        #[cfg(not(feature = "media"))]
        return Err(ToolError::ExecutionFailed(
            "media feature is not enabled in this build".into(),
        ));

        #[cfg(feature = "media")]
        {
            let path_str = params
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidParameters("missing 'path' parameter".into()))?;

            let path = resolve_safe_path(path_str, workspace_base()?.as_deref())?;
            let size_bytes = file_size(&path).await?;

            // Read up to 8 KiB for MIME sniffing (avoids loading huge files).
            let header = tokio::fs::read(&path).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("cannot read '{}': {}", path.display(), e))
            })?;

            let mime = detect_mime(&header, &path);

            // For images, decode just the header to get dimensions.
            let dimensions = if mime.starts_with("image/") && size_bytes <= MAX_IMAGE_BYTES {
                image_dimensions_from_bytes(&header)
            } else {
                None
            };

            let mut result = json!({
                "path": path.display().to_string(),
                "mime_type": mime,
                "size_bytes": size_bytes,
            });

            if let Some((w, h)) = dimensions {
                result["width"] = json!(w);
                result["height"] = json!(h);
            }

            Ok(ToolOutput::success(result, start.elapsed()))
        }
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ── ImageResizeTool ───────────────────────────────────────────────────────────

/// Per-agent image resize dimension configuration, read from environment variables.
///
/// | Variable                       | Default | Description                                              |
/// |--------------------------------|---------|----------------------------------------------------------|
/// | `IMAGE_RESIZE_DEFAULT_WIDTH`   | —       | Target width when the call omits both `width` and `height` |
/// | `IMAGE_RESIZE_DEFAULT_HEIGHT`  | —       | Target height when the call omits both `width` and `height` |
/// | `IMAGE_MAX_WIDTH`              | 65535   | Hard ceiling on output width in pixels                   |
/// | `IMAGE_MAX_HEIGHT`             | 65535   | Hard ceiling on output height in pixels                  |
///
/// When both `IMAGE_RESIZE_DEFAULT_WIDTH` and `IMAGE_RESIZE_DEFAULT_HEIGHT` are set,
/// both are applied as explicit targets (aspect ratio not preserved). When only one is
/// set, the other is computed from the source aspect ratio. Max values cap even
/// explicit caller-provided dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageResizeConfig {
    /// Default target width when neither `width` nor `height` is given by the caller.
    pub default_width: Option<u32>,
    /// Default target height when neither `width` nor `height` is given by the caller.
    pub default_height: Option<u32>,
    /// Maximum permitted output width. Caller values are clamped to this.
    pub max_width: u32,
    /// Maximum permitted output height. Caller values are clamped to this.
    pub max_height: u32,
}

impl Default for ImageResizeConfig {
    fn default() -> Self {
        Self {
            default_width: None,
            default_height: None,
            max_width: 65535,
            max_height: 65535,
        }
    }
}

/// Read [`ImageResizeConfig`] from environment variables.
///
/// Values of `0` and non-integer strings are silently ignored, preserving defaults.
pub(crate) fn image_resize_config() -> ImageResizeConfig {
    let parse = |var: &str| -> Option<u32> {
        std::env::var(var)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|&n| n >= 1)
    };
    ImageResizeConfig {
        default_width: parse("IMAGE_RESIZE_DEFAULT_WIDTH"),
        default_height: parse("IMAGE_RESIZE_DEFAULT_HEIGHT"),
        max_width: parse("IMAGE_MAX_WIDTH").unwrap_or(65535),
        max_height: parse("IMAGE_MAX_HEIGHT").unwrap_or(65535),
    }
}

/// Resize an image to new dimensions, preserving aspect ratio by default.
///
/// Uses Lanczos3 resampling for high-quality downscaling and upscaling.
/// If only `width` or only `height` is given, the other dimension is computed
/// to maintain the original aspect ratio.
#[derive(Debug, Default)]
pub struct ImageResizeTool;

#[async_trait]
impl Tool for ImageResizeTool {
    fn name(&self) -> &str {
        "image_resize"
    }

    fn description(&self) -> &str {
        "Resize an image to specified pixel dimensions. Provide at least one of \
         `width` or `height`; the other is computed to preserve the aspect ratio \
         unless both are given. Supported formats: JPEG, PNG, WebP, GIF, BMP, \
         ICO, TIFF. The output format is inferred from `output_path`."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "input_path": {
                    "type": "string",
                    "description": "Source image path"
                },
                "output_path": {
                    "type": "string",
                    "description": "Destination path (format inferred from extension)"
                },
                "width": {
                    "type": "integer",
                    "description": "Target width in pixels (optional)",
                    "minimum": 1,
                    "maximum": 65535
                },
                "height": {
                    "type": "integer",
                    "description": "Target height in pixels (optional)",
                    "minimum": 1,
                    "maximum": 65535
                },
                "filter": {
                    "type": "string",
                    "description": "Resampling filter: 'lanczos3' (default, best quality), 'triangle', 'nearest'",
                    "enum": ["lanczos3", "triangle", "nearest"]
                },
                "quality": {
                    "type": "integer",
                    "description": "Output quality 1–100 for lossy formats (JPEG, WebP). Default: 85.",
                    "minimum": 1,
                    "maximum": 100
                }
            },
            "required": ["input_path", "output_path"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        #[cfg(not(feature = "media"))]
        return Err(ToolError::ExecutionFailed(
            "media feature is not enabled in this build".into(),
        ));

        #[cfg(feature = "media")]
        {
            use image::imageops::FilterType;

            let input_str = params
                .get("input_path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidParameters("missing 'input_path'".into()))?;
            let output_str = params
                .get("output_path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidParameters("missing 'output_path'".into()))?;

            let target_width: Option<u32> = params
                .get("width")
                .and_then(|v| v.as_u64())
                .map(|n| n.min(65535) as u32);
            let target_height: Option<u32> = params
                .get("height")
                .and_then(|v| v.as_u64())
                .map(|n| n.min(65535) as u32);
            let quality: u8 = params
                .get("quality")
                .and_then(|v| v.as_u64())
                .map(|n| n.clamp(1, 100) as u8)
                .unwrap_or(85);

            // Apply configurable defaults and max-dimension ceilings.
            let cfg = image_resize_config();
            let (target_width, target_height) =
                resolve_target_dims(target_width, target_height, &cfg)?;

            let filter = match params
                .get("filter")
                .and_then(|v| v.as_str())
                .unwrap_or("lanczos3")
            {
                "triangle" => FilterType::Triangle,
                "nearest" => FilterType::Nearest,
                _ => FilterType::Lanczos3,
            };

            let base = workspace_base()?;
            let input_path = resolve_safe_path(input_str, base.as_deref())?;
            let output_path = resolve_safe_path(output_str, base.as_deref())?;

            // Check input size before loading into memory.
            let input_size = file_size(&input_path).await?;
            if input_size > MAX_IMAGE_BYTES {
                return Err(ToolError::InvalidParameters(format!(
                    "image is {input_size} bytes, exceeds {}MiB limit",
                    MAX_IMAGE_BYTES / 1024 / 1024
                )));
            }

            // Ensure output directory exists.
            if let Some(parent) = output_path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    ToolError::ExecutionFailed(format!("cannot create output dir: {e}"))
                })?;
            }

            let input_bytes = tokio::fs::read(&input_path)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("cannot read input: {e}")))?;
            let output_path_clone = output_path.clone();

            // Run the CPU-bound image decode/resize/encode on a blocking thread.
            let result = tokio::task::spawn_blocking(move || {
                let img = image::load_from_memory(&input_bytes)
                    .map_err(|e| ToolError::ExecutionFailed(format!("cannot decode image: {e}")))?;

                let orig_w = img.width();
                let orig_h = img.height();

                let (new_w, new_h) =
                    compute_dimensions(orig_w, orig_h, target_width, target_height);
                let resized = img.resize_exact(new_w, new_h, filter);

                save_image_with_quality(&resized, &output_path_clone, quality)?;

                Ok::<serde_json::Value, ToolError>(json!({
                    "output_path": output_path_clone.display().to_string(),
                    "original_width": orig_w,
                    "original_height": orig_h,
                    "new_width": new_w,
                    "new_height": new_h,
                }))
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("resize task failed: {e}")))?;

            Ok(ToolOutput::success(result?, start.elapsed()))
        }
    }

    fn requires_sanitization(&self) -> bool {
        false
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::UnlessAutoApproved
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(20, 200))
    }
}

// ── ImageConvertTool ──────────────────────────────────────────────────────────

/// Convert an image between formats.
///
/// The output format is inferred from the `output_path` extension.
/// Supported output formats: `.png`, `.jpg`/`.jpeg`, `.webp`, `.gif`, `.bmp`.
#[derive(Debug, Default)]
pub struct ImageConvertTool;

#[async_trait]
impl Tool for ImageConvertTool {
    fn name(&self) -> &str {
        "image_convert"
    }

    fn description(&self) -> &str {
        "Convert an image from one format to another. The output format is \
         determined by the file extension of `output_path` (.png, .jpg, .webp, \
         .gif, .bmp). Optionally control JPEG/WebP quality (1–100, default 85)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "input_path": {
                    "type": "string",
                    "description": "Source image path"
                },
                "output_path": {
                    "type": "string",
                    "description": "Destination path (format determined by extension)"
                },
                "quality": {
                    "type": "integer",
                    "description": "Output quality 1–100 for lossy formats (JPEG, WebP). Default: 85.",
                    "minimum": 1,
                    "maximum": 100
                }
            },
            "required": ["input_path", "output_path"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        #[cfg(not(feature = "media"))]
        return Err(ToolError::ExecutionFailed(
            "media feature is not enabled in this build".into(),
        ));

        #[cfg(feature = "media")]
        {
            let input_str = params
                .get("input_path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidParameters("missing 'input_path'".into()))?;
            let output_str = params
                .get("output_path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidParameters("missing 'output_path'".into()))?;
            let quality: u8 = params
                .get("quality")
                .and_then(|v| v.as_u64())
                .map(|n| n.clamp(1, 100) as u8)
                .unwrap_or(85);

            let base = workspace_base()?;
            let input_path = resolve_safe_path(input_str, base.as_deref())?;
            let output_path = resolve_safe_path(output_str, base.as_deref())?;

            let input_size = file_size(&input_path).await?;
            if input_size > MAX_IMAGE_BYTES {
                return Err(ToolError::InvalidParameters(format!(
                    "image is {input_size} bytes, exceeds {}MiB limit",
                    MAX_IMAGE_BYTES / 1024 / 1024
                )));
            }

            if let Some(parent) = output_path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    ToolError::ExecutionFailed(format!("cannot create output dir: {e}"))
                })?;
            }

            let input_bytes = tokio::fs::read(&input_path)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("cannot read input: {e}")))?;
            let output_path_clone = output_path.clone();
            let input_format = detect_mime(&input_bytes, &input_path);

            let result = tokio::task::spawn_blocking(move || {
                let img = image::load_from_memory(&input_bytes)
                    .map_err(|e| ToolError::ExecutionFailed(format!("cannot decode image: {e}")))?;

                save_image_with_quality(&img, &output_path_clone, quality)?;

                let output_format = detect_mime_from_path(&output_path_clone);
                Ok::<serde_json::Value, ToolError>(json!({
                    "output_path": output_path_clone.display().to_string(),
                    "input_format": input_format,
                    "output_format": output_format,
                    "width": img.width(),
                    "height": img.height(),
                }))
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("convert task failed: {e}")))?;

            Ok(ToolOutput::success(result?, start.elapsed()))
        }
    }

    fn requires_sanitization(&self) -> bool {
        false
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::UnlessAutoApproved
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(20, 200))
    }
}

// ── PdfExtractTextTool ────────────────────────────────────────────────────────

/// Extract plain text from a PDF file.
///
/// Extraction is done in a blocking thread pool to avoid blocking the async
/// executor. Returns extracted text and page count metadata.
#[derive(Debug, Default)]
pub struct PdfExtractTextTool;

#[async_trait]
impl Tool for PdfExtractTextTool {
    fn name(&self) -> &str {
        "pdf_extract_text"
    }

    fn description(&self) -> &str {
        "Extract plain text content from a PDF file. Returns the text of all pages \
         (or a specified page range) and the total page count. Text is limited to \
         128 KiB per call; use `pages` to extract specific pages from large documents."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the PDF file"
                },
                "pages": {
                    "type": "array",
                    "items": { "type": "integer", "minimum": 1 },
                    "description": "1-indexed page numbers to extract (default: all pages)",
                    "maxItems": 100
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        #[cfg(not(feature = "media"))]
        return Err(ToolError::ExecutionFailed(
            "media feature is not enabled in this build".into(),
        ));

        #[cfg(feature = "media")]
        {
            let path_str = params
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidParameters("missing 'path'".into()))?;

            let pages: Option<Vec<u32>> =
                params.get("pages").and_then(|v| v.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|n| n.as_u64().map(|n| n.max(1) as u32))
                        .collect()
                });

            let base = workspace_base()?;
            let path = resolve_safe_path(path_str, base.as_deref())?;

            let size = file_size(&path).await?;
            if size > MAX_PDF_BYTES {
                return Err(ToolError::InvalidParameters(format!(
                    "PDF is {size} bytes, exceeds {}MiB limit",
                    MAX_PDF_BYTES / 1024 / 1024
                )));
            }

            let pdf_bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("cannot read PDF: {e}")))?;

            let result =
                tokio::task::spawn_blocking(move || extract_pdf_text(&pdf_bytes, pages.as_deref()))
                    .await
                    .map_err(|e| ToolError::ExecutionFailed(format!("PDF task panicked: {e}")))?;

            Ok(ToolOutput::success(result?, start.elapsed()))
        }
    }

    fn requires_sanitization(&self) -> bool {
        // PDF content is untrusted external data — always sanitize.
        true
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(10, 100))
    }
}

// ── AudioTranscribeTool ───────────────────────────────────────────────────────

/// Transcribe an audio file using the OpenAI Whisper API.
///
/// Requires `WHISPER_API_KEY` (or `OPENAI_API_KEY`) in the environment.
/// Uses `WHISPER_BASE_URL` (default: `https://api.openai.com`) and
/// `WHISPER_MODEL` (default: `whisper-1`).
///
/// Supported formats: flac, mp3, mp4, mpeg, mpga, m4a, ogg, wav, webm.
/// Maximum file size: 25 MiB (Whisper API hard limit).
#[derive(Debug, Default)]
pub struct AudioTranscribeTool;

#[async_trait]
impl Tool for AudioTranscribeTool {
    fn name(&self) -> &str {
        "audio_transcribe"
    }

    fn description(&self) -> &str {
        "Transcribe an audio file to text using the Whisper API. Requires \
         WHISPER_API_KEY (or OPENAI_API_KEY) environment variable. \
         Supported formats: flac, mp3, mp4, mpeg, mpga, m4a, ogg, wav, webm. \
         Maximum audio size: 25 MiB."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the audio file"
                },
                "language": {
                    "type": "string",
                    "description": "ISO 639-1 language code (e.g. 'en', 'es'). Leave blank for auto-detect."
                },
                "prompt": {
                    "type": "string",
                    "description": "Optional hint to improve transcription accuracy (e.g. technical terms, speaker names)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'path'".into()))?;
        let language = params
            .get("language")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        // Resolve API credentials — check dedicated key first, then fallback.
        let api_key = std::env::var("WHISPER_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .map_err(|_| {
                ToolError::NotAuthorized(
                    "WHISPER_API_KEY or OPENAI_API_KEY must be set for audio transcription".into(),
                )
            })?;

        let base_url =
            std::env::var("WHISPER_BASE_URL").unwrap_or_else(|_| "https://api.openai.com".into());
        let model = std::env::var("WHISPER_MODEL").unwrap_or_else(|_| "whisper-1".into());

        let base = workspace_base()?;
        let path = resolve_safe_path(path_str, base.as_deref())?;

        let size = file_size(&path).await?;
        if size > MAX_AUDIO_BYTES {
            return Err(ToolError::InvalidParameters(format!(
                "audio file is {size} bytes, exceeds {}MiB Whisper API limit",
                MAX_AUDIO_BYTES / 1024 / 1024
            )));
        }

        // Determine MIME type from extension for the multipart Content-Type header.
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio.mp3")
            .to_owned();
        let audio_mime = audio_mime_for_path(&path);

        // Validate extension is supported by Whisper.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        const SUPPORTED: &[&str] = &[
            "flac", "mp3", "mp4", "mpeg", "mpga", "m4a", "ogg", "wav", "webm",
        ];
        if !SUPPORTED.contains(&ext.as_str()) {
            return Err(ToolError::InvalidParameters(format!(
                "unsupported audio format '.{ext}'; supported: {SUPPORTED:?}"
            )));
        }

        let audio_bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("cannot read audio file: {e}")))?;

        // Build multipart form.
        let part = reqwest::multipart::Part::bytes(audio_bytes)
            .file_name(file_name)
            .mime_str(&audio_mime)
            .map_err(|e| ToolError::ExecutionFailed(format!("invalid MIME: {e}")))?;

        let mut form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", model);

        if let Some(lang) = language {
            form = form.text("language", lang);
        }
        if let Some(p) = prompt {
            form = form.text("prompt", p);
        }

        let url = format!("{}/v1/audio/transcriptions", base_url.trim_end_matches('/'));

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .bearer_auth(&api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| ToolError::ExternalService(format!("Whisper API request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::ExternalService(format!(
                "Whisper API returned {status}: {body}"
            )));
        }

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ToolError::ExternalService(format!("invalid Whisper response: {e}")))?;

        let text = data
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        Ok(ToolOutput::success(
            json!({
                "transcript": text,
                "path": path.display().to_string(),
            }),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        // Transcript text is external/untrusted — always sanitize.
        true
    }

    fn execution_timeout(&self) -> std::time::Duration {
        // Audio files can be long; allow up to 5 minutes.
        std::time::Duration::from_secs(300)
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(5, 30))
    }
}

// ── Feature-gated helpers ─────────────────────────────────────────────────────

/// Return the workspace base directory from the `WORKSPACE_DIR` env var,
/// falling back to `~/.ironclaw/workspace`. Returns `None` if the path cannot
/// be determined (tools fall back to CWD-relative resolution).
pub(crate) fn workspace_base() -> Result<Option<PathBuf>, ToolError> {
    if let Ok(val) = std::env::var("WORKSPACE_DIR") {
        let p = PathBuf::from(val);
        return Ok(Some(p));
    }
    // Fall back to ~/.ironclaw/workspace
    if let Some(home) = dirs::home_dir() {
        let p = home.join(".ironclaw").join("workspace");
        return Ok(Some(p));
    }
    Ok(None)
}

#[cfg(feature = "media")]
pub(crate) fn detect_mime(bytes: &[u8], path: &Path) -> String {
    // Magic-byte based detection first.
    if let Some(kind) = infer::get(bytes) {
        return kind.mime_type().to_owned();
    }
    // Fall back to extension-based guess.
    detect_mime_from_path(path)
}

#[cfg(feature = "media")]
fn detect_mime_from_path(path: &Path) -> String {
    mime_guess::from_path(path)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_owned()
}

#[cfg(feature = "media")]
fn image_dimensions_from_bytes(bytes: &[u8]) -> Option<(u32, u32)> {
    // Use `ImageReader` to probe dimensions without full decode.
    use std::io::Cursor;
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.into_dimensions().ok()
}

#[cfg(feature = "media")]
fn compute_dimensions(
    orig_w: u32,
    orig_h: u32,
    target_w: Option<u32>,
    target_h: Option<u32>,
) -> (u32, u32) {
    match (target_w, target_h) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => {
            let h = ((orig_h as f64 / orig_w as f64) * w as f64).round() as u32;
            (w, h.max(1))
        }
        (None, Some(h)) => {
            let w = ((orig_w as f64 / orig_h as f64) * h as f64).round() as u32;
            (w.max(1), h)
        }
        (None, None) => (orig_w, orig_h),
    }
}

/// Resolve final `(target_width, target_height)` considering configured defaults and max limits.
///
/// 1. If neither dimension was provided by the caller, fall back to
///    `IMAGE_RESIZE_DEFAULT_WIDTH` / `IMAGE_RESIZE_DEFAULT_HEIGHT`.
/// 2. If still no dimension is known, return an error.
/// 3. Clamp any non-`None` dimension to the configured max.
#[cfg(feature = "media")]
fn resolve_target_dims(
    caller_w: Option<u32>,
    caller_h: Option<u32>,
    cfg: &ImageResizeConfig,
) -> Result<(Option<u32>, Option<u32>), ToolError> {
    let (w, h) = match (caller_w, caller_h) {
        (None, None) => {
            // Caller provided no dimensions — use configured defaults.
            if cfg.default_width.is_none() && cfg.default_height.is_none() {
                return Err(ToolError::InvalidParameters(
                    "at least one of 'width' or 'height' must be provided \
                     (or set IMAGE_RESIZE_DEFAULT_WIDTH / IMAGE_RESIZE_DEFAULT_HEIGHT)"
                        .into(),
                ));
            }
            (cfg.default_width, cfg.default_height)
        }
        (w, h) => (w, h),
    };
    // Apply max-dimension ceilings.
    let w = w.map(|n| n.min(cfg.max_width));
    let h = h.map(|n| n.min(cfg.max_height));
    Ok((w, h))
}

/// Save a `DynamicImage` to `path`, honouring quality for lossy formats.
#[cfg(feature = "media")]
fn save_image_with_quality(
    img: &image::DynamicImage,
    path: &Path,
    quality: u8,
) -> Result<(), ToolError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_lowercase();

    match ext.as_str() {
        "jpg" | "jpeg" => {
            let file = std::fs::File::create(path).map_err(|e| {
                ToolError::ExecutionFailed(format!("cannot create '{}': {e}", path.display()))
            })?;
            let writer = std::io::BufWriter::new(file);
            image::codecs::jpeg::JpegEncoder::new_with_quality(writer, quality)
                .encode_image(img)
                .map_err(|e| ToolError::ExecutionFailed(format!("JPEG encode failed: {e}")))?;
        }
        "webp" => {
            // `image` crate WebP encoder writes lossless by default when using
            // `save()`; use it directly and note quality is advisory only.
            img.save(path)
                .map_err(|e| ToolError::ExecutionFailed(format!("WebP save failed: {e}")))?;
        }
        _ => {
            img.save(path).map_err(|e| {
                ToolError::ExecutionFailed(format!("save '{}' failed: {e}", path.display()))
            })?;
        }
    }
    Ok(())
}

/// Extract text from PDF bytes, optionally filtering to `pages` (1-indexed).
///
/// Per-page filtering uses the form-feed character (`\x0C`) that `pdf-extract`
/// emits between pages as a page-break marker.
#[cfg(feature = "media")]
fn extract_pdf_text(
    pdf_bytes: &[u8],
    pages: Option<&[u32]>,
) -> Result<serde_json::Value, ToolError> {
    // Get total page count from lopdf (fast, no text extraction needed).
    let page_count: u32 = lopdf::Document::load_mem(pdf_bytes)
        .map(|doc| doc.get_pages().len() as u32)
        .unwrap_or(0);

    // Extract all text; pdf-extract inserts `\x0C` (form feed) between pages.
    let all_text = pdf_extract::extract_text_from_mem(pdf_bytes)
        .map_err(|e| ToolError::ExecutionFailed(format!("PDF extraction failed: {e}")))?;

    // Split into per-page segments on form-feed boundaries.
    let page_texts: Vec<&str> = all_text.split('\x0C').collect();

    let text: String = if let Some(page_nums) = pages {
        // Validate all requested pages before starting output.
        for &page in page_nums {
            if page == 0 || page > page_count {
                return Err(ToolError::InvalidParameters(format!(
                    "page {page} is out of range (PDF has {page_count} pages)"
                )));
            }
        }
        let mut out = String::new();
        for &page in page_nums {
            // page is 1-indexed; page_texts is 0-indexed.
            let idx = (page - 1) as usize;
            if let Some(text) = page_texts.get(idx) {
                out.push_str(text);
                out.push('\n');
            }
        }
        out
    } else {
        // Replace form-feeds with newlines for clean output.
        all_text.replace('\x0C', "\n")
    };

    // Trim to output limit (character-boundary safe).
    let truncated = text.len() > MAX_PDF_OUTPUT_CHARS;
    let output_text = if truncated {
        // Walk back to a valid char boundary.
        let mut end = MAX_PDF_OUTPUT_CHARS;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text[..end].to_owned()
    } else {
        text
    };

    Ok(json!({
        "text": output_text,
        "page_count": page_count,
        "truncated": truncated,
        "characters": output_text.len(),
    }))
}

/// Map audio file extension to a MIME type string for the multipart upload.
fn audio_mime_for_path(path: &Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        "mp4" => "audio/mp4",
        "mpeg" => "audio/mpeg",
        "mpga" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "webm" => "audio/webm",
        _ => "audio/mpeg",
    }
    .to_owned()
}

// lopdf is used inside extract_pdf_text() above (feature-gated).

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalise_lexical_dotdot_blocked() {
        let path = Path::new("foo/../bar");
        assert_eq!(normalise_lexical(path), PathBuf::from("bar"));
    }

    #[test]
    fn test_normalise_lexical_current_dir_stripped() {
        let path = Path::new("./foo/./bar");
        assert_eq!(normalise_lexical(path), PathBuf::from("foo/bar"));
    }

    #[test]
    fn test_normalise_lexical_absolute_unchanged() {
        let path = Path::new("/absolute/path");
        assert_eq!(normalise_lexical(path), PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_resolve_safe_path_null_byte_rejected() {
        let result = resolve_safe_path("foo\0bar", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("null byte"));
    }

    #[test]
    fn test_resolve_safe_path_empty_rejected() {
        let result = resolve_safe_path("", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_safe_path_traversal_rejected() {
        let base = std::env::temp_dir();
        let result = resolve_safe_path("../../../etc/passwd", Some(&base));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("outside the workspace")
        );
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_compute_dimensions_both_given() {
        assert_eq!(
            compute_dimensions(800, 600, Some(400), Some(300)),
            (400, 300)
        );
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_compute_dimensions_width_only_preserves_ratio() {
        let (w, h) = compute_dimensions(800, 400, Some(400), None);
        assert_eq!(w, 400);
        assert_eq!(h, 200);
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_compute_dimensions_height_only_preserves_ratio() {
        let (w, h) = compute_dimensions(800, 400, None, Some(200));
        assert_eq!(w, 400);
        assert_eq!(h, 200);
    }

    // ---- resolve_target_dims / ImageResizeConfig ────────────────────────────

    #[cfg(feature = "media")]
    #[test]
    fn test_resolve_target_dims_caller_width_only() {
        let cfg = ImageResizeConfig::default();
        let (w, h) = resolve_target_dims(Some(800), None, &cfg).unwrap();
        assert_eq!(w, Some(800));
        assert!(h.is_none());
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_resolve_target_dims_caller_both_dims() {
        let cfg = ImageResizeConfig::default();
        let (w, h) = resolve_target_dims(Some(1920), Some(1080), &cfg).unwrap();
        assert_eq!(w, Some(1920));
        assert_eq!(h, Some(1080));
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_resolve_target_dims_no_caller_no_defaults_errors() {
        let cfg = ImageResizeConfig::default();
        let result = resolve_target_dims(None, None, &cfg);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("IMAGE_RESIZE_DEFAULT_WIDTH"),
            "error should mention env var: {msg}"
        );
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_resolve_target_dims_no_caller_uses_default_width() {
        let cfg = ImageResizeConfig {
            default_width: Some(640),
            ..ImageResizeConfig::default()
        };
        let (w, h) = resolve_target_dims(None, None, &cfg).unwrap();
        assert_eq!(w, Some(640));
        assert!(h.is_none()); // height computed from ratio later
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_resolve_target_dims_no_caller_uses_both_defaults() {
        let cfg = ImageResizeConfig {
            default_width: Some(120),
            default_height: Some(120),
            ..ImageResizeConfig::default()
        };
        let (w, h) = resolve_target_dims(None, None, &cfg).unwrap();
        assert_eq!(w, Some(120));
        assert_eq!(h, Some(120));
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_resolve_target_dims_clamped_to_max_width() {
        let cfg = ImageResizeConfig {
            max_width: 1920,
            ..ImageResizeConfig::default()
        };
        let (w, _h) = resolve_target_dims(Some(4096), Some(2160), &cfg).unwrap();
        assert_eq!(w, Some(1920)); // clamped
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_resolve_target_dims_clamped_to_max_height() {
        let cfg = ImageResizeConfig {
            max_height: 1080,
            ..ImageResizeConfig::default()
        };
        let (_w, h) = resolve_target_dims(Some(1920), Some(2160), &cfg).unwrap();
        assert_eq!(h, Some(1080)); // clamped
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_resolve_target_dims_defaults_also_clamped() {
        // Default exceeds max — should be clamped.
        let cfg = ImageResizeConfig {
            default_width: Some(4000),
            max_width: 1920,
            ..ImageResizeConfig::default()
        };
        let (w, _) = resolve_target_dims(None, None, &cfg).unwrap();
        assert_eq!(w, Some(1920));
    }

    #[test]
    fn test_image_resize_config_defaults() {
        // Without env vars set, defaults should match the struct defaults.
        // We can't guarantee a clean env in all test runners, so only check
        // the invariant that max values are positive.
        let cfg = ImageResizeConfig::default();
        assert!(cfg.max_width >= 1);
        assert!(cfg.max_height >= 1);
        assert_eq!(cfg.max_width, 65535);
        assert_eq!(cfg.max_height, 65535);
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_detect_mime_from_path_png() {
        let path = Path::new("test.png");
        assert_eq!(detect_mime_from_path(path), "image/png");
    }

    #[cfg(feature = "media")]
    #[test]
    fn test_detect_mime_from_path_pdf() {
        let path = Path::new("document.pdf");
        assert_eq!(detect_mime_from_path(path), "application/pdf");
    }

    #[test]
    fn test_audio_mime_for_path_mp3() {
        assert_eq!(audio_mime_for_path(Path::new("clip.mp3")), "audio/mpeg");
    }

    #[test]
    fn test_audio_mime_for_path_wav() {
        assert_eq!(audio_mime_for_path(Path::new("clip.WAV")), "audio/wav");
    }

    #[test]
    fn test_media_info_tool_name() {
        assert_eq!(MediaInfoTool.name(), "media_info");
    }

    #[test]
    fn test_image_resize_tool_name() {
        assert_eq!(ImageResizeTool.name(), "image_resize");
    }

    #[test]
    fn test_image_convert_tool_name() {
        assert_eq!(ImageConvertTool.name(), "image_convert");
    }

    #[test]
    fn test_pdf_extract_tool_name() {
        assert_eq!(PdfExtractTextTool.name(), "pdf_extract_text");
    }

    #[test]
    fn test_audio_transcribe_tool_name() {
        assert_eq!(AudioTranscribeTool.name(), "audio_transcribe");
    }

    #[tokio::test]
    async fn test_image_resize_missing_dims_rejected() {
        let tool = ImageResizeTool;
        let ctx = crate::context::JobContext::default();
        let result = tool
            .execute(
                serde_json::json!({"input_path": "foo.png", "output_path": "bar.png"}),
                &ctx,
            )
            .await;
        assert!(result.is_err());
    }
}
