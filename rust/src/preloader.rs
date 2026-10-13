//! Preload-all cache (port of `Preloader.hpp/cpp`).
//!
//! At startup every static wallpaper referenced by any monitor config is
//! decoded once on a threadpool and kept in RAM, so workspace switches
//! never touch disk. Animated posters (first frames for mpvpaper
//! wallpapers, see [`crate::poster`]) are cached alongside so transitions
//! can slide to/from them. Thread-safe; [`Preloader::get`] returns
//! [`None`] on miss.

use crate::config::ConfigStore;
use crate::image::{is_media_wallpaper, load_image, DecodedImage};
use crate::poster;
use crate::util::expand_home;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::RwLock;

/// Thread-safe decode cache for static wallpapers + video posters.
pub struct Preloader {
    cache: RwLock<HashMap<String, Arc<DecodedImage>>>,
    posters: RwLock<HashMap<String, Arc<DecodedImage>>>,
    threads: usize,
}

impl Preloader {
    /// `threads == 0` selects hardware concurrency, falling back to 4
    /// (mirrors the C++ ctor).
    pub fn new(threads: usize) -> Self {
        let threads = if threads == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        } else {
            threads
        };
        Self {
            cache: RwLock::new(HashMap::new()),
            posters: RwLock::new(HashMap::new()),
            threads,
        }
    }

    fn decode_static(path: &str) -> Option<Arc<DecodedImage>> {
        load_image(path).map(|img| Arc::new(img))
    }

    /// Decode one static wallpaper into the cache (no-op for empty,
    /// media, or already-cached paths; missing files never insert).
    pub fn preload_one(&self, path: &str) {
        if path.is_empty() || is_media_wallpaper(path) {
            return;
        }
        {
            let cache = self.cache.read().unwrap();
            if cache.contains_key(path) {
                return;
            }
        }
        // Mirror C++ withFallback: try the ~/$HOME-expanded path first,
        // then the literal path.
        let expanded = expand_home(path);
        let first = if expanded.is_empty() { path } else { &expanded };
        let mut img = Self::decode_static(first);
        if img.is_none() && !expanded.is_empty() && expanded != path {
            img = Self::decode_static(path);
        }
        if let Some(img) = img {
            self.cache.write().unwrap().insert(path.to_string(), img);
        }
    }

    /// Extract one video poster into the poster cache (no-op for empty,
    /// non-media, or already-cached paths).
    pub fn preload_poster_one(&self, path: &str) {
        if path.is_empty() || !is_media_wallpaper(path) {
            return;
        }
        {
            let posters = self.posters.read().unwrap();
            if posters.contains_key(path) {
                return;
            }
        }
        let expanded = expand_home(path);
        let first = if expanded.is_empty() { path } else { &expanded };
        let mut img = poster::load(first).map(Arc::new);
        if img.is_none() && !expanded.is_empty() && expanded != path {
            img = poster::load(path).map(Arc::new);
        }
        if let Some(img) = img {
            self.posters.write().unwrap().insert(path.to_string(), img);
        }
    }

    fn run_pool(&self, jobs: &[String], f: fn(&Self, &str)) {
        if jobs.is_empty() {
            return;
        }
        let workers = self.threads.clamp(1, jobs.len());
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= jobs.len() {
                        break;
                    }
                    f(self, &jobs[i]);
                });
            }
        });
    }

    /// Preload every wallpaper referenced by any monitor config: statics
    /// through [`Self::preload_one`], media through
    /// [`Self::preload_poster_one`]. Posters (~250ms ffmpeg each) share
    /// the pool so startup stays fast.
    pub fn preload_all(&self, store: &ConfigStore) {
        let mut wanted = Vec::new();
        let mut wanted_media = Vec::new();
        for mon in store.monitors() {
            for (_ws, path) in store.wallpapers_for(&mon) {
                if path.is_empty() {
                    continue;
                }
                if is_media_wallpaper(&path) {
                    wanted_media.push(path);
                } else {
                    wanted.push(path);
                }
            }
        }
        wanted.sort();
        wanted.dedup();
        wanted_media.sort();
        wanted_media.dedup();
        self.run_pool(&wanted, Self::preload_one);
        self.run_pool(&wanted_media, Self::preload_poster_one);
    }

    /// Cached static wallpaper, or [`None`] on miss.
    pub fn get(&self, path: &str) -> Option<Arc<DecodedImage>> {
        self.cache.read().unwrap().get(path).cloned()
    }

    /// Cached video poster, or [`None`] on miss.
    pub fn get_poster(&self, path: &str) -> Option<Arc<DecodedImage>> {
        self.posters.read().unwrap().get(path).cloned()
    }

    /// Total entries across both caches.
    pub fn size(&self) -> usize {
        self.cache.read().unwrap().len() + self.posters.read().unwrap().len()
    }
}
