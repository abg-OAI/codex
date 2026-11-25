//! Per-request limits for rollout I/O during thread search.
//!
//! Account checks, filesystem listing and content matching all open rollout readers. They
//! share this request's counters, including when a reader moves onto a blocking worker.

use std::future::Future;
use std::io;
use std::io::Read;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

const SEARCH_BYTES: u64 = 64 * 1024 * 1024;

tokio::task_local! {
    static ACTIVE: Arc<SearchReadBudget>;
}

/// Limits encoded and decoded rollout input independently across all readers in one search.
pub async fn with_thread_search_read_limit<T>(operation: impl Future<Output = T>) -> io::Result<T> {
    let budget = Arc::new(SearchReadBudget::new(SEARCH_BYTES));
    let result = ACTIVE.scope(Arc::clone(&budget), operation).await;
    // Some discovery paths skip unreadable candidates. Exhaustion must still fail the request.
    if budget.exceeded.load(Ordering::Relaxed) {
        return Err(limit_error());
    }
    Ok(result)
}

pub(crate) struct SearchReadBudget {
    encoded: Mutex<u64>,
    decoded: Mutex<u64>,
    exceeded: AtomicBool,
}

impl SearchReadBudget {
    fn new(limit: u64) -> Self {
        Self {
            encoded: Mutex::new(limit),
            decoded: Mutex::new(limit),
            exceeded: AtomicBool::new(false),
        }
    }

    pub(crate) fn current() -> Option<Arc<Self>> {
        ACTIVE.try_with(Arc::clone).ok()
    }

    pub(crate) fn reader(
        self: &Arc<Self>,
        file: std::fs::File,
        compressed: bool,
    ) -> io::Result<Box<dyn Read + Send>> {
        let size = file.metadata()?.len();
        let encoded = MeteredReader {
            inner: file,
            budget: Arc::clone(self),
            decoded: false,
        }
        .take(size);
        if compressed {
            let decoder = zstd::stream::read::Decoder::new(encoded)?;
            Ok(Box::new(MeteredReader {
                inner: decoder,
                budget: Arc::clone(self),
                decoded: true,
            }))
        } else {
            Ok(Box::new(
                MeteredReader {
                    inner: encoded,
                    budget: Arc::clone(self),
                    decoded: true,
                }
                .take(size),
            ))
        }
    }
}

fn limit_error() -> io::Error {
    io::Error::other("thread/search exceeded the 64 MiB request input limit")
}

struct MeteredReader<R> {
    inner: R,
    budget: Arc<SearchReadBudget>,
    decoded: bool,
}

impl<R: Read> Read for MeteredReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let counter = if self.decoded {
            &self.budget.decoded
        } else {
            &self.budget.encoded
        };
        let mut remaining = counter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *remaining == 0 {
            // Physical inputs are capped at the size of the open file, so an exhausted
            // encoded reader is only called when that file still contains data. Decoders
            // can finish a frame without producing another byte.
            if self.decoded && self.inner.read(&mut [0])? == 0 {
                return Ok(0);
            }
            self.budget.exceeded.store(true, Ordering::Relaxed);
            return Err(limit_error());
        }
        let available = (*remaining).min(buf.len() as u64) as usize;
        let count = self.inner.read(&mut buf[..available])?;
        *remaining -= count as u64;
        Ok(count)
    }
}

#[cfg(test)]
#[path = "read_budget_tests.rs"]
mod tests;
