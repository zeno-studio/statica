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
use tower_http::compression::CompressionLayer; // 新增：支持 gzip 压缩
use arc_swap::ArcSwap;
use std::collections::HashMap;
use tokio::sync::Mutex;
use zip::ZipArchive;
use std::io::Cursor;
use tracing::{info, error, Level};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};
use tracing_appender::rolling; // 新增：日志轮转
use serde::{Serialize, Deserialize};
use std::fs::{File, OpenOptions};
use std::io::Write;


// 定义应用状态
#[derive(Debug, Clone)]
struct AppState {
    files: Arc<ArcSwap<HashMap<String, (Vec<u8>, String)>>>,
    last_updated: Arc<Mutex<i64>>,
    update_log: Arc<Mutex<Vec<UpdateLogEntry>>>,
    api_key: String,
    index_visits: Arc<Mutex<u64>>,
    total_visits: Arc<Mutex<u64>>,
    last_reset: Arc<Mutex<DateTime<Local>>>,
    stats_file: Arc<Mutex<File>>, // 持久化统计文件
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
        // 初始化统计文件
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
        )
            .into_response()
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

fn extract_zip(zip_data: &[u8]) -> Result<HashMap<String, (Vec<u8>, String)>, anyhow::Error> {
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
            files.insert(path.clone(), (content, mime));
            info!(path = %path, "Extracted file");
        }
    }
    info!(file_count = files.len(), "Extracted files from ZIP");
    Ok(files)
}

#[derive(Deserialize)]
struct UpdateRequest {
    cid: String,
}   

// API Key 验证中间件
async fn api_key_middleware(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
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
                .body(axum::body::Body::from("Invalid or missing API Key"))
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
    let mut last_error = None;

    info!(cid = %cid, "Starting update");
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
                            last_error = Some("New files missing index.html".to_string());
                            error!("Update failed: missing index.html");
                            continue;
                        }

                        state.files.store(Arc::new(new_files));
                        let mut last_updated = state.last_updated.lock().await;
                        *last_updated = Utc::now().timestamp();

                        let mut log = state.update_log.lock().await;
                        log.push(UpdateLogEntry {
                            timestamp: Utc::now(),
                            cid: cid.clone(),
                            success: true,
                            message: format!("Files updated successfully in {:.2}s", start_time.elapsed().as_secs_f64()),
                        });
                        info!(duration = start_time.elapsed().as_secs_f64(), "Update completed");
                        return Ok((StatusCode::OK, "Files updated successfully"));
                    }
                    Err(e) => {
                        last_error = Some(format!("Failed to extract ZIP: {}", e));
                        error!(error = %e, "Failed to extract ZIP");
                    }
                }
            }
            Err(e) => {
                last_error = Some(format!("Failed to fetch ZIP: {}", e));
                error!(error = %e, "Failed to fetch ZIP");
            }
        }

        if attempt < max_attempts {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    }

    let mut log = state.update_log.lock().await;
    log.push(UpdateLogEntry {
        timestamp: Utc::now(),
        cid,
        success: false,
        message: last_error.unwrap_or("Unknown error".to_string()),
    });
    error!(attempts = max_attempts, "Update failed");

    Err(AppError(anyhow::anyhow!("Failed to update files after {} attempts", max_attempts)))
}

async fn get_update_log(State(state): State<AppState>) -> impl IntoResponse {
    let log = state.update_log.lock().await;
    let index_visits = *state.index_visits.lock().await;
    let total_visits = *state.total_visits.lock().await;
    let last_reset = *state.last_reset.lock().await;
    info!(entry_count = log.len(), index_visits = index_visits, total_visits = total_visits, "Serving update log");

    // 持久化当天统计数据
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
    _request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    // 增加总访问计数
    let mut total_visits = state.total_visits.lock().await;
    *total_visits += 1;

    // 如果是 index.html，增加 index 访问计数
    if path == "index.html" {
        let mut index_visits = state.index_visits.lock().await;
        *index_visits += 1;
    }

    // 检查是否需要重置计数
    let mut last_reset = state.last_reset.lock().await;
    let now = Local::now();
    if now.date_naive() != last_reset.date_naive() {
        // 持久化前一天的统计数据
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

        // 重置计数
        *state.index_visits.lock().await = 0;
        *state.total_visits.lock().await = 0;
        *last_reset = now;
        info!(date = %now.date_naive(), "Reset daily visit counts");
    }

    let files = state.files.load();
    match files.get(&path) {
        Some((content, mime)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_str())
            .body(axum::body::Body::from(content.clone()))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(axum::body::Body::from("File not found"))
            .unwrap(),
    }
}

// 健康检查路由
async fn health_check() -> impl IntoResponse {
    info!("Health check requested");
    Json(serde_json::json!({
        "status": "healthy",
        "timestamp": Utc::now().to_rfc3339(),
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 初始化 tracing，输出 JSON 格式日志，带轮转
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
        .route("/health", get(health_check)) // 新增健康检查路由
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=3600"),
        ))
        .layer(CompressionLayer::new()) // 新增 gzip 压缩
        .with_state(state);

    let cert_path_str = env::var("TLS_CERT_PATH").unwrap_or("./cert.pem".to_string());
    let key_path_str = env::var("TLS_KEY_PATH").unwrap_or("./key.pem".to_string());

    let cert_path = Path::new(&cert_path_str);
    let key_path = Path::new(&key_path_str);

    if cert_path.is_file() && key_path.is_file() {
        match RustlsConfig::from_pem_file(cert_path, key_path).await {
            Ok(tls_config) => {
                info!("Starting HTTPS server on 0.0.0.0:8443");
                axum_server::bind_rustls("0.0.0.0:8443".parse().unwrap(), tls_config)
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