//! Pyramid blocks, decoded once and shared by every thread of every render.
//!
//! A block read through the GTI index costs ~16 ms even with GDAL's cache
//! warm: the index composites every source overlapping it. The rays of all a
//! render's threads cross the blocks near the viewpoint, and the next render
//! of the same ground wants them again, so each is read once and kept here.

use gdal::Dataset;
use quick_cache::Weighter;
use quick_cache::sync::Cache;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

/// Pixels per block side.
pub const BLOCK: usize = 512;

const BLOCK_BYTES: u64 = (BLOCK * BLOCK * 2) as u64;

/// What an entry with no data costs against the budget: roughly its slot in
/// the cache, so ground with no data cannot grow it without bound.
const EMPTY_BYTES: u64 = 256;

/// A block's place: the level file's modification time, so a level written
/// afresh by `bin/index.sh` is never answered from blocks of the one before,
/// then the level and the block.
type Key = (u128, u32, i64, i64);

/// A block, or `None` where it holds no data -- sea, or past every source --
/// which costs a cache slot rather than half a megabyte.
pub type Block = Option<Arc<[u16]>>;

#[derive(Clone)]
struct Bytes;

impl Weighter<Key, Block> for Bytes {
    fn weight(&self, _: &Key, block: &Block) -> u64 {
        block.as_ref().map_or(EMPTY_BYTES, |_| BLOCK_BYTES)
    }
}

/// Bounded by memory. A miss is read by one caller while the others wanting
/// the same block wait for it; a read that fails or panics is not kept.
pub struct BlockCache(Cache<Key, Block, Bytes>);

impl BlockCache {
    pub fn new(budget_mb: u64) -> Self {
        let budget = budget_mb * 1024 * 1024;
        #[allow(clippy::cast_possible_truncation)]
        let blocks = (budget / BLOCK_BYTES).max(1) as usize;
        Self(Cache::with_weighter(blocks, budget, Bytes))
    }

    /// The block, read with `read` on a miss, and whether this call read it.
    pub fn get_or_read<E>(&self, k: Key, read: impl FnOnce() -> Result<Block, E>) -> (Block, bool) {
        let mut fresh = false;
        let block = self.0.get_or_insert_with(&k, || {
            fresh = true;
            read()
        });
        // A failed read is answered as no data for this lookup only.
        (block.unwrap_or(None), fresh)
    }
}

/// When a level file was last written, as the cache's generation for it.
pub fn generation(path: &Path) -> u128 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

thread_local! {
    /// Each thread's open index levels. A GDAL dataset is not shared between
    /// threads, but the render threads outlive the render, so opening a level
    /// once per thread rather than once per render saves the open each time.
    static OPEN: RefCell<HashMap<PathBuf, (u128, Rc<Dataset>)>> = RefCell::new(HashMap::new());
}

/// This thread's dataset for a level file at `generation`, opened on first
/// use and again once the file is rewritten.
pub fn dataset(path: &Path, generation: u128) -> gdal::errors::Result<Rc<Dataset>> {
    OPEN.with(|open| {
        let mut open = open.borrow_mut();
        if let Some((g, ds)) = open.get(path)
            && *g == generation
        {
            return Ok(ds.clone());
        }
        let ds = Rc::new(Dataset::open(path)?);
        open.insert(path.to_path_buf(), (generation, ds.clone()));
        Ok(ds)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn data() -> Result<Block, ()> {
        Ok(Some(Arc::from(vec![7u16; BLOCK * BLOCK])))
    }

    #[test]
    fn a_block_is_read_once() {
        let cache = BlockCache::new(64);
        let mut reads = 0;
        for _ in 0..3 {
            cache.get_or_read((1, 14, 3, 4), || {
                reads += 1;
                data()
            });
        }
        assert_eq!(reads, 1);
    }

    /// Threads wanting the same block at once wait for one read.
    #[test]
    fn concurrent_misses_read_once() {
        let cache = BlockCache::new(64);
        let reads = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    cache.get_or_read((1, 14, 0, 0), || {
                        reads.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        data()
                    })
                });
            }
        });
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    /// A failed read is not mistaken for ground without data next time.
    #[test]
    fn a_failed_read_is_retried() {
        let cache = BlockCache::new(64);
        assert!(
            cache
                .get_or_read((1, 14, 0, 0), || Err::<Block, ()>(()))
                .0
                .is_none()
        );
        assert!(cache.get_or_read((1, 14, 0, 0), data).0.is_some());
    }

    /// A panic mid-read leaves the block to be read again.
    #[test]
    fn a_panicking_read_is_recovered() {
        let cache = Arc::new(BlockCache::new(64));
        let c = cache.clone();
        let _ = std::thread::spawn(move || {
            c.get_or_read((1, 14, 0, 0), || -> Result<Block, ()> {
                panic!("read failed")
            })
        })
        .join();
        assert!(cache.get_or_read((1, 14, 0, 0), data).0.is_some());
    }

    #[test]
    fn a_rewritten_level_is_read_afresh() {
        let cache = BlockCache::new(64);
        let a = cache
            .get_or_read((1, 14, 0, 0), || {
                Ok::<_, ()>(Some(Arc::from(vec![1u16; 4])))
            })
            .0;
        let b = cache
            .get_or_read((2, 14, 0, 0), || {
                Ok::<_, ()>(Some(Arc::from(vec![2u16; 4])))
            })
            .0;
        assert_ne!(a, b);
    }

    /// Past the budget blocks go, empty ones included.
    #[test]
    fn the_budget_holds() {
        for mb in [16, 64] {
            let cache = BlockCache::new(mb);
            for i in 0..2000 {
                cache.get_or_read((1, 14, i, 0), data);
                cache.get_or_read((1, 14, i, 1), || Ok::<Block, ()>(None));
            }
            assert!(
                cache.0.weight() <= mb * 1024 * 1024,
                "{mb} MB budget holds {}",
                cache.0.weight()
            );
            assert!(!cache.0.is_empty());
        }
    }
}
