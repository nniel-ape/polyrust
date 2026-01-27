use std::collections::HashMap;
use std::sync::Arc;

use polyrust_core::prelude::*;
use rust_decimal::Decimal;
use tokio::sync::RwLock;

/// Thread-safe orderbook state manager.
///
/// Maintains the latest `OrderbookSnapshot` per token ID.
/// Strategies and the paper trading engine use this to query current
/// orderbook state without subscribing directly to the event bus.
#[derive(Debug, Clone)]
pub struct OrderbookManager {
    snapshots: Arc<RwLock<HashMap<TokenId, OrderbookSnapshot>>>,
}

impl OrderbookManager {
    pub fn new() -> Self {
        Self {
            snapshots: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Update the orderbook snapshot for a token.
    pub async fn update(&self, snapshot: OrderbookSnapshot) {
        let token_id = snapshot.token_id.clone();
        let mut snapshots = self.snapshots.write().await;
        snapshots.insert(token_id, snapshot);
    }

    /// Get the latest orderbook snapshot for a token, if available.
    pub async fn get_snapshot(&self, token_id: &str) -> Option<OrderbookSnapshot> {
        let snapshots = self.snapshots.read().await;
        snapshots.get(token_id).cloned()
    }

    /// Get the mid price for a token.
    pub async fn get_mid_price(&self, token_id: &str) -> Option<Decimal> {
        self.get_snapshot(token_id)
            .await
            .and_then(|s| s.mid_price())
    }

    /// Get the best bid price for a token.
    pub async fn get_best_bid(&self, token_id: &str) -> Option<Decimal> {
        self.get_snapshot(token_id).await.and_then(|s| s.best_bid())
    }

    /// Get the best ask price for a token.
    pub async fn get_best_ask(&self, token_id: &str) -> Option<Decimal> {
        self.get_snapshot(token_id).await.and_then(|s| s.best_ask())
    }

    /// Get the spread for a token.
    pub async fn get_spread(&self, token_id: &str) -> Option<Decimal> {
        self.get_snapshot(token_id).await.and_then(|s| s.spread())
    }

    /// Get all tracked token IDs.
    pub async fn tracked_tokens(&self) -> Vec<TokenId> {
        let snapshots = self.snapshots.read().await;
        snapshots.keys().cloned().collect()
    }

    /// Remove a token's orderbook data.
    pub async fn remove(&self, token_id: &str) {
        let mut snapshots = self.snapshots.write().await;
        snapshots.remove(token_id);
    }

    /// Clear all orderbook data.
    pub async fn clear(&self) {
        let mut snapshots = self.snapshots.write().await;
        snapshots.clear();
    }
}

impl Default for OrderbookManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn make_snapshot(
        token_id: &str,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
    ) -> OrderbookSnapshot {
        OrderbookSnapshot {
            token_id: token_id.to_string(),
            bids: bids
                .into_iter()
                .map(|(price, size)| OrderbookLevel { price, size })
                .collect(),
            asks: asks
                .into_iter()
                .map(|(price, size)| OrderbookLevel { price, size })
                .collect(),
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_update_and_get_snapshot() {
        let manager = OrderbookManager::new();
        let snapshot = make_snapshot(
            "token_a",
            vec![(dec!(0.48), dec!(100.0)), (dec!(0.47), dec!(200.0))],
            vec![(dec!(0.52), dec!(150.0)), (dec!(0.53), dec!(250.0))],
        );

        manager.update(snapshot.clone()).await;

        let retrieved = manager.get_snapshot("token_a").await;
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.token_id, "token_a");
        assert_eq!(retrieved.bids.len(), 2);
        assert_eq!(retrieved.asks.len(), 2);
        assert_eq!(retrieved.bids[0].price, dec!(0.48));
        assert_eq!(retrieved.asks[0].price, dec!(0.52));
    }

    #[tokio::test]
    async fn test_mid_price() {
        let manager = OrderbookManager::new();
        let snapshot = make_snapshot(
            "token_mid",
            vec![(dec!(0.48), dec!(100.0))],
            vec![(dec!(0.52), dec!(100.0))],
        );
        manager.update(snapshot).await;

        let mid = manager.get_mid_price("token_mid").await;
        assert_eq!(mid, Some(dec!(0.50)));
    }

    #[tokio::test]
    async fn test_spread() {
        let manager = OrderbookManager::new();
        let snapshot = make_snapshot(
            "token_spread",
            vec![(dec!(0.45), dec!(100.0))],
            vec![(dec!(0.55), dec!(100.0))],
        );
        manager.update(snapshot).await;

        let spread = manager.get_spread("token_spread").await;
        assert_eq!(spread, Some(dec!(0.10)));
    }

    #[tokio::test]
    async fn test_missing_token_returns_none() {
        let manager = OrderbookManager::new();

        assert!(manager.get_snapshot("nonexistent").await.is_none());
        assert!(manager.get_mid_price("nonexistent").await.is_none());
        assert!(manager.get_best_bid("nonexistent").await.is_none());
        assert!(manager.get_best_ask("nonexistent").await.is_none());
        assert!(manager.get_spread("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_update_overwrites_previous() {
        let manager = OrderbookManager::new();

        let snapshot1 = make_snapshot(
            "token_x",
            vec![(dec!(0.40), dec!(100.0))],
            vec![(dec!(0.60), dec!(100.0))],
        );
        manager.update(snapshot1).await;
        assert_eq!(manager.get_mid_price("token_x").await, Some(dec!(0.50)));

        let snapshot2 = make_snapshot(
            "token_x",
            vec![(dec!(0.45), dec!(100.0))],
            vec![(dec!(0.55), dec!(100.0))],
        );
        manager.update(snapshot2).await;
        assert_eq!(manager.get_mid_price("token_x").await, Some(dec!(0.50)));
        assert_eq!(manager.get_best_bid("token_x").await, Some(dec!(0.45)));
    }

    #[tokio::test]
    async fn test_remove_token() {
        let manager = OrderbookManager::new();
        let snapshot = make_snapshot(
            "token_rm",
            vec![(dec!(0.48), dec!(100.0))],
            vec![(dec!(0.52), dec!(100.0))],
        );
        manager.update(snapshot).await;
        assert!(manager.get_snapshot("token_rm").await.is_some());

        manager.remove("token_rm").await;
        assert!(manager.get_snapshot("token_rm").await.is_none());
    }

    #[tokio::test]
    async fn test_tracked_tokens() {
        let manager = OrderbookManager::new();
        manager
            .update(make_snapshot("t1", vec![(dec!(0.5), dec!(10.0))], vec![]))
            .await;
        manager
            .update(make_snapshot("t2", vec![], vec![(dec!(0.6), dec!(10.0))]))
            .await;

        let mut tokens = manager.tracked_tokens().await;
        tokens.sort();
        assert_eq!(tokens, vec!["t1", "t2"]);
    }

    #[tokio::test]
    async fn test_clear() {
        let manager = OrderbookManager::new();
        manager
            .update(make_snapshot("t1", vec![(dec!(0.5), dec!(10.0))], vec![]))
            .await;
        manager
            .update(make_snapshot("t2", vec![], vec![(dec!(0.6), dec!(10.0))]))
            .await;

        manager.clear().await;
        assert!(manager.tracked_tokens().await.is_empty());
    }

    #[tokio::test]
    async fn test_empty_orderbook_returns_none_for_derived_values() {
        let manager = OrderbookManager::new();
        let snapshot = make_snapshot("empty", vec![], vec![]);
        manager.update(snapshot).await;

        // Snapshot exists but has no levels
        assert!(manager.get_snapshot("empty").await.is_some());
        assert!(manager.get_mid_price("empty").await.is_none());
        assert!(manager.get_best_bid("empty").await.is_none());
        assert!(manager.get_best_ask("empty").await.is_none());
        assert!(manager.get_spread("empty").await.is_none());
    }
}
