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

#[inline]
fn class_index(size: usize) -> usize {
    debug_assert!(size.is_power_of_two());
    size.trailing_zeros() as usize - MIN_BUFFER_SIZE.trailing_zeros() as usize
}

fn acquire_vec(size: usize) -> Vec<u8> {
    let idx = class_index(size);
    let pool = &pools()[idx];
    let mut guard = pool.lock();
    guard.pop().unwrap_or_else(|| {
        let mut buffer = Vec::with_capacity(size);
        buffer.resize(size, 0);
        buffer
    })
}

fn release_vec(size: usize, mut buffer: Vec<u8>) {
    let idx = class_index(size);
    buffer.clear();
    buffer.resize(size, 0);
    pools()[idx].lock().push(buffer);
}

/// Lease representing exclusive write access to a buffer prior to packet creation.
pub struct BufferLease {
    size_class: usize,
    data: Option<Vec<u8>>,
}

impl BufferLease {
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

#[derive(Debug)]
struct BufferInner {
    size_class: usize,
    data: Option<Vec<u8>>,
}

impl BufferInner {
    fn new(size_class: usize, data: Vec<u8>) -> Self {
        Self {
            size_class,
            data: Some(data),
        }
    }

    fn slice(&self) -> &[u8] {
        self.data.as_ref().map(|d| d.as_slice()).unwrap_or_default()
    }

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
