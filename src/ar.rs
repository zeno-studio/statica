
use std::collections::HashMap;
use reqwest::Client;
use serde_json::Value;


pub async fn fetch_arweave_folder(
    client: &Client,
    manifest_txid: &str,
) -> Result<HashMap<String, (Vec<u8>, String)>, Box<dyn std::error::Error + Send + Sync>> {
    let manifest_url = format!("https://arweave.net/{}", manifest_txid);
    let manifest: Value = client.get(&manifest_url).send().await?.json().await?;
    
    let mut files = HashMap::new();
    let paths = manifest["paths"]
        .as_object()
        .ok_or("Invalid manifest")?;
    
    for (path, info) in paths {
        let txid = info["id"].as_str().ok_or("Invalid txid")?;
        let file_url = format!("https://arweave.net/{}", txid);
        let response = client.get(&file_url).send().await?;
        let bytes = response.bytes().await?.to_vec();
        
        // Determine MIME type based on file extension
        let mime = match path.rsplit('.').next() {
            Some("html") => "text/html",
            Some("js") => "application/javascript",
            Some("css") => "text/css",
            _ => "application/octet-stream",
        }.to_string();
        
        files.insert(path.clone(), (bytes, mime));
    }
    
    Ok(files)
}

