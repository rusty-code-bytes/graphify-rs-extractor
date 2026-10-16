//! Opt-in diagnostic spans. No graph metadata or extraction behavior changes.
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Span(Option<(String, f64)>);
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
pub fn span(name: impl FnOnce() -> String) -> Span {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var_os("GRE_PROFILE").is_some()) {
        return Span(None);
    }
    let name = name();
    let start = now();
    eprintln!(
        "GRE_PROFILE {}",
        serde_json::json!({"event":"begin","name":name,"time":start})
    );
    Span(Some((name, start)))
}
impl Drop for Span {
    fn drop(&mut self) {
        if let Some((name, start)) = &self.0 {
            let end = now();
            eprintln!(
                "GRE_PROFILE {}",
                serde_json::json!({"event":"end","name":name,"time":end,"seconds":end-start})
            );
        }
    }
}
