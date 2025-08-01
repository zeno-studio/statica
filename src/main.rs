use axum::{
    Router,
    extract::{State, Json},
    http::{HeaderValue, StatusCode, header, Request},
    response::{IntoResponse, Response},
    routing::{get, post},
    middleware,
};
use axum::body::Body;
use axum_server::tls_rustls::RustlsConfig;
use chrono::{DateTime, Utc, Local, NaiveDate};
use dotenv::dotenv;
use reqwest::Client;
use std::env;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tower_http::set_header::SetResponseHeaderLayer;
use arc_swap::ArcSwap;
use std::collections::HashMap;
use tokio::sync::Mutex;
use zip::ZipArchive;
use std::io::Cursor;
use tracing::{info, error, Level};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};
use tracing_appender::rolling;
use serde::{Serialize, Deserialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use brotli::CompressorWriter;
use flate2::write::GzEncoder;
use flate2::Compression;
use sysinfo::System; // Added for memory logging

// Define application state
#[derive(Debug, Clone)]
struct AppState {
    files: Arc<ArcSwap<HashMap<String, (Vec<u8>, String, Option<Vec<u8>>, Option<Vec<u8>>)>>>,
    last_updated: Arc<Mutex<i64>>,
    update_log: Arc<Mutex<Vec<UpdateLogEntry>>>,
    api_key: String,
    index_visits: Arc<Mutex<u64>>,
    total_visits: Arc<Mutex<u64>>,
    last_reset: Arc<Mutex<DateTime<Local>>>,
    stats_file: Arc<Mutex<File>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateLogEntry {
    timestamp: DateTime<Utc>,
    cid: String,
    success: bool,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VisitStats {
    date: NaiveDate,
    index_visits: u64,
    total_visits: u64,
}

impl AppState {
    fn new(api_key: String) -> Self {
        let stats_file = OpenOptions::new()
            .append(true)
            .create(true)
            .open("/var/log/visit_stats.jsonl")
            .unwrap_or_else(|e| {
                error!(error = %e, "Failed to open visit_stats.jsonl");
                File::create("/tmp/visit_stats.jsonl").expect("Failed to create fallback stats file")
            });

        Self {
            files: Arc::new(ArcSwap::new(Arc::new(HashMap::new()))),
            last_updated: Arc::new(Mutex::new(0)),
            update_log: Arc::new(Mutex::new(Vec::new())),
            api_key,
            index_visits: Arc::new(Mutex::new(0)),
            total_visits: Arc::new(Mutex::new(0)),
            last_reset: Arc::new(Mutex::new(Local::now())),
            stats_file: Arc::new(Mutex::new(stats_file)),
        }
    }
}

#[derive(Debug)]
struct AppError(anyhow::Error);

unsafe impl Send for AppError {}
unsafe impl Sync for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error: {:?}", self.0),
        ).into_response()
    }
}

async fn fetch_zip(client: &Client, cid: &str) -> Result<Vec<u8>, anyhow::Error> {
    let url = format!("https://arweave.net/{}", cid);
    info!(url = %url, "Fetching ZIP from Arweave");
    let response = client.get(&url).send().await?;
    if response.status().is_success() {
        let bytes = response.bytes().await?;
        info!(size = bytes.len(), "Successfully fetched ZIP");
        Ok(bytes.to_vec())
    } else {
        error!(status = %response.status(), "Failed to fetch ZIP");
        Err(anyhow::anyhow!("Failed to fetch ZIP from Arweave: {}", response.status()))
    }
}
fn extract_zip(zip_data: &[u8]) -> Result<HashMap<String, (Vec<u8>, String, Option<Vec<u8>>, Option<Vec<u8>>)>, anyhow::Error> {
    info!(size = zip_data.len(), "Extracting ZIP file");
    let cursor = Cursor::new(zip_data);
    let mut archive = ZipArchive::new(cursor)?;
    let mut files = HashMap::new();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.is_file() {
            let path = file.name().to_string();
            let mut content = Vec::new();
            std::io::copy(&mut file, &mut content)?;
            let mime = match Path::new(&path).extension().and_then(|ext| ext.to_str()) {
                Some("html") => "text/html",
                Some("css") => "text/css",
                Some("js") => "application/javascript",
                Some("png") => "image/png",
                Some("jpg") | Some("jpeg") => "image/jpeg",
                Some("svg") => "image/svg+xml",
                Some("ico") => "image/x-icon",
                _ => "application/octet-stream",
            }.to_string();

            let brotli_content = {
                let mut compressed = Vec::new();
                {
                    let mut writer = CompressorWriter::new(&mut compressed, 4096, 4, 22);
                    writer.write_all(&content)?;
                    writer.flush()?;
                } // writer is dropped here
                Some(compressed)
            };

            let gzip_content = {
                let mut compressed = Vec::new();
                {
                    let mut writer = GzEncoder::new(&mut compressed, Compression::default());
                    writer.write_all(&content)?;
                    writer.finish()?;
                } // writer is dropped here
                Some(compressed)
            };

            files.insert(path.clone(), (content, mime, brotli_content, gzip_content));
            info!(path = %path, "Extracted and pre-compressed file");
        }
    }
    info!(file_count = files.len(), "Extracted files from ZIP");
    Ok(files)
}

#[derive(Deserialize)]
struct UpdateRequest {
    cid: String,
}

async fn api_key_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: middleware::Next,
) -> impl IntoResponse {
    let header_api_key = request.headers().get("X-API-Key").and_then(|v| v.to_str().ok());
    match header_api_key {
        Some(key) if key == state.api_key => {
            info!("API Key validated successfully");
            next.run(request).await
        }
        _ => {
            error!("Invalid or missing API Key");
            Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from("Invalid or missing API Key"))
                .unwrap()
        }
    }
}

async fn update_files(
    State(state): State<AppState>,
    Json(payload): Json<UpdateRequest>,
) -> Result<impl IntoResponse, AppError> {
    let start_time = std::time::Instant::now();
    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .map_err(|e| {
            error!("Failed to create HTTP client: {}", e);
            AppError(anyhow::anyhow!("Failed to create HTTP client: {}", e))
        })?;
    info!("Built reqwest client with rustls TLS");
    let cid = payload.cid;
    let max_attempts = 15;

    // Initialize sysinfo for memory tracking
    let mut system = System::new_all();
    system.refresh_memory();
    let before_mem = system.used_memory();

    info!(cid = %cid, before_mem_kb = before_mem, "Starting update");
    let mut log = state.update_log.lock().await;
    log.push(UpdateLogEntry {
        timestamp: Utc::now(),
        cid: cid.clone(),
        success: false,
        message: "Update started".to_string(),
    });
    drop(log);

    for attempt in 1..=max_attempts {
        info!(attempt = attempt, cid = %cid, "Update attempt");
        match fetch_zip(&client, &cid).await {
            Ok(zip_data) => {
                match extract_zip(&zip_data) {
                    Ok(new_files) => {
                        if !new_files.contains_key("index.html") {
                            let error_msg = "New files missing index.html".to_string();
                            error!("Update failed: {}", error_msg);
                            let mut log = state.update_log.lock().await;
                            log.push(UpdateLogEntry {
                                timestamp: Utc::now(),
                                cid: cid.clone(),
                                success: false,
                                message: error_msg.clone(),
                            });
                            if attempt < max_attempts {
                                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                                continue;
                            }
                            return Err(AppError(anyhow::anyhow!(error_msg)));
                        }

                        // Log memory before storing new files
                        system.refresh_memory();
                        let pre_store_mem = system.used_memory();

                        // Atomically replace old files
                        state.files.store(Arc::new(new_files));

                        // Log memory after storing and wait briefly for GC
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        system.refresh_memory();
                        let after_mem = system.used_memory();

                        let mut last_updated = state.last_updated.lock().await;
                        *last_updated = Utc::now().timestamp();

                        let mut log = state.update_log.lock().await;
                        log.push(UpdateLogEntry {
                            timestamp: Utc::now(),
                            cid: cid.clone(),
                            success: true,
                            message: format!(
                                "Files updated successfully in {:.2}s, memory: {} KB -> {} KB -> {} KB",
                                start_time.elapsed().as_secs_f64(),
                                before_mem,
                                pre_store_mem,
                                after_mem
                            ),
                        });
                        info!(
                            duration = start_time.elapsed().as_secs_f64(),
                            before_mem_kb = before_mem,
                            pre_store_mem_kb = pre_store_mem,
                            after_mem_kb = after_mem,
                            "Update completed"
                        );
                        return Ok((StatusCode::OK, "Files updated successfully"));
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to extract ZIP");
                        let mut log = state.update_log.lock().await;
                        log.push(UpdateLogEntry {
                            timestamp: Utc::now(),
                            cid: cid.clone(),
                            success: false,
                            message: format!("Failed to extract ZIP: {}", e),
                        });
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "Failed to fetch ZIP");
                let mut log = state.update_log.lock().await;
                log.push(UpdateLogEntry {
                    timestamp: Utc::now(),
                    cid: cid.clone(),
                    success: false,
                    message: format!("Failed to fetch ZIP: {}", e),
                });
            }
        }

        if attempt < max_attempts {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    }

    Err(AppError(anyhow::anyhow!("Failed to update files after {} attempts", max_attempts)))
}

async fn get_update_log(State(state): State<AppState>) -> impl IntoResponse {
    let log = state.update_log.lock().await;
    let index_visits = *state.index_visits.lock().await;
    let total_visits = *state.total_visits.lock().await;
    let last_reset = *state.last_reset.lock().await;
    info!(entry_count = log.len(), index_visits = index_visits, total_visits = total_visits, "Serving update log");

    let stats = VisitStats {
        date: last_reset.date_naive(),
        index_visits,
        total_visits,
    };
    let stats_json = serde_json::to_string(&stats).unwrap_or_else(|e| {
        error!(error = %e, "Failed to serialize visit stats");
        "{}".to_string()
    });
    let mut stats_file = state.stats_file.lock().await;
    writeln!(stats_file, "{}", stats_json).unwrap_or_else(|e| {
        error!(error = %e, "Failed to write visit stats");
    });
    stats_file.flush().unwrap_or_else(|e| {
        error!(error = %e, "Failed to flush visit stats");
    });

    Json(serde_json::json!({
        "update_log": &*log,
        "index_visits": index_visits,
        "total_visits": total_visits,
        "last_reset": last_reset.to_rfc3339(),
    }))
}

async fn serve_file(
    axum::extract::Path(path): axum::extract::Path<String>,
    State(state): State<AppState>,
    request: Request<Body>,
) -> impl IntoResponse {
    let mut total_visits = state.total_visits.lock().await;
    *total_visits += 1;
    if path == "index.html" {
        let mut index_visits = state.index_visits.lock().await;
        *index_visits += 1;
    }

    let mut last_reset = state.last_reset.lock().await;
    let now = Local::now();
    if now.date_naive() != last_reset.date_naive() {
        let stats = VisitStats {
            date: last_reset.date_naive(),
            index_visits: *state.index_visits.lock().await,
            total_visits: *state.total_visits.lock().await,
        };
        let stats_json = serde_json::to_string(&stats).unwrap_or_else(|e| {
            error!(error = %e, "Failed to serialize visit stats");
            "{}".to_string()
        });
        let mut stats_file = state.stats_file.lock().await;
        writeln!(stats_file, "{}", stats_json).unwrap_or_else(|e| {
            error!(error = %e, "Failed to write visit stats");
        });
        stats_file.flush().unwrap_or_else(|e| {
            error!(error = %e, "Failed to flush visit stats");
        });

        *state.index_visits.lock().await = 0;
        *state.total_visits.lock().await = 0;
        *last_reset = now;
        info!(date = %now.date_naive(), "Reset daily visit counts");
    }

    let files = state.files.load();
    let accept_encoding = request.headers().get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    match files.get(&path) {
        Some((content, mime, brotli_content, gzip_content)) => {
            if let Some(accept) = accept_encoding {
                if accept.contains("br") && brotli_content.is_some() {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, mime.as_str())
                        .header(header::CONTENT_ENCODING, "br")
                        .body(Body::from(brotli_content.clone().unwrap()))
                        .unwrap();
                } else if accept.contains("gzip") && gzip_content.is_some() {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, mime.as_str())
                        .header(header::CONTENT_ENCODING, "gzip")
                        .body(Body::from(gzip_content.clone().unwrap()))
                        .unwrap();
                }
            }
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_str())
                .body(Body::from(content.clone()))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("File not found"))
            .unwrap(),
    }
}

async fn health_check() -> impl IntoResponse {
    info!("Health check requested");
    Json(serde_json::json!({
        "status": "healthy",
        "timestamp": Utc::now().to_rfc3339(),
    }))
}

async fn redirect_to_https(req: Request<Body>, next: middleware::Next) -> impl IntoResponse {
    if req.uri().scheme() == Some(&http::uri::Scheme::HTTP) {
        let mut uri_parts = req.uri().clone().into_parts();
        uri_parts.scheme = Some(http::uri::Scheme::HTTPS);
        uri_parts.authority = Some(http::uri::Authority::from_static("localhost:8443")); // Replace with your domain
        let redirect_uri = http::Uri::from_parts(uri_parts).unwrap();
        return Response::builder()
            .status(StatusCode::PERMANENT_REDIRECT)
            .header(header::LOCATION, redirect_uri.to_string())
            .body(Body::empty())
            .unwrap();
    }
    next.run(req).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let file_writer = rolling::daily("/var/log", "axum.log");
    tracing_subscriber::registry()
        .with(fmt::layer()
            .json()
            .with_writer(file_writer)
            .with_filter(EnvFilter::from_default_env().add_directive(Level::INFO.into())))
        .init();

    info!("Starting server");
    dotenv().ok();

    let api_key = env::var("API_KEY").expect("API_KEY environment variable must be set");
    let state = AppState::new(api_key);

    let app = Router::new()
        .route(
            "/",
            get(|state: State<AppState>, request| {
                serve_file(axum::extract::Path("index.html".to_string()), state, request)
            }),
        )
        .route("/*path", get(serve_file))
        .route("/update", post(update_files).layer(middleware::from_fn_with_state(state.clone(), api_key_middleware)))
        .route("/update_log", get(get_update_log))
        .route("/health", get(health_check))
        .layer(middleware::from_fn(redirect_to_https)) // Added HTTP-to-HTTPS redirect
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=3600"),
        ))
        .with_state(state);

    let cert_path_str = env::var("TLS_CERT_PATH").unwrap_or("./cert.pem".to_string());
    let key_path_str = env::var("TLS_KEY_PATH").unwrap_or("./key.pem".to_string());
    let cert_path = Path::new(&cert_path_str);
    let key_path = Path::new(&key_path_str);
    let addr = SocketAddr::from(([0, 0, 0, 0], 8443));  

    if cert_path.is_file() && key_path.is_file() {
        match RustlsConfig::from_pem_file(cert_path, key_path).await {
            Ok(tls_config) => {
                info!("Starting HTTPS server on 0.0.0.0:8443");
                axum_server::bind_rustls(addr, tls_config)
                    .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                    .await?;
            }
            Err(e) => {
                error!(error = %e, "Failed to load TLS certificates. Falling back to HTTP.");
                start_http_server(app).await?;
            }
        }
    } else {
        tracing::warn!("Certificate or key file not found. Falling back to HTTP.");
        start_http_server(app).await?;
    }

    Ok(())
}

async fn start_http_server(app: Router) -> Result<(), std::io::Error> {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    info!("Starting HTTP server on 0.0.0.0:3000");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await
}