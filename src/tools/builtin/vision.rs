//! Vision tool: multi-modal image analysis via the configured LLM backend.
//!
//! [`ImageAnalyzeTool`] reads one or more local image files, encodes them as
//! base64 data-URIs, and issues a single multi-modal prompt to the configured
//! [`LlmProvider`].  Only image MIME types are accepted; other files are
//! rejected with a descriptive error before any network call is made.
//!
//! ## Security
//!
//! * All paths are validated against the workspace root (same sandbox as the
//!   media tools) using [`crate::tools::builtin::media::resolve_safe_path`].
//! * Files larger than [`MAX_IMAGE_BYTES`] are rejected before encoding.
//! * At most [`MAX_IMAGES`] images per call to bound request size.

#[cfg(feature = "media")]
use std::sync::Arc;

#[cfg(feature = "media")]
use async_trait::async_trait;

#[cfg(feature = "media")]
use crate::{
    context::JobContext,
    llm::{ChatMessage, CompletionRequest, ContentPart, LlmProvider},
    tools::tool::{Tool, ToolError, ToolOutput, ToolRateLimitConfig},
};

#[cfg(feature = "media")]
use crate::tools::builtin::media::{detect_mime, resolve_safe_path, workspace_base};

// ── Limits ───────────────────────────────────────────────────────────────────

/// Maximum number of images per `image_analyze` call.
const MAX_IMAGES: usize = 10;

/// Maximum bytes per image (20 MiB). Larger files are rejected before encoding.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

// ── Tool ─────────────────────────────────────────────────────────────────────

/// Analyse one or more images with the configured LLM.
///
/// Supply a `question` and up to [`MAX_IMAGES`] local image `paths`.  The tool
/// encodes each image as a base64 data-URI and forwards them together with the
/// question to the LLM\'s vision endpoint.  The LLM\'s natural-language reply is
/// returned as the tool output.
///
/// **Supported formats**: any MIME type that begins with `image/` (JPEG, PNG,
/// WebP, GIF, BMP, ...).  Non-image files are rejected.
///
/// **Requires** an LLM backend that supports multi-modal inputs (e.g. NEAR AI
/// with a vision-capable model such as `claude-3-5-sonnet-20241022`).
#[cfg(feature = "media")]
pub struct ImageAnalyzeTool {
    llm: Arc<dyn LlmProvider + Send + Sync>,
}

#[cfg(feature = "media")]
impl ImageAnalyzeTool {
    /// Create a new [`ImageAnalyzeTool`] backed by `llm`.
    pub fn new(llm: Arc<dyn LlmProvider + Send + Sync>) -> Self {
        Self { llm }
    }

    fn invalid(reason: impl Into<String>) -> ToolError {
        ToolError::InvalidParameters(reason.into())
    }

    fn failed(reason: impl Into<String>) -> ToolError {
        ToolError::ExecutionFailed(reason.into())
    }
}

#[cfg(feature = "media")]
#[async_trait]
impl Tool for ImageAnalyzeTool {
    fn name(&self) -> &str {
        "image_analyze"
    }

    fn description(&self) -> &str {
        "Analyse one or more local images using the LLM\'s vision capability. \
         Provide a list of image file paths and a natural-language question. \
         The LLM will examine the images and answer in plain text. \
         Supports JPEG, PNG, WebP, GIF, BMP and other common raster formats. \
         Maximum 10 images per call; each image must be <= 20 MiB."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "maxItems": 10,
                    "description": "Absolute or workspace-relative paths to the image files to analyse."
                },
                "question": {
                    "type": "string",
                    "description": "The question or instruction to ask the vision model about the images."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override. When omitted the provider default is used."
                }
            },
            "required": ["paths", "question"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        // ── Parse & validate parameters ──────────────────────────────────────

        let paths_val = params
            .get("paths")
            .ok_or_else(|| Self::invalid(r#"missing required parameter "paths""#))?;

        let paths: Vec<String> = serde_json::from_value(paths_val.clone())
            .map_err(|e| Self::invalid(format!(r#""paths" must be an array of strings: {e}"#)))?;

        if paths.is_empty() {
            return Err(Self::invalid(
                r#""paths" must contain at least one image path"#,
            ));
        }
        if paths.len() > MAX_IMAGES {
            return Err(Self::invalid(format!(
                "at most {MAX_IMAGES} images per call (got {})",
                paths.len()
            )));
        }

        let question = params
            .get("question")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Self::invalid(r#"missing required parameter "question""#))?
            .to_string();

        if question.trim().is_empty() {
            return Err(Self::invalid(r#""question" must not be empty"#));
        }

        let model_override: Option<String> = params
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // ── Load, validate and encode images ─────────────────────────────────

        let base = workspace_base()?;
        let mut image_parts: Vec<ContentPart> = Vec::with_capacity(paths.len());

        for path_str in &paths {
            // Validate path is within workspace (same rules as the media tools).
            let abs_path = resolve_safe_path(path_str, base.as_deref())?;

            // Read file bytes asynchronously.
            let raw = tokio::fs::read(&abs_path).await.map_err(|e| {
                Self::failed(format!("failed to read \'{}\': {e}", abs_path.display()))
            })?;

            // Reject oversized files before encoding.
            if raw.len() > MAX_IMAGE_BYTES {
                return Err(Self::invalid(format!(
                    "\'{}\' is {:.1} MiB; maximum allowed image size is {} MiB",
                    abs_path.display(),
                    raw.len() as f64 / (1024.0 * 1024.0),
                    MAX_IMAGE_BYTES / (1024 * 1024),
                )));
            }

            // Detect MIME type -- reject non-image files early.
            let mime = detect_mime(&raw, &abs_path);
            if !mime.starts_with("image/") {
                return Err(Self::invalid(format!(
                    "\'{}\' has MIME type \'{}\'; only image/* files are supported",
                    abs_path.display(),
                    mime,
                )));
            }

            // Base64-encode on a blocking thread (CPU-bound for large images).
            let (mime_owned, b64) = tokio::task::spawn_blocking(move || {
                let b64 = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &raw,
                );
                (mime, b64)
            })
            .await
            .map_err(|e| Self::failed(format!("base64 encoding task panicked: {e}")))?;

            image_parts.push(ContentPart::ImageUrl {
                image_url: crate::llm::ImageUrl {
                    url: format!("data:{mime_owned};base64,{b64}"),
                    detail: Some("auto".to_string()),
                },
            });
        }

        // ── Build multi-modal prompt ──────────────────────────────────────────

        let msg = ChatMessage::user_multimodal(question, image_parts);
        let request = CompletionRequest {
            messages: vec![msg],
            model: model_override,
            max_tokens: Some(4096),
            temperature: Some(0.2),
            stop_sequences: None,
            metadata: std::collections::HashMap::new(),
        };

        // ── Call LLM ─────────────────────────────────────────────────────────

        let response = self
            .llm
            .complete(request)
            .await
            .map_err(|e| Self::failed(format!("LLM vision call failed: {e}")))?;

        Ok(ToolOutput::text(response.content, start.elapsed()))
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        // Vision calls are expensive; keep limits conservative.
        Some(ToolRateLimitConfig::new(5, 20))
    }

    fn requires_sanitization(&self) -> bool {
        // LLM output describing image content may contain attacker-controlled
        // text (e.g. prompt injection embedded in images).
        true
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "media"))]
mod tests {
    use super::*;
    use crate::error::LlmError;
    use crate::llm::{
        CompletionResponse, FinishReason, ToolCompletionRequest, ToolCompletionResponse,
    };
    use async_trait::async_trait;
    use rust_decimal::Decimal;

    // ── Mock LLM ─────────────────────────────────────────────────────────────

    struct MockVisionLlm {
        response: String,
    }

    #[async_trait]
    impl LlmProvider for MockVisionLlm {
        fn model_name(&self) -> &str {
            "mock-vision"
        }

        fn cost_per_token(&self) -> (Decimal, Decimal) {
            (Decimal::ZERO, Decimal::ZERO)
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: self.response.clone(),
                input_tokens: 10,
                output_tokens: 20,
                finish_reason: FinishReason::Stop,
            })
        }

        async fn complete_with_tools(
            &self,
            _request: ToolCompletionRequest,
        ) -> Result<ToolCompletionResponse, LlmError> {
            Err(LlmError::RequestFailed {
                provider: "mock".into(),
                reason: "vision mock does not support tool calls".into(),
            })
        }
    }

    // ── Workspace helper for tests ────────────────────────────────────────────

    /// Pin the workspace to `/tmp` for all file-I/O tests.
    ///
    /// Setting the env var to the same idempotent value is safe even under
    /// concurrent test execution.  We intentionally do NOT unset it after each
    /// test — if another test sees WORKSPACE_DIR=/tmp it is still correct.
    fn use_tmp_workspace() {
        // SAFETY: The value written is always "/tmp" (idempotent).  Concurrent
        // calls to set the same env var to the same value are safe.
        unsafe { std::env::set_var("WORKSPACE_DIR", "/tmp") };
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_tool(response: &str) -> ImageAnalyzeTool {
        ImageAnalyzeTool::new(Arc::new(MockVisionLlm {
            response: response.to_string(),
        }))
    }

    fn make_context() -> JobContext {
        JobContext::default()
    }

    // ── Static property tests ─────────────────────────────────────────────────

    #[test]
    fn test_tool_name() {
        assert_eq!(make_tool("ok").name(), "image_analyze");
    }

    #[test]
    fn test_requires_sanitization() {
        assert!(make_tool("ok").requires_sanitization());
    }

    #[test]
    fn test_rate_limit_config() {
        let rl = make_tool("ok")
            .rate_limit_config()
            .expect("rate limit config should be Some");
        assert_eq!(rl.requests_per_minute, 5);
        assert_eq!(rl.requests_per_hour, 20);
    }

    #[test]
    fn test_parameters_schema_required_fields() {
        let schema = make_tool("ok").parameters_schema();
        let required = schema["required"].as_array().unwrap();
        let keys: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(keys.contains(&"paths"));
        assert!(keys.contains(&"question"));
    }

    // ── Validation tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_missing_paths_returns_invalid_input() {
        let err = make_tool("ok")
            .execute(
                serde_json::json!({"question": "describe"}),
                &make_context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        assert!(err.to_string().contains("paths"));
    }

    #[tokio::test]
    async fn test_missing_question_returns_invalid_input() {
        let err = make_tool("ok")
            .execute(
                serde_json::json!({"paths": ["/some/img.png"]}),
                &make_context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        assert!(err.to_string().contains("question"));
    }

    #[tokio::test]
    async fn test_empty_paths_array_rejected() {
        let err = make_tool("ok")
            .execute(
                serde_json::json!({"paths": [], "question": "describe"}),
                &make_context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
    }

    #[tokio::test]
    async fn test_too_many_paths_rejected() {
        let paths: Vec<String> = (0..=MAX_IMAGES).map(|i| format!("/tmp/i{i}.png")).collect();
        let err = make_tool("ok")
            .execute(
                serde_json::json!({"paths": paths, "question": "describe"}),
                &make_context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        assert!(err.to_string().contains("10"));
    }

    #[tokio::test]
    async fn test_blank_question_rejected() {
        let err = make_tool("ok")
            .execute(
                serde_json::json!({"paths": ["/tmp/x.png"], "question": "  "}),
                &make_context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
    }

    #[tokio::test]
    async fn test_path_traversal_rejected() {
        use_tmp_workspace();

        // /etc/passwd is outside /tmp -> workspace containment rejected.
        let err = make_tool("ok")
            .execute(
                serde_json::json!({"paths": ["/etc/passwd"], "question": "contents?"}),
                &make_context(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, ToolError::NotAuthorized(_)),
            "expected NotAuthorized for out-of-workspace path, got: {msg}"
        );
        assert!(
            msg.contains("outside the workspace"),
            "error should say 'outside the workspace': {msg}"
        );
    }

    #[tokio::test]
    async fn test_non_image_file_rejected() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        use_tmp_workspace();

        let txt = dir.path().join("doc.txt");
        let mut f = std::fs::File::create(&txt).unwrap();
        f.write_all(b"hello world plain text").unwrap();
        drop(f);

        let err = make_tool("ok")
            .execute(
                serde_json::json!({"paths": [txt.to_str().unwrap()], "question": "?"}),
                &make_context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParameters(_)),
            "expected InvalidParameters for txt file, got: {err}"
        );
        assert!(
            err.to_string().contains("image/"),
            "error should mention image/ MIME requirement: {err}"
        );
    }

    #[tokio::test]
    async fn test_llm_response_forwarded() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        use_tmp_workspace();
        let png = dir.path().join("px.png");

        // Minimal valid 1x1 white PNG (hand-crafted, well-formed).
        let png_bytes: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // PNG magic
            0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, // IHDR chunk header
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // width=1, height=1
            0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, // bit-depth/color/CRC
            0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, // IDAT chunk header
            0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00, // compressed pixel
            0x00, 0x00, 0x02, 0x00, 0x01, 0xe2, 0x21, 0xbc, // IDAT CRC
            0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, // IEND chunk header
            0x44, 0xae, 0x42, 0x60, 0x82, // IEND CRC
        ];

        let mut f = std::fs::File::create(&png).unwrap();
        f.write_all(png_bytes).unwrap();
        drop(f);

        let output = make_tool("A white pixel.")
            .execute(
                serde_json::json!({
                    "paths": [png.to_str().unwrap()],
                    "question": "What colour is this pixel?"
                }),
                &make_context(),
            )
            .await
            .unwrap();

        assert_eq!(output.result.as_str().unwrap(), "A white pixel.");
    }
}
