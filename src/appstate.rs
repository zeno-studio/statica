use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;




#[derive(Clone)]
pub struct AppState {
    pub files: Arc<ArcSwap<HashMap<String, (Vec<u8>, String)>>>, // Path -> (Content, MIME type)
    pub last_updated: Arc<Mutex<i64>>,
}
impl AppState {
    pub fn new() -> Self {
        Self {
            files: Arc::new(ArcSwap::new(Arc::new(HashMap::new()))),
            last_updated: Arc::new(Mutex::new(0)),
        }
    }
}