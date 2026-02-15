//! SSE Dashboard Update Emission.

use std::sync::Arc;

use polyrust_core::prelude::*;

use super::CryptoArbDashboard;
use crate::crypto_arb::runtime::CryptoArbRuntime;

/// Emit SSE dashboard-update signals if the shared throttle allows.
///
/// Each signal carries pre-rendered HTML so the SSE handler can broadcast it
/// without re-acquiring strategy locks. Called at the end of each strategy's
/// `on_event()` — the shared 5-second throttle ensures only one strategy per
/// window triggers the render.
pub async fn try_emit_dashboard_updates(base: &Arc<CryptoArbRuntime>) -> Vec<Action> {
    if !base.try_claim_dashboard_emit().await {
        return vec![];
    }

    let provider = CryptoArbDashboard::new(Arc::clone(base));
    match provider.render_view().await {
        Ok(html) => vec![Action::EmitSignal {
            signal_type: "dashboard-update".to_string(),
            payload: serde_json::json!({
                "view_name": provider.view_name(),
                "rendered_html": html,
            }),
        }],
        Err(_) => vec![],
    }
}
