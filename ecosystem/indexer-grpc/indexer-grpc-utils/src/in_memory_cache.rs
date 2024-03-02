// Copyright © Aptos Foundation

use crate::compression_util::{CacheEntry, StorageFormat};
use anyhow::Context;
use aptos_protos::transaction::v1::Transaction;
// use mini_moka::sync::Cache;
// use quick_cache::{sync::Cache, Weighter};
use dashmap::DashMap;
use itertools::Itertools;
use prost::Message;
use redis::AsyncCommands;
use std::sync::Arc;
use tokio::sync::RwLock;

// Internal lookup retry interval for in-memory cache.
const IN_MEMORY_CACHE_LOOKUP_RETRY_INTERVAL_MS: u64 = 10;
// Max cache size in bytes: 5 GB.
const MAX_IN_MEMORY_CACHE_CAPACITY_IN_BYTES: u64 = 5_000_000_000;
// Max cache entry TTL: 30 seconds.
// const MAX_IN_MEMORY_CACHE_ENTRY_TTL: u64 = 30;
// Warm-up cache entries. Pre-fetch the cache entries to warm up the cache.
const WARM_UP_CACHE_ENTRIES: u64 = 20_000;
const MAX_REDIS_FETCH_BATCH_SIZE: usize = 500;

#[derive(Debug, Clone, Copy)]
struct CacheMetadata {
    total_size_in_bytes: u64,
    latest_version: u64,
    first_version: u64,
}

/// InMemoryCache is a simple in-memory cache that stores the protobuf Transaction.
pub struct InMemoryCache {
    /// Cache maps the cache key to the deserialized Transaction.
    cache: Arc<DashMap<u64, Arc<Transaction>>>,
    cache_metadata: Arc<RwLock<CacheMetadata>>,
    _cancellation_token_drop_guard: tokio_util::sync::DropGuard,
}

impl InMemoryCache {
    pub async fn new_with_redis_connection<C>(
        conn: C,
        storage_format: StorageFormat,
    ) -> anyhow::Result<Self>
    where
        C: redis::aio::ConnectionLike + Send + Sync + Clone + 'static,
    {
        let cache = DashMap::new();
        let cache_arc = Arc::new(cache);
        let (in_memory_first_version, in_memory_latest_version, total_size_in_bytes) =
            warm_up_the_cache(conn.clone(), cache_arc.clone(), storage_format).await?;
        tracing::info!(
            "In-memory cache is warmed up to version {}",
            in_memory_latest_version
        );
        let cancellation_token = tokio_util::sync::CancellationToken::new();
        let cancellation_token_clone = cancellation_token.clone();
        let cache_metadata = Arc::new(RwLock::new(CacheMetadata {
            first_version: in_memory_first_version,
            total_size_in_bytes,
            latest_version: in_memory_latest_version,
        }));
        create_update_task(
            conn,
            cache_arc.clone(),
            cache_metadata.clone(),
            storage_format,
            cancellation_token_clone,
        )
        .await;
        tracing::info!("In-memory cache is created");
        Ok(Self {
            cache: cache_arc,
            cache_metadata,
            _cancellation_token_drop_guard: cancellation_token.drop_guard(),
        })
    }

    async fn latest_version(&self) -> u64 {
        self.cache_metadata.read().await.latest_version
    }

    // This returns the transaction if it exists in the cache.
    // If requested version is not in the cache, it blocks until the version is available.
    // Otherwise, empty.
    pub async fn get_transactions(&self, starting_version: u64) -> Vec<Transaction> {
        let start_time = std::time::Instant::now();
        let (versions_to_fetch, in_memory_latest_version) = loop {
            let latest_version = self.latest_version().await;
            if starting_version >= latest_version {
                tokio::time::sleep(std::time::Duration::from_millis(
                    IN_MEMORY_CACHE_LOOKUP_RETRY_INTERVAL_MS,
                ))
                .await;
                continue;
            }
            // This is to avoid fetching too many transactions at once.
            let ending_version = std::cmp::min(
                latest_version,
                starting_version + MAX_REDIS_FETCH_BATCH_SIZE as u64,
            );
            break (
                (starting_version..ending_version).collect::<Vec<u64>>(),
                latest_version,
            );
        };
        let mut arc_transactions = Vec::new();
        for key in versions_to_fetch {
            if let Some(transaction) = self.cache.get(&key) {
                arc_transactions.push(transaction.clone());
            } else {
                break;
            }
        }

        tracing::info!(
            transactions_count = arc_transactions.len(),
            starting_version,
            in_memory_latest_version,
            duration_in_seconds = start_time.elapsed().as_secs_f64(),
            "In-memory cache lookup",
        );
        // Actual clone.
        arc_transactions
            .into_iter()
            .map(|t| t.as_ref().clone())
            .collect()
    }
}

/// Warm up the cache with the latest transactions.
async fn warm_up_the_cache<C>(
    conn: C,
    cache: Arc<DashMap<u64, Arc<Transaction>>>,
    storage_format: StorageFormat,
) -> anyhow::Result<(u64, u64, u64)>
where
    C: redis::aio::ConnectionLike + Send + Sync + Clone + 'static,
{
    let mut conn = conn.clone();
    let latest_version = get_config_by_key(&mut conn, "latest_version")
        .await
        .context("Failed to fetch the latest version from redis")?
        .context("Latest version doesn't exist in Redis")?;
    let versions_to_fetch: Vec<u64> =
        (latest_version.saturating_sub(WARM_UP_CACHE_ENTRIES)..latest_version).collect();
    let first_version = versions_to_fetch[0];
    let transactions = batch_get_transactions(&mut conn, versions_to_fetch, storage_format).await?;
    let total_size_in_bytes = transactions.iter().map(|t| t.encoded_len() as u64).sum();
    for transaction in transactions {
        cache.insert(transaction.version, Arc::new(transaction));
    }
    Ok((first_version, latest_version, total_size_in_bytes))
}

async fn create_update_task<C>(
    conn: C,
    cache: Arc<DashMap<u64, Arc<Transaction>>>,
    cache_metadata: Arc<RwLock<CacheMetadata>>,
    storage_format: StorageFormat,
    cancellation_token: tokio_util::sync::CancellationToken,
) where
    C: redis::aio::ConnectionLike + Send + Sync + Clone + 'static,
{
    tokio::spawn(async move {
        let mut conn = conn.clone();
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(
            IN_MEMORY_CACHE_LOOKUP_RETRY_INTERVAL_MS,
        ));
        loop {
            let current_latest_version = get_config_by_key(&mut conn, "latest_version")
                .await
                .context("Failed to fetch the latest version from redis")
                .unwrap()
                .context("Latest version doesn't exist in Redis")
                .unwrap();
            let in_cache_latest_version = { cache_metadata.read().await.latest_version };
            if current_latest_version == in_cache_latest_version {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        tracing::info!("In-memory cache update task is cancelled.");
                        return;
                    },
                    _ = interval.tick() => {
                        continue;
                    },
                }
            }
            let start_time = std::time::Instant::now();
            let end_version = std::cmp::min(
                current_latest_version,
                in_cache_latest_version + 10 * MAX_REDIS_FETCH_BATCH_SIZE as u64,
            );
            let versions_to_fetch = (in_cache_latest_version..end_version).collect();
            let transactions = batch_get_transactions(&mut conn, versions_to_fetch, storage_format)
                .await
                .unwrap();
            // Ensure that transactions are ordered by version.
            let cache_processing_start_time = std::time::Instant::now();
            let mut newly_added_bytes = 0;
            for (ind, transaction) in transactions.iter().enumerate() {
                if transaction.version != in_cache_latest_version + ind as u64 {
                    panic!("Transactions are not ordered by version");
                }
                newly_added_bytes += transaction.encoded_len() as u64;
            }

            for transaction in transactions {
                cache.insert(transaction.version, Arc::new(transaction));
            }
            let processing_duration = start_time.elapsed().as_secs_f64();
            tracing::info!(
                redis_latest_version = current_latest_version,
                in_memory_latest_version = in_cache_latest_version,
                new_in_memory_latest_version = end_version,
                processing_duration,
                cache_processing_duration = cache_processing_start_time.elapsed().as_secs_f64(),
                "In-memory cache is updated"
            );
            let mut current_cache_metadata = { *cache_metadata.read().await };
            current_cache_metadata.latest_version = end_version;
            current_cache_metadata.total_size_in_bytes += newly_added_bytes;
            // Get the data available.
            *cache_metadata.write().await = current_cache_metadata;
            // Clean up.
            while current_cache_metadata.total_size_in_bytes > MAX_IN_MEMORY_CACHE_CAPACITY_IN_BYTES
            {
                let key_to_remove = current_cache_metadata.first_version;
                let (_k, v) = cache
                    .remove(&key_to_remove)
                    .expect("Failed to remove the key");
                current_cache_metadata.total_size_in_bytes -= v.encoded_len() as u64;
                current_cache_metadata.first_version += 1;
            }
            *cache_metadata.write().await = current_cache_metadata;
        }
    });
}

// TODO: move the following functions to cache operator.
async fn get_config_by_key<C>(conn: &mut C, key: &str) -> anyhow::Result<Option<u64>>
where
    C: redis::aio::ConnectionLike + Send + Sync + Clone + 'static,
{
    let value = redis::cmd("GET").arg(key).query_async(conn).await?;
    Ok(value)
}

async fn batch_get_transactions<C>(
    conn: &mut C,
    versions: Vec<u64>,
    storage_format: StorageFormat,
) -> anyhow::Result<Vec<Transaction>>
where
    C: redis::aio::ConnectionLike + Send + Sync + Clone + 'static,
{
    let keys: Vec<String> = versions
        .into_iter()
        .map(|version| CacheEntry::build_key(version, storage_format))
        .collect();
    let mut tasks: Vec<tokio::task::JoinHandle<anyhow::Result<Vec<Transaction>>>> = Vec::new();
    for chunk in &keys.into_iter().chunks(MAX_REDIS_FETCH_BATCH_SIZE) {
        let keys: Vec<String> = chunk.collect();
        let mut conn = conn.clone();
        tasks.push(tokio::spawn(async move {
            let values = conn.mget::<Vec<String>, Vec<Vec<u8>>>(keys).await?;
            // If any of the values are empty, we return an error.
            if values.iter().any(|v| v.is_empty()) {
                return Err(anyhow::anyhow!("Failed to fetch all the keys"));
            }
            let transactions = values
                .into_iter()
                .map(|v| {
                    let cache_entry = CacheEntry::new(v, storage_format);
                    cache_entry.into_transaction()
                })
                .collect();
            Ok(transactions)
        }));
    }
    // join all.
    let results = futures::future::join_all(tasks).await;
    let mut transactions = Vec::new();
    for result in results {
        transactions.extend(result??);
    }
    anyhow::Result::Ok(transactions)
}
