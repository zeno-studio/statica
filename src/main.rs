use axum::{
    Router,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use axum_server::tls_rustls::RustlsConfig;
use chrono::Utc;
use dotenv::dotenv;
use reqwest::Client;
use std::env;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tower_http::set_header::SetResponseHeaderLayer;
mod ar;
use ar::fetch_arweave_folder;

mod appstate;
use appstate::AppState;

#[derive(Debug)]
struct AppError(anyhow::Error);

// Implement Send and Sync for AppError since anyhow::Error is Send + Sync
unsafe impl Send for AppError {}
unsafe impl Sync for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Update failed: {:?}", self.0),
        )
            .into_response()
    }
}

async fn update_files(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let client = Client::new();
    let manifest_txid = "YOUR_ARWEAVE_MANIFEST_TXID"; // Replace with your manifest txid

    match fetch_arweave_folder(&client, manifest_txid).await {
        Ok(new_files) => {
            // Update files in memory
            state.files.store(Arc::new(new_files));
            // Update timestamp
            let mut last_updated = state.last_updated.lock().await;
            *last_updated = Utc::now().timestamp();
            Ok((StatusCode::OK, "Files updated successfully"))
        }
        Err(e) => {
            eprintln!("Failed to fetch files: {:?}", e);
            Err(AppError(anyhow::anyhow!("Failed to fetch files: {}", e)))
        }
    }
}

// Handler to serve files from memory



async fn serve_file(
    axum::extract::Path(path): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
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

// Handler to update files in memory (called monthly)

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize in-memory storage
    dotenv().ok(); // 加载环境变量

    let state = AppState::new();

    let client = Client::new();
    let manifest_txid = "YOUR_ARWEAVE_MANIFEST_TXID"; // Replace with your manifest txid
    let files = fetch_arweave_folder(&client, manifest_txid).await?;

    state.files.store(Arc::new(files));

    // Set up Axum router
    let app = Router::new()
        .route(
            "/",
            get(|state: State<AppState>| {
                serve_file(axum::extract::Path("index.html".to_string()), state)
            }),
        )
        .route("/{path}", get(serve_file))
        .route("/update", get(update_files))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=3600"),
        ))
        .with_state(state);

    let cert_path_str = env::var("TLS_CERT_PATH").unwrap_or("./cert.pem".to_string());
    let key_path_str = env::var("TLS_KEY_PATH").unwrap_or("./key.pem".to_string());

    let cert_path = Path::new(&cert_path_str);
    let key_path = Path::new(&key_path_str);

    if cert_path.is_file() && key_path.is_file() {
        match std::fs::metadata(cert_path).and_then(|_| std::fs::metadata(key_path)) {
            Ok(_) => match RustlsConfig::from_pem_file(cert_path, key_path).await {
                Ok(tls_config) => {
                    println!("Current user: {:?}", std::env::var("USER"));
                    println!("TLS certificates loaded successfully:");
                    println!("  Certificate: {}", cert_path.display());
                    println!("  Private Key: {}", key_path.display());
                    println!("Server running on https://0.0.0.0:8443");

                    axum_server::bind_rustls("0.0.0.0:8443".parse().unwrap(), tls_config)
                        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                        .await
                        .unwrap();
                }
                Err(e) => {
                    println!("Failed to load TLS certificates: {}", e);
                    println!("Falling back to HTTP server on http://0.0.0.0:3000...");
                    start_http_server(app).await;
                }
            },
            Err(e) => {
                println!("Cannot access certificate or key files: {}", e);
                println!("  Certificate: {}", cert_path.display());
                println!("  Private Key: {}", key_path.display());
                println!("Falling back to HTTP server on http://0.0.0.0:3000...");
                start_http_server(app).await;
            }
        }
    } else {
        println!("Certificate or key file not found:");
        if !cert_path.is_file() {
            println!("  Certificate: {} (not a file)", cert_path.display());
        }
        if !key_path.is_file() {
            println!("  Private Key: {} (not a file)", key_path.display());
        }
        println!("Falling back to HTTP server on http://0.0.0.0:3000...");
        start_http_server(app).await;
    }

    async fn start_http_server(app: Router) {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
        println!("Server running on http://0.0.0.0:3000");
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    }
    Ok(())
}
