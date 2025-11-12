//! Fixed-size buffer pool enabling zero-copy ingress/egress transfers.
//!
//! Buffers are organised in power-of-two slabs ranging from 64 bytes up to 64 KiB. Each slab keeps
//! a small stash of pre-allocated `Vec<u8>` instances that are recycled via RAII. Callers lease a
//! buffer sized to the upcoming datagram, fill it, and convert it into a `BufferHandle` which wraps
//! the allocation in an `Arc` so packets can be cloned without copying.

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::OnceLock;

const MIN_BUFFER_SIZE: usize = 64;
const MAX_BUFFER_SIZE: usize = 65_536;
const BUFFER_SIZES: [usize; 11] = [
    64, 128, 256, 512, 1024, 2048, 4096, 8192, 16_384, 32_768, 65_536,
];
const BUFFERS_PER_SIZE: usize = 32;

static POOLS: OnceLock<Vec<Mutex<Vec<Vec<u8>>>>> = OnceLock::new();

fn pools() -> &'static [Mutex<Vec<Vec<u8>>>] {
    POOLS.get_or_init(|| {
        BUFFER_SIZES
            .iter()
            .map(|&size| {
                let mut buffers = Vec::with_capacity(BUFFERS_PER_SIZE);
                for _ in 0..BUFFERS_PER_SIZE {
                    let mut buffer = Vec::with_capacity(size);
                    buffer.resize(size, 0);
                    buffers.push(buffer);
                }
                Mutex::new(buffers)
            })
            .collect()
    })
}

/// Compute the appropriate buffer size class for a given length hint.
///
/// Returns the smallest power-of-two buffer size that can accommodate `len` bytes,
/// clamped to the valid range [MIN_BUFFER_SIZE, MAX_BUFFER_SIZE]. This ensures efficient
/// memory usage while providing sufficient capacity.
///
/// # Arguments
/// * `len` - Desired buffer size in bytes (may be 0)
///
/// # Returns
/// Power-of-two buffer size class (64, 128, 256, ..., 65536)
///
/// # Algorithm
/// 1. If `len == 0`, return `MIN_BUFFER_SIZE` (64 bytes)
/// 2. Clamp `len` to [0, MAX_BUFFER_SIZE] to prevent oversized allocations
/// 3. Round up to next power of two using `next_power_of_two()`
/// 4. Clamp result to [MIN_BUFFER_SIZE, MAX_BUFFER_SIZE]
#[inline]
fn size_class_for(len: usize) -> usize {
    if len == 0 {
        return MIN_BUFFER_SIZE;
    }
    let capped = len.min(MAX_BUFFER_SIZE);
    let mut next = capped.next_power_of_two();
    if next < MIN_BUFFER_SIZE {
        next = MIN_BUFFER_SIZE;
    }
    if next > MAX_BUFFER_SIZE {
        next = MAX_BUFFER_SIZE;
    }
    next
}

/// Compute the pool index for a given power-of-two buffer size.
///
/// Uses bit manipulation (trailing zeros) for O(1) lookup. The index is computed as
/// the difference in trailing zeros between the size and MIN_BUFFER_SIZE, which maps
/// each power-of-two size to its corresponding pool index.
///
/// # Arguments
/// * `size` - Buffer size (must be a power of two, checked via debug_assert)
///
/// # Returns
/// Pool index (0 for 64 bytes, 1 for 128 bytes, ..., 10 for 65536 bytes)
///
/// # Performance
/// O(1) operation using bit manipulation. Much faster than binary search or hash lookup.
#[inline]
fn class_index(size: usize) -> usize {
    debug_assert!(size.is_power_of_two());
    size.trailing_zeros() as usize - MIN_BUFFER_SIZE.trailing_zeros() as usize
}

/// Acquire a buffer from the pool or allocate a new one if the pool is empty.
///
/// Attempts to pop a buffer from the appropriate size class pool. If the pool is empty,
/// allocates a new buffer of the requested size. This ensures zero-copy operations
/// while gracefully handling pool exhaustion.
///
/// # Arguments
/// * `size` - Buffer size (must be a power of two, from BUFFER_SIZES)
///
/// # Returns
/// A `Vec<u8>` buffer of the requested size (initialized to zeros)
///
/// # Thread Safety
/// Uses `parking_lot::Mutex` for pool access. Multiple threads can safely acquire
/// buffers concurrently without blocking (mutex contention is minimal).
fn acquire_vec(size: usize) -> Vec<u8> {
    let idx = class_index(size);
    let pool = &pools()[idx];
    let mut guard = pool.lock();
    // Try to reuse a buffer from the pool, or allocate a new one if pool is empty
    guard.pop().unwrap_or_else(|| {
        let mut buffer = Vec::with_capacity(size);
        buffer.resize(size, 0);
        buffer
    })
}

/// Release a buffer back to the pool for reuse.
///
/// Clears the buffer contents and resizes it to its original size class, then pushes
/// it back into the pool. This enables buffer recycling, reducing allocation pressure
/// and improving performance for high-throughput workloads.
///
/// # Arguments
/// * `size` - Original buffer size class (must match the buffer's allocated size)
/// * `buffer` - The buffer to recycle (will be cleared and resized)
///
/// # Thread Safety
/// Uses `parking_lot::Mutex` for pool access. Multiple threads can safely release
/// buffers concurrently.
fn release_vec(size: usize, mut buffer: Vec<u8>) {
    let idx = class_index(size);
    // Clear buffer contents (set all bytes to 0) and resize to original capacity
    buffer.clear();
    buffer.resize(size, 0);
    // Push back into pool for reuse
    pools()[idx].lock().push(buffer);
}

/// Lease representing exclusive write access to a buffer prior to packet creation.
pub struct BufferLease {
    size_class: usize,
    data: Option<Vec<u8>>,
}

impl BufferLease {
    /// Create a new buffer lease with exclusive write access.
    ///
    /// # Arguments
    /// * `size_class` - Size class of the buffer (for recycling on drop)
    /// * `data` - The buffer data (wrapped in Option for move semantics)
    fn new(size_class: usize, data: Vec<u8>) -> Self {
        Self {
            size_class,
            data: Some(data),
        }
    }

    /// Borrow a mutable slice to fill with incoming data.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.data
            .as_mut()
            .expect("buffer lease already consumed")
            .as_mut_slice()
    }

    /// Convert the lease into a shared `BufferHandle` containing `len` valid bytes.
    pub fn freeze(mut self, len: usize) -> BufferHandle {
        let mut data = self.data.take().expect("buffer lease already consumed");
        let valid = len.min(data.len());
        data.truncate(valid);
        BufferHandle::new(self.size_class, data)
    }
}

impl Drop for BufferLease {
    fn drop(&mut self) {
        if let Some(data) = self.data.take() {
            release_vec(self.size_class, data);
        }
    }
}

/// Internal buffer representation wrapped in `Arc` for shared ownership.
///
/// `BufferInner` holds the actual `Vec<u8>` allocation and tracks its size class for
/// proper recycling when the last `BufferHandle` is dropped. The `data` field is
/// `Option<Vec<u8>>` to allow moving it out during drop (RAII pattern).
#[derive(Debug)]
struct BufferInner {
    /// Size class of this buffer (used to return it to the correct pool on drop)
    size_class: usize,
    /// The actual buffer data (moved out on drop for recycling)
    data: Option<Vec<u8>>,
}

impl BufferInner {
    /// Create a new buffer inner with the given size class and data.
    ///
    /// # Arguments
    /// * `size_class` - Size class for pool recycling (must match data.len())
    /// * `data` - The buffer data (will be wrapped in Option)
    fn new(size_class: usize, data: Vec<u8>) -> Self {
        Self {
            size_class,
            data: Some(data),
        }
    }

    /// Return a read-only slice of the buffer data.
    ///
    /// Returns an empty slice if the data has already been moved out (shouldn't happen
    /// in normal operation, but provides safety).
    fn slice(&self) -> &[u8] {
        self.data.as_ref().map(|d| d.as_slice()).unwrap_or_default()
    }

    /// Return the length of the buffer data.
    ///
    /// Returns 0 if the data has already been moved out.
    fn len(&self) -> usize {
        self.data.as_ref().map(|d| d.len()).unwrap_or(0)
    }
}

impl Drop for BufferInner {
    fn drop(&mut self) {
        if let Some(mut data) = self.data.take() {
            let size = self.size_class;
            data.clear();
            data.resize(size, 0);
            release_vec(size, data);
        }
    }
}

/// Shared, cloneable handle to a pooled buffer.
#[derive(Clone, Debug)]
pub struct BufferHandle {
    inner: Arc<BufferInner>,
}

impl BufferHandle {
    /// Create a new buffer handle wrapping the given data.
    ///
    /// The data is wrapped in `Arc<BufferInner>` to enable shared ownership. When the
    /// last `BufferHandle` is dropped, the buffer is automatically recycled back to the
    /// pool via `BufferInner::drop`.
    ///
    /// # Arguments
    /// * `size_class` - Size class of the buffer (for recycling on drop)
    /// * `data` - The buffer data (will be wrapped in Arc)
    fn new(size_class: usize, data: Vec<u8>) -> Self {
        Self {
            inner: Arc::new(BufferInner::new(size_class, data)),
        }
    }

    /// Return a read-only view of the payload bytes.
    pub fn as_slice(&self) -> &[u8] {
        self.inner.slice()
    }

    /// Number of valid bytes within the handle.
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

/// Acquire a buffer sized for `size_hint` bytes.
pub fn lease(size_hint: usize) -> BufferLease {
    let size = size_class_for(size_hint);
    BufferLease::new(size, acquire_vec(size))
}
