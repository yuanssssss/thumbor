use anyhow::Result;
use axum::{
    Extension, Router,
    extract::{Multipart, Path},
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
    sync::Arc,
    vec,
};
use tower_http::add_extension::AddExtensionLayer;
use uuid::Uuid;

use tokio::{net::TcpListener, sync::Mutex};
use tower::ServiceBuilder;
use tracing::{info, instrument};
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

// 10MB limit
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

type Cache = Arc<Mutex<LruCache<u64, Bytes>>>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let cache: Cache = Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(1024).unwrap())));
    
    // Create uploads directory if it doesn't exist
    let uploads_dir = "uploads";
    tokio::fs::create_dir_all(uploads_dir)
        .await
        .expect("Failed to create uploads directory");
    info!("Uploads directory created/verified at: {}", uploads_dir);
    
    let app = Router::new()
        .route("/", get(index))
        .route("/api/process", post(process_upload))
        .route("/api/upload", post(upload_and_save))
        .route("/image/{spec}/{url}", get(generate))
        .layer(
            ServiceBuilder::new()
                .layer(AddExtensionLayer::new(cache))
                .into_inner(),
    );
    let addr = "127.0.0.1:3000";
    let listener = TcpListener::bind(addr).await.unwrap();
    println!("studio: http://{addr}/");
    print_test_url("https://images.pexels.com/photos/1562477/pexels-photo-1562477.jpeg?auto=compress&cs=tinysrgb&dpr=3&h=750&w=1260");
    info!("listening on {}", addr);
    axum::serve(listener, app).await.unwrap();
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn generate(Path(Params { spec, url }): Path<Params>,
    Extension(cache): Extension<Cache>
) -> Result<(HeaderMap, Vec<u8>), StatusCode> {
    let url = percent_decode_str(&url).decode_utf8_lossy();
    let spec: ImageSpec = spec
        .as_str()
        .try_into()
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let data = retrieve_image(&url, cache)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let mut engine = Photon::try_from(data).map_err(|_| StatusCode::BAD_REQUEST)?;
    engine.apply(&spec.specs);
    let image = engine.generate(ImageFormat::Jpeg);

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("image/jpeg"));
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

async fn process_upload(mut multipart: Multipart) -> Result<(HeaderMap, Vec<u8>), StatusCode> {
    let mut image = None;
    let mut options = ProcessOptions::default();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
    {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "image" => {
                let data = field.bytes().await.map_err(|_| StatusCode::BAD_REQUEST)?;
                if !data.is_empty() {
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
            _ => {}
        }
    }

    let image = image.ok_or(StatusCode::BAD_REQUEST)?;
    let mut engine = Photon::try_from(image).map_err(|_| StatusCode::BAD_REQUEST)?;
    let specs = build_specs(&options);
    engine.apply(&specs);

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("image/jpeg"));
    Ok((headers, engine.generate(ImageFormat::Jpeg)))
}

#[instrument(level = "info")]
async fn upload_and_save(mut multipart: Multipart) -> Result<Json<UploadResponse>, StatusCode> {
    let uploads_dir = "uploads";
    
    // Parse multipart form
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
    {
        let name = field.name().unwrap_or_default().to_owned();
        
        if name == "image" {
            // Get the file data
            let data = field.bytes().await.map_err(|_| StatusCode::BAD_REQUEST)?;
            
            // Check file size (10MB limit)
            if data.len() > MAX_FILE_SIZE as usize {
                let msg = format!(
                    "File too large: {} bytes (max: {} bytes)", 
                    data.len(), 
                    MAX_FILE_SIZE
                );
                info!("{}", msg);
                return Ok(Json(UploadResponse {
                    success: false,
                    message: msg,
                    filename: None,
                }));
            }
            
            // Validate that it's a valid image
            if data.is_empty() {
                let msg = "Empty file".to_string();
                info!("{}", msg);
                return Ok(Json(UploadResponse {
                    success: false,
                    message: msg,
                    filename: None,
                }));
            }
            
            // Try to determine image format
            let format_result = image::guess_format(&data).map_err(|_| StatusCode::BAD_REQUEST);
            if format_result.is_err() {
                let msg = "Invalid image format".to_string();
                info!("{}", msg);
                return Ok(Json(UploadResponse {
                    success: false,
                    message: msg,
                    filename: None,
                }));
            }
            
            // Generate unique filename
            let uuid = Uuid::new_v4();
            let filename = format!("{}.jpg", uuid);
            let filepath = format!("{}/{}", uploads_dir, filename);
            
            // Save the file
            tokio::fs::write(&filepath, &data)
                .await
                .map_err(|e| {
                    info!("Failed to save file: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
            
            info!(
                "Image saved successfully: {} ({} bytes)",
                filename,
                data.len()
            );
            
            return Ok(Json(UploadResponse {
                success: true,
                message: "Image uploaded and saved successfully".to_string(),
                filename: Some(filename),
            }));
        }
    }
    
    Ok(Json(UploadResponse {
        success: false,
        message: "No image field found in request".to_string(),
        filename: None,
    }))
}

#[instrument(level = "info", skip(cache))]
async fn retrieve_image(url: &str, cache: Cache) -> Result<Bytes> {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    let hash = hasher.finish();

    if let Some(image) = cache.lock().await.get(&hash) {
        info!("cache hit for url: {}", url);
        return Ok(image.clone());
    }

    info!("cache miss for url: {}", url);
    let response = reqwest::get(url).await?;
    let bytes = response.bytes().await?;
    cache.lock().await.put(hash, bytes.clone());
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
