//! Thread-safe port allocator for QEMU SSH forwarding.
//!
//! Each stereOS VM instance needs a unique host-side SSH port for
//! `hostfwd=tcp::{port}-:22`. This allocator manages a port range
//! and ensures no two VMs use the same port.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::sandbox::error::{Result, SandboxError};

/// Allocates unique host-side ports for QEMU SSH forwarding.
pub struct PortAllocator {
    /// First port in the allocation range.
    base: u16,
    /// Number of ports available (base..base+range).
    range: u16,
    /// Currently allocated ports.
    used: Arc<RwLock<HashSet<u16>>>,
}

impl PortAllocator {
    /// Create a new port allocator.
    ///
    /// Allocates ports from `base` to `base + range - 1`.
    pub fn new(base: u16, range: u16) -> Self {
        Self {
            base,
            range,
            used: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Allocate a free port, verifying it's actually bindable.
    ///
    /// Scans the range sequentially under a write lock. Each candidate port is
    /// tested with a blocking `TcpListener::bind` (dispatched via
    /// `spawn_blocking` to avoid stalling the tokio runtime). Port arithmetic
    /// uses `checked_add` to guard against overflow near `u16::MAX`.
    ///
    /// There is a small TOCTOU window between the bind check and QEMU's actual
    /// bind; in practice this is negligible on the loopback interface.
    pub async fn allocate(&self) -> Result<u16> {
        let mut used = self.used.write().await;

        for offset in 0..self.range {
            let port = match self.base.checked_add(offset) {
                Some(p) => p,
                None => {
                    return Err(SandboxError::CapacityExhausted {
                        reason: format!(
                            "port range overflow: base={} offset={}",
                            self.base, offset
                        ),
                    });
                }
            };
            if used.contains(&port) {
                continue;
            }

            // Verify the port is actually free on the host.
            // Run the blocking bind() check on a blocking thread to avoid
            // holding the tokio executor.
            let available = tokio::task::spawn_blocking(move || Self::is_port_available(port))
                .await
                .unwrap_or(false);

            if available {
                used.insert(port);
                return Ok(port);
            }
        }

        Err(SandboxError::CapacityExhausted {
            reason: format!(
                "no free ports in range {}..{} ({} in use)",
                self.base,
                self.base.saturating_add(self.range),
                used.len()
            ),
        })
    }

    /// Release a previously allocated port.
    pub async fn release(&self, port: u16) {
        self.used.write().await.remove(&port);
    }

    /// Check how many ports are currently allocated.
    pub async fn allocated_count(&self) -> usize {
        self.used.read().await.len()
    }

    /// Check if a port is available by attempting to bind to it.
    fn is_port_available(port: u16) -> bool {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use crate::sandbox::stereos::ports::*;

    #[tokio::test]
    async fn test_allocate_returns_port_in_range() {
        let alloc = PortAllocator::new(49000, 10);
        let port = alloc.allocate().await.unwrap();
        assert!((49000..49010).contains(&port));
    }

    #[tokio::test]
    async fn test_allocate_no_duplicates() {
        let alloc = PortAllocator::new(49010, 5);
        let mut ports = Vec::new();

        for _ in 0..5 {
            if let Ok(p) = alloc.allocate().await {
                ports.push(p);
            }
        }

        let unique: HashSet<_> = ports.iter().collect();
        assert_eq!(ports.len(), unique.len(), "duplicate ports allocated");
    }

    #[tokio::test]
    async fn test_release_allows_reallocation() {
        let alloc = PortAllocator::new(49020, 1);
        let port = alloc.allocate().await.unwrap();
        assert_eq!(alloc.allocated_count().await, 1);

        alloc.release(port).await;
        assert_eq!(alloc.allocated_count().await, 0);

        let port2 = alloc.allocate().await.unwrap();
        assert_eq!(port, port2);
    }

    #[tokio::test]
    async fn test_exhaustion_returns_error() {
        let alloc = PortAllocator::new(49030, 2);
        let _p1 = alloc.allocate().await.unwrap();
        let _p2 = alloc.allocate().await.unwrap();
        let result = alloc.allocate().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_overflow_returns_error() {
        // base near u16::MAX should not panic
        let alloc = PortAllocator::new(65530, 10);
        // Should get at most 6 ports (65530..65535) before overflow
        let mut count = 0;
        for _ in 0..10 {
            match alloc.allocate().await {
                Ok(_) => count += 1,
                Err(_) => break,
            }
        }
        assert!(count <= 6, "should not exceed u16 range");
    }
}
