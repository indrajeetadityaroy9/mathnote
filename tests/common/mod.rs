use mathnote::cache::{default_cache_dir, ensure_cache_warm};
use mathnote::driver::{DriverConfig, TexDriver};

/// A snapshot driver running this crate's binary as its engine, over the warmed default cache.
pub fn driver() -> TexDriver {
    let cache_dir = default_cache_dir();
    ensure_cache_warm(&cache_dir).expect("warm Tectonic cache");
    TexDriver::new(DriverConfig {
        engine_exe: env!("CARGO_BIN_EXE_mathnote").into(),
        cache_dir,
    })
}
