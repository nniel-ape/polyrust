use crate::error::Result;
use std::future::Future;
use std::pin::Pin;

/// Trait for strategies that provide a custom dashboard view.
///
/// Implement this on your strategy struct to render a custom HTML fragment
/// that will be displayed at `/strategy/<view_name>` in the dashboard.
pub trait DashboardViewProvider: Send + Sync {
    /// Short URL-safe name for this view (used in route: `/strategy/<name>`)
    fn view_name(&self) -> &str;

    /// Render the strategy's dashboard view as an HTML fragment.
    /// The fragment is inserted into the strategy_view template.
    fn render_view(&self) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>>;
}
