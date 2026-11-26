//! `victory preload`: translate all shaders from the game directory into the cache
//! so the first run doesn't spend time translating on the fly.

use std::path::PathBuf;

use anyhow::Result;
use victory_core::Config;
use victory_hle::modules::d3d12x::use_shader_cache;

/// Run the preload: scan game directory for all shaders and warm the cache.
pub fn run(validate: bool, pipelines: bool, jobs: Option<usize>) -> Result<()> {
    let cfg = Config::load()?;
    use_shader_cache(cfg.cache_subdir("shaders")?);

    let paths = crate::default_paths(vec![])?;
    let all_flag = PathBuf::from("all"); // sentinel for "all hashes"

    // Apply the same env vars as translate for consistency
    if let Some(n) = std::env::var("VICTORY_VIEW_LIMIT").ok().and_then(|v| v.parse().ok()) {
        victory_shader::emit::set_view_heap_limit(n);
    }
    if let Some(n) = std::env::var("VICTORY_HOST_WAVE").ok().and_then(|v| v.parse().ok()) {
        victory_shader::emit::set_host_max_wave(n);
    }

    println!("preload      scanning {} path(s) for shaders", paths.len());
    for p in &paths {
        println!("  {}", p.display());
    }

    translate::warm(paths, &all_flag, jobs)?;

    if validate || pipelines {
        println!("preload      done (cache warmed)");
    }
    Ok(())
}

// Re-export for convenience
mod translate {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;
    use blake3;
    use walkdir;

    use crate::shader::{MAX_FILE, sources};
    use victory_hle::modules::d3d12x::warm_shader;

    /// Warm the shader cache with all containers from `paths`.
    /// `file` is a PathBuf sentinel; if it equals "all", translate everything.
    pub(super) fn warm(paths: Vec<PathBuf>, file: &Path, jobs: Option<usize>) -> Result<()> {
        let all = file.as_os_str() == "all";
        let mut wanted: HashSet<String> = HashSet::new();
        if !all {
            let text = std::fs::read_to_string(file)?;
            for line in text.lines() {
                for word in line.split(|c: char| !c.is_ascii_hexdigit()) {
                    if word.len() == 16 {
                        wanted.insert(word.to_ascii_lowercase());
                    }
                }
            }
            anyhow::ensure!(!wanted.is_empty(), "no 16-digit shader hash in {}", file.display());
        }

        let limit = std::env::var("VICTORY_WARM_SECONDS").ok().and_then(|v| v.trim().parse::<u64>().ok());
        let threads = jobs.unwrap_or(4).max(1);
        let began = std::time::Instant::now();
        let done = Mutex::new(HashSet::new());
        let counts = [const { AtomicUsize::new(0) }; 5];

        for root in &paths {
            for entry in walkdir::WalkDir::new(root).into_iter().filter_map(Result::ok) {
                if !entry.file_type().is_file() || entry.metadata().map_or(0, |m| m.len()) > MAX_FILE {
                    continue;
                }
                let Ok(data) = std::fs::read(entry.path()) else {
                    continue;
                };
                let jobs = sources(&data);
                let next = AtomicUsize::new(0);
                std::thread::scope(|s| {
                    for _ in 0..threads.min(jobs.len()) {
                        s.spawn(|| {
                            while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                                let Some(bytes) = job.bytes() else {
                                    continue;
                                };
                                let hash = blake3::hash(&bytes).to_hex()[..16].to_owned();
                                if limit.is_some_and(|n| began.elapsed().as_secs() >= n) {
                                    counts[3].fetch_add(1, Ordering::Relaxed);
                                    break;
                                }
                                if (!all && !wanted.contains(&hash)) || !lock(&done).insert(hash) {
                                    continue;
                                }
                                let n = counts[4].fetch_add(1, Ordering::Relaxed) + 1;
                                if n.is_multiple_of(1000) {
                                    println!("preload      {n} shaders so far, {:.0} s", began.elapsed().as_secs_f64());
                                }
                                let which = match warm_shader(&bytes) {
                                    Ok(true) => 0,
                                    Ok(false) => 1,
                                    Err(_) => 2,
                                };
                                counts[which].fetch_add(1, Ordering::Relaxed);
                            }
                        });
                    }
                });
            }
        }

        let [translated, there, failed] = [0, 1, 2].map(|i| counts[i].load(Ordering::Relaxed));
        let asked = if all { "all".to_owned() } else { wanted.len().to_string() };
        let stopped = if counts[3].load(Ordering::Relaxed) > 0 { " (stopped at time limit)" } else { "" };
        println!(
            "preload      {asked} hashes asked, {} found: {translated} translated now, {there} in cache already, {failed} do not translate; {:.1} s on {threads} threads{stopped}",
            translated + there + failed,
            began.elapsed().as_secs_f64()
        );
        Ok(())
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }
}