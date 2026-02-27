//! Subscription management for streaming services.

#![allow(missing_docs)] // Internal subscription management

use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct Subscription {
    pub service: String,
    pub symbols: HashSet<String>,
    pub fields: Vec<String>,
}

#[derive(Debug)]
pub struct SubscriptionManager {
    subscriptions: RwLock<HashMap<String, Subscription>>,
    service_fields: RwLock<HashMap<String, String>>, // service -> comma-separated field ids
}

impl SubscriptionManager {
    pub fn new() -> Self {
        Self {
            subscriptions: RwLock::new(HashMap::new()),
            service_fields: RwLock::new(HashMap::new()),
        }
    }

    pub fn add_subscription(&self, service: String, symbols: Vec<String>, fields: String) {
        let mut subs = self.subscriptions.write();
        
        let entry = subs.entry(service.clone()).or_insert_with(|| {
            Subscription {
                service,
                symbols: HashSet::new(),
                fields: Vec::new(),
            }
        });

        for symbol in symbols {
            entry.symbols.insert(symbol);
        }
        
        // Update fields if provided (use comma-separated string)
        if !fields.is_empty() {
            entry.fields = fields.split(',').map(|s| s.to_string()).collect();
        }
    }

    pub fn remove_subscription(&self, service: String, symbols: Vec<String>) {
        let mut subs = self.subscriptions.write();
        
        if let Some(subscription) = subs.get_mut(&service) {
            for symbol in symbols {
                subscription.symbols.remove(&symbol);
            }

            if subscription.symbols.is_empty() {
                subs.remove(&service);
            }
        }
    }

    pub fn get_subscription(&self, service: &str) -> Option<Subscription> {
        self.subscriptions.read().get(service).cloned()
    }

    pub fn get_all_subscriptions(&self) -> HashMap<String, Vec<String>> {
        self.subscriptions
            .read()
            .iter()
            .map(|(service, sub)| {
                (
                    service.clone(),
                    sub.symbols.iter().cloned().collect(),
                )
            })
            .collect()
    }

    pub fn set_service_fields(&self, service: String, fields_csv: String) {
        self.service_fields.write().insert(service, fields_csv);
    }

    pub fn get_service_fields(&self, service: &str) -> Option<String> {
        self.service_fields.read().get(service).cloned()
    }

    pub fn clear_all(&self) {
        self.subscriptions.write().clear();
    }

    pub fn is_subscribed(&self, service: &str, symbol: &str) -> bool {
        self.subscriptions
            .read()
            .get(service)
            .map(|sub| sub.symbols.contains(symbol))
            .unwrap_or(false)
    }

    pub fn get_symbol_count(&self) -> usize {
        self.subscriptions
            .read()
            .values()
            .map(|sub| sub.symbols.len())
            .sum()
    }
}

impl Default for SubscriptionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subscription_management() {
        let manager = SubscriptionManager::new();

        // Add subscriptions
        manager.add_subscription(
            "LEVELONE_EQUITIES".to_string(),
            vec!["AAPL".to_string(), "GOOGL".to_string()],
            "0,1,2,3".to_string(),
        );

        assert!(manager.is_subscribed("LEVELONE_EQUITIES", "AAPL"));
        assert!(manager.is_subscribed("LEVELONE_EQUITIES", "GOOGL"));
        assert!(!manager.is_subscribed("LEVELONE_EQUITIES", "MSFT"));

        // Add more symbols to existing service
        manager.add_subscription(
            "LEVELONE_EQUITIES".to_string(),
            vec!["MSFT".to_string()],
            "0,1,2,3".to_string(),
        );

        assert!(manager.is_subscribed("LEVELONE_EQUITIES", "MSFT"));
        assert_eq!(manager.get_symbol_count(), 3);

        // Remove subscription
        manager.remove_subscription(
            "LEVELONE_EQUITIES".to_string(),
            vec!["AAPL".to_string()],
        );

        assert!(!manager.is_subscribed("LEVELONE_EQUITIES", "AAPL"));
        assert!(manager.is_subscribed("LEVELONE_EQUITIES", "GOOGL"));
        assert_eq!(manager.get_symbol_count(), 2);

        // Clear all
        manager.clear_all();
        assert_eq!(manager.get_symbol_count(), 0);
    }
}