use anyhow::Result;
use axum::{
    Extension, Router,
    extract::{DefaultBodyLimit, Multipart, Path},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Html,
    routing::{get, post},
    Json,
};
use bytes::Bytes;
use lru::LruCache;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, percent_encode};
use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::DefaultHasher,
    convert::TryInto,
    hash::{Hash, Hasher},
    num::NonZeroUsize,
    path::Path as FsPath,
    path::PathBuf,
    sync::Arc,
    vec,
};
use tower_http::add_extension::AddExtensionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::{TraceLayer, DefaultMakeSpan, DefaultOnResponse};
use uuid::Uuid;
use tracing_subscriber::filter::EnvFilter;
use tracing_appender::non_blocking::WorkerGuard;

use tokio::{net::TcpListener, sync::Mutex};
use tower::ServiceBuilder;
use tracing::{info, instrument, warn};
mod pb;
use pb::*;
mod engine;
use engine::{Photon, Engine};
use image::ImageFormat;

#[derive(Deserialize)]
struct Params {
    spec: String,
    url: String,
}

#[derive(Serialize, Deserialize)]
struct UploadResponse {
    success: bool,
    message: String,
    filename: Option<String>,
}

struct UploadedImage {
    data: Bytes,
    original_filename: Option<String>,
}

// 10MB limit
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

type Cache = Arc<Mutex<LruCache<u64, Bytes>>>;
type UploadsPath = Arc<PathBuf>;

/// Get the absolute path for the uploads directory
fn get_uploads_dir() -> PathBuf {
    let mut path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    path.push("uploads");
    path
}

/// Initialize tracing subscriber with console and file output
fn init_tracing() -> Option<WorkerGuard> {
    use tracing_subscriber::{fmt, prelude::*};
    use std::io;

    // Create logs directory if it doesn't exist
    let _ = std::fs::create_dir_all("logs");

    // Set default log level to INFO if RUST_LOG is not set
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("thumbor=info,axum=info"));

    // File appender for logs
    let file_appender = tracing_appender::rolling::daily("logs", "thumbor.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_thread_ids(true);

    let console_layer = fmt::layer()
        .with_writer(io::stdout)
        .with_ansi(true)
        .with_target(true)
        .with_level(true);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    Some(guard)
}

#[tokio::main]
async fn main() {
    // Initialize logging first
    let _guard = init_tracing();
    
    info!("╔══════════════════════════════════════╗");
    info!("║   Thumbor Image Server Starting     ║");
    info!("╚══════════════════════════════════════╝");
    
    // Get working directory and uploads path
    let work_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    info!("Working directory: {}", work_dir.display());
    
    let uploads_dir = get_uploads_dir();
    info!("Uploads directory: {}", uploads_dir.display());
    
    let cache: Cache = Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(1024).unwrap())));
    let uploads_path: UploadsPath = Arc::new(uploads_dir);
    
    // Create uploads directory if it doesn't exist
    match tokio::fs::create_dir_all(uploads_path.as_ref()).await {
        Ok(_) => info!("✓ Uploads directory ready: {}", uploads_path.display()),
        Err(e) => {
            info!("✗ Failed to create uploads directory: {}", e);
            eprintln!("Failed to create uploads directory: {}", e);
            return;
        }
    }
    
    // Check write permissions
    match tokio::fs::metadata(uploads_path.as_ref()).await {
        Ok(meta) => {
            info!("✓ Uploads directory accessible");
            if meta.permissions().readonly() {
                info!("⚠ WARNING: Uploads directory is read-only!");
            }
        }
        Err(e) => {
            info!("✗ Cannot access uploads directory: {}", e);
        }
    }
    
    let app = Router::new()
        .route("/", get(index))
        .route("/api/process", post(process_upload))
        .route("/api/upload", post(upload_and_save))
        .route("/image/{spec}/{url}", get(generate))
        .layer(DefaultBodyLimit::max((MAX_FILE_SIZE + 1024 * 1024) as usize))
        .layer(
            ServiceBuilder::new()
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
                        .on_response(DefaultOnResponse::new().level(tracing::Level::INFO))
                )
                .layer(CorsLayer::permissive())
                .layer(AddExtensionLayer::new(cache))
                .layer(AddExtensionLayer::new(uploads_path))
                .into_inner(),
    );
    let addr = "0.0.0.0:3000";
    
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => {
            info!("✓ Server binding successful");
            l
        }
        Err(e) => {
            info!("✗ Failed to bind to {}: {}", addr, e);
            eprintln!("Failed to bind to {}: {}", addr, e);
            return;
        }
    };
    
    println!("╔══════════════════════════════════════╗");
    println!("║   🚀 Thumbor Server is Running      ║");
    println!("╠══════════════════════════════════════╣");
    println!("║  Studio: http://0.0.0.0:3000/       ║");
    println!("║  Upload: POST /api/upload            ║");
    println!("║  Process: POST /api/process          ║");
    println!("║  Image: GET /image/{{spec}}/{{url}}   ║");
    println!("║  Logs: ./logs/thumbor.log            ║");
    println!("╚══════════════════════════════════════╝");
    
    print_test_url("https://images.pexels.com/photos/1562477/pexels-photo-1562477.jpeg?auto=compress&cs=tinysrgb&dpr=3&h=750&w=1260");
    
    info!("Server listening on http://{}", addr);
    info!("Log level: {} (set RUST_LOG to change)", std::env::var("RUST_LOG").unwrap_or_else(|_| "thumbor=info".to_string()));
    info!("═══════════════════════════════════════");
    
    if let Err(e) = axum::serve(listener, app).await {
        info!("✗ Server error: {}", e);
        eprintln!("Server error: {}", e);
    }
}

async fn index() -> Html<&'static str> {
    info!("GET / - Serving index page");
    Html(include_str!("../static/index.html"))
}

#[instrument(level = "info", skip(cache))]
async fn generate(
    Path(Params { spec, url }): Path<Params>,
    Extension(cache): Extension<Cache>
) -> Result<(HeaderMap, Vec<u8>), StatusCode> {
    info!("GET /image/{{spec}}/{{url}} - Received image generation request");
    info!("  Spec (encoded): {}", spec);
    info!("  URL (encoded): {}", url);
    
    let url = percent_decode_str(&url).decode_utf8_lossy();
    info!("  URL (decoded): {}", url);
    
    let spec: ImageSpec = spec
        .as_str()
        .try_into()
        .map_err(|e| {
            warn!("Failed to parse image spec: {}", e);
            StatusCode::BAD_REQUEST
        })?;
    info!("  Spec parsed successfully");

    let data = retrieve_image(&url, cache)
        .await
        .map_err(|e| {
            warn!("Failed to retrieve image: {}", e);
            StatusCode::BAD_REQUEST
        })?;
    info!("  Image data retrieved ({} bytes)", data.len());

    let mut engine = Photon::try_from(data).map_err(|e| {
        warn!("Failed to create Photon engine: {}", e);
        StatusCode::BAD_REQUEST
    })?;
    info!("  Photon engine created");
    
    engine.apply(&spec.specs);
    info!("  Image specs applied");
    
    let image = engine.generate(ImageFormat::Jpeg);
    info!("  Image generation completed ({} bytes)", image.len());

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("image/jpeg"));
    info!("  ✓ Image generation successful");
    Ok((headers, image))
}

#[derive(Default)]
struct ProcessOptions {
    resize_width: Option<u32>,
    resize_height: Option<u32>,
    resize_type: Option<String>,
    resize_filter: Option<String>,
    crop_x1: Option<u32>,
    crop_y1: Option<u32>,
    crop_x2: Option<u32>,
    crop_y2: Option<u32>,
    contrast: Option<f32>,
    image_filter: Option<String>,
    fliph: bool,
    flipv: bool,
    watermark: bool,
    watermark_x: Option<u32>,
    watermark_y: Option<u32>,
}

async fn process_upload(
    Extension(uploads_path): Extension<UploadsPath>,
    mut multipart: Multipart,
) -> Result<(HeaderMap, Vec<u8>), StatusCode> {
    info!("POST /api/process - Image processing request received");
    let mut image = None;
    let mut original_filename = None;
    let mut options = ProcessOptions::default();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| {
            warn!("Failed to read multipart field: {}", e);
            StatusCode::BAD_REQUEST
        })?
    {
        let name = field.name().unwrap_or_default().to_owned();
        info!("  Processing field: {}", name);
        
        match name.as_str() {
            "image" => {
                original_filename = field.file_name().map(str::to_owned);
                let data = field.bytes().await.map_err(|e| {
                    warn!("Failed to read image bytes: {}", e);
                    StatusCode::BAD_REQUEST
                })?;
                if !data.is_empty() {
                    info!("  Image field received ({} bytes)", data.len());
                    image = Some(data);
                }
            }
            "resize_width" => options.resize_width = parse_optional_u32(field).await?,
            "resize_height" => options.resize_height = parse_optional_u32(field).await?,
            "resize_type" => options.resize_type = parse_optional_string(field).await?,
            "resize_filter" => options.resize_filter = parse_optional_string(field).await?,
            "crop_x1" => options.crop_x1 = parse_optional_u32(field).await?,
            "crop_y1" => options.crop_y1 = parse_optional_u32(field).await?,
            "crop_x2" => options.crop_x2 = parse_optional_u32(field).await?,
            "crop_y2" => options.crop_y2 = parse_optional_u32(field).await?,
            "contrast" => options.contrast = parse_optional_f32(field).await?,
            "image_filter" => options.image_filter = parse_optional_string(field).await?,
            "fliph" => options.fliph = parse_checkbox(field).await?,
            "flipv" => options.flipv = parse_checkbox(field).await?,
            "watermark" => options.watermark = parse_checkbox(field).await?,
            "watermark_x" => options.watermark_x = parse_optional_u32(field).await?,
            "watermark_y" => options.watermark_y = parse_optional_u32(field).await?,
            _ => {
                info!("  Unknown field ignored: {}", name);
            }
        }
    }

    let image = image.ok_or_else(|| {
        warn!("No image field found in request");
        StatusCode::BAD_REQUEST
    })?;
    info!("Image data prepared for processing");

    let upload_id = Uuid::new_v4();
    let original_extension = detect_upload_extension(original_filename.as_deref(), &image);
    let original_filename = format!("{}_original.{}", upload_id, original_extension);
    let original_filepath = uploads_path.join(&original_filename);

    info!("  Saving original uploaded image:");
    info!("    Filename: {}", original_filename);
    info!("    Path: {}", original_filepath.display());

    if let Err(e) = tokio::fs::write(&original_filepath, &image).await {
        warn!(
            "⚠ Failed to save original uploaded image to {}: {}",
            original_filepath.display(),
            e
        );
    } else {
        info!(
            "  ✓ Original uploaded image saved successfully to: {}",
            original_filepath.display()
        );
    }
    
    let mut engine = Photon::try_from(image).map_err(|e| {
        warn!("Failed to create Photon engine: {}", e);
        StatusCode::BAD_REQUEST
    })?;
    info!("Photon engine created");
    
    let specs = build_specs(&options);
    info!("Processing options applied");
    engine.apply(&specs);

    let result = engine.generate(ImageFormat::Jpeg);
    info!("✓ Image processing completed ({} bytes)", result.len());
    
    // Save the processed image
    let filename = format!("{}_processed.jpg", upload_id);
    let filepath = uploads_path.join(&filename);
    
    info!("  Saving processed image:");
    info!("    Filename: {}", filename);
    info!("    Path: {}", filepath.display());
    
    if let Err(e) = tokio::fs::write(&filepath, &result).await {
        warn!("⚠ Failed to save processed image to {}: {}", filepath.display(), e);
    } else {
        info!("  ✓ Processed image saved successfully to: {}", filepath.display());
    }

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("image/jpeg"));
    Ok((headers, result))
}

#[instrument(level = "info")]
async fn upload_and_save(
    Extension(uploads_path): Extension<UploadsPath>,
    mut multipart: Multipart,
) -> Result<Json<UploadResponse>, StatusCode> {
    info!("POST /api/upload - Image upload request received");
    info!("  Uploads directory: {}", uploads_path.display());
    
    // Parse multipart form with better error handling
    match parse_multipart_field(&mut multipart).await {
        Ok(Some(uploaded)) => {
            let UploadedImage {
                data,
                original_filename,
            } = uploaded;

            info!("  Received data: {} bytes", data.len());
            
            // Check file size (10MB limit)
            if data.len() > MAX_FILE_SIZE as usize {
                let msg = format!(
                    "File too large: {} bytes (max: {} bytes)", 
                    data.len(), 
                    MAX_FILE_SIZE
                );
                warn!("✗ {}", msg);
                return Ok(Json(UploadResponse {
                    success: false,
                    message: msg,
                    filename: None,
                }));
            }
            info!("  ✓ File size check passed");
            
            if data.is_empty() {
                let msg = "Empty file".to_string();
                warn!("✗ {}", msg);
                return Ok(Json(UploadResponse {
                    success: false,
                    message: msg,
                    filename: None,
                }));
            }
            
            // Try to determine image format, but don't fail if we can't
            if let Ok(format) = image::guess_format(&data) {
                info!("  Image format detected: {:?}", format);
            } else {
                info!("  Could not detect image format, but proceeding with upload");
            }
            
            let extension = detect_upload_extension(original_filename.as_deref(), &data);

            // Generate unique filename
            let uuid = Uuid::new_v4();
            let filename = format!("{}.{}", uuid, extension);
            let filepath = uploads_path.join(&filename);
            
            info!("  Filename: {}", filename);
            info!("  Full path: {}", filepath.display());
            
            // Check if uploads directory exists and is writable
            match tokio::fs::metadata(uploads_path.as_ref()).await {
                Ok(meta) => {
                    info!("  Directory check: exists={}, readonly={}", true, meta.permissions().readonly());
                }
                Err(e) => {
                    warn!("  ⚠ Directory check failed: {}", e);
                }
            }
            
            // Save the file
            match tokio::fs::write(&filepath, &data).await {
                Ok(_) => {
                    info!("  File write completed");
                    
                    // Verify file was actually created
                    match tokio::fs::metadata(&filepath).await {
                        Ok(meta) => {
                            info!("  ✓ File verified: size={} bytes", meta.len());
                            info!("✓ Image saved successfully: {} ({} bytes)", filepath.display(), data.len());
                            Ok(Json(UploadResponse {
                                success: true,
                                message: format!("Image uploaded and saved to: {}", filepath.display()),
                                filename: Some(filename),
                            }))
                        }
                        Err(e) => {
                            warn!("  ✗ File verification failed: {} (created but cannot stat)", e);
                            warn!("✗ Failed to verify saved file: {}", e);
                            Ok(Json(UploadResponse {
                                success: false,
                                message: format!("File created but verification failed: {}", e),
                                filename: None,
                            }))
                        }
                    }
                }
                Err(e) => {
                    warn!("✗ Failed to save file to {}: {} (code: {})", filepath.display(), e, e.kind());
                    Ok(Json(UploadResponse {
                        success: false,
                        message: format!("Failed to save file: {} (kind: {:?})", e, e.kind()),
                        filename: None,
                    }))
                }
            }
        }
        Ok(None) => {
            let msg = "No image field found in request".to_string();
            warn!("✗ {}", msg);
            Ok(Json(UploadResponse {
                success: false,
                message: msg,
                filename: None,
            }))
        }
        Err(e) => {
            warn!("✗ Multipart parsing error: {}", e);
            Ok(Json(UploadResponse {
                success: false,
                message: format!("Invalid request format: {}", e),
                filename: None,
            }))
        }
    }
}

async fn parse_multipart_field(multipart: &mut Multipart) -> Result<Option<UploadedImage>, String> {
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_owned();
                
                if name == "image" {
                    let original_filename = field.file_name().map(str::to_owned);
                    match field.bytes().await {
                        Ok(data) => {
                            return Ok(Some(UploadedImage {
                                data,
                                original_filename,
                            }))
                        }
                        Err(e) => return Err(format!("Failed to read file bytes: {}", e)),
                    }
                }
            }
            Ok(None) => return Ok(None),
            Err(e) => return Err(format!("Multipart parsing failed: {}", e)),
        }
    }
}

fn detect_upload_extension(original_filename: Option<&str>, data: &[u8]) -> String {
    if let Some(filename) = original_filename {
        if let Some(extension) = extract_extension(filename) {
            return extension;
        }
    }

    image::guess_format(data)
        .map(image_format_extension)
        .unwrap_or("bin")
        .to_string()
}

fn extract_extension(filename: &str) -> Option<String> {
    FsPath::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .map(|ext| ext.to_ascii_lowercase())
        .filter(|ext| {
            ext.chars()
                .all(|ch| ch.is_ascii_alphanumeric())
        })
}

fn image_format_extension(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Png => "png",
        ImageFormat::Gif => "gif",
        ImageFormat::WebP => "webp",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        ImageFormat::Tga => "tga",
        ImageFormat::Pnm => "pnm",
        ImageFormat::Dds => "dds",
        ImageFormat::Ico => "ico",
        ImageFormat::Hdr => "hdr",
        ImageFormat::OpenExr => "exr",
        ImageFormat::Farbfeld => "ff",
        ImageFormat::Avif => "avif",
        ImageFormat::Qoi => "qoi",
        _ => "bin",
    }
}

#[instrument(level = "info", skip(cache))]
async fn retrieve_image(url: &str, cache: Cache) -> Result<Bytes> {
    info!("Retrieving image from: {}", url);
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    let hash = hasher.finish();
    info!("Cache hash: {}", hash);

    if let Some(image) = cache.lock().await.get(&hash) {
        info!("✓ Cache hit! ({} bytes)", image.len());
        return Ok(image.clone());
    }

    info!("Cache miss, downloading from remote URL");
    let response = reqwest::get(url).await?;
    info!("Response status: {}", response.status());
    
    let bytes = response.bytes().await?;
    info!("Downloaded: {} bytes", bytes.len());
    
    cache.lock().await.put(hash, bytes.clone());
    info!("✓ Image cached successfully");
    Ok(bytes)
}

fn print_test_url(url: &str) {
    use std::borrow::Borrow;
    let spec1 = Spec::new_resize(500, 800, resize::SampleFilter::CatmullRom);
    let spec2 = Spec::new_watermark(20, 20);
    let spec3 = Spec::new_filter(filter::Filter::Marine);
    let image_spec = ImageSpec::new(vec![spec1, spec2, spec3]);
    let spec_str: String = image_spec.borrow().into();
    let test_image = percent_encode(url.as_bytes(), NON_ALPHANUMERIC).to_string();
    println!(
        "test url: http://localhost:3000/image/{}/{}",
        spec_str, test_image
    );
}

async fn parse_optional_string(field: axum::extract::multipart::Field<'_>) -> Result<Option<String>, StatusCode> {
    let text = field.text().await.map_err(|_| StatusCode::BAD_REQUEST)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_owned()))
    }
}

async fn parse_optional_u32(field: axum::extract::multipart::Field<'_>) -> Result<Option<u32>, StatusCode> {
    match parse_optional_string(field).await? {
        Some(value) => value
            .parse::<u32>()
            .map(Some)
            .map_err(|_| StatusCode::BAD_REQUEST),
        None => Ok(None),
    }
}

async fn parse_optional_f32(field: axum::extract::multipart::Field<'_>) -> Result<Option<f32>, StatusCode> {
    match parse_optional_string(field).await? {
        Some(value) => value
            .parse::<f32>()
            .map(Some)
            .map_err(|_| StatusCode::BAD_REQUEST),
        None => Ok(None),
    }
}

async fn parse_checkbox(field: axum::extract::multipart::Field<'_>) -> Result<bool, StatusCode> {
    Ok(parse_optional_string(field)
        .await?
        .map(|value| matches!(value.as_str(), "true" | "1" | "on" | "yes"))
        .unwrap_or(false))
}

fn build_specs(options: &ProcessOptions) -> Vec<Spec> {
    let mut specs = Vec::new();

    if let (Some(width), Some(height)) = (options.resize_width, options.resize_height) {
        let rtype = match options.resize_type.as_deref() {
            Some("seam_carve") => resize::ResizeType::SeamCarve,
            _ => resize::ResizeType::Normal,
        };
        let filter = match options.resize_filter.as_deref() {
            Some("nearest") => resize::SampleFilter::Nearest,
            Some("triangle") => resize::SampleFilter::Triangle,
            Some("gaussian") => resize::SampleFilter::Gaussian,
            Some("lanczos3") => resize::SampleFilter::Lanczos3,
            _ => resize::SampleFilter::CatmullRom,
        };
        specs.push(Spec {
            data: Some(spec::Data::Resize(Resize {
                width,
                height,
                rtype: rtype as i32,
                filter: filter as i32,
            })),
        });
    }

    if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
        options.crop_x1,
        options.crop_y1,
        options.crop_x2,
        options.crop_y2,
    ) {
        specs.push(Spec {
            data: Some(spec::Data::Crop(Crop { x1, y1, x2, y2 })),
        });
    }

    if let Some(contrast) = options.contrast {
        if contrast.abs() > f32::EPSILON {
            specs.push(Spec {
                data: Some(spec::Data::Contrast(Contrast { contrast })),
            });
        }
    }

    if let Some(filter) = options.image_filter.as_deref() {
        let filter = match filter {
            "oceanic" => filter::Filter::Oceanic,
            "islands" => filter::Filter::Islands,
            "marine" => filter::Filter::Marine,
            _ => filter::Filter::Unspecified,
        };
        if filter != filter::Filter::Unspecified {
            specs.push(Spec::new_filter(filter));
        }
    }

    if options.fliph {
        specs.push(Spec {
            data: Some(spec::Data::Fliph(Fliph {})),
        });
    }

    if options.flipv {
        specs.push(Spec {
            data: Some(spec::Data::Flipv(Flipv {})),
        });
    }

    if options.watermark {
        specs.push(Spec::new_watermark(
            options.watermark_x.unwrap_or(20),
            options.watermark_y.unwrap_or(20),
        ));
    }

    specs
}
