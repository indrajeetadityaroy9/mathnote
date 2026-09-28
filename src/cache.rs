//! The verified local Tectonic resources: bundle files and the xelatex format.
//!
//! Public API summary:
//! - [`ensure_cache_warm`] makes the cache usable, bootstrapping it online when needed.
//! - [`open_bundle`] opens the pinned bundle; with `only_cached` it never touches the network.
//! - [`bundle_digest`] names the cached format files; [`default_cache_dir`] locates the cache.
//!
//! Bundle integrity:
//! One exact format-33 URL is used, and the bundle is rejected unless Tectonic reports the
//! expected cryptographic content digest. The bootstrap typesets a canonical note to PDF, which
//! fetches every file the template, the engine format and the page renderer need. Afterwards the
//! verified cache is used offline.

use std::{
    env,
    fmt::{self, Arguments, Display},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use tectonic::{
    driver::{OutputFormat, ProcessingSessionBuilder},
    io::DigestData,
    status::{MessageKind, StatusBackend},
};
use tectonic_bundles::{Bundle, detect_bundle};

use crate::{document::Document, latex::emit_latex};

const FORMAT_VERSION: u32 = 33;
const BUNDLE_URL: &str = "https://data1b.fullyjustified.net/tlextras-2022.0r0.tar";
const BUNDLE_DIGEST: &str = "6ffe055852f8faf66c0acbe1a7fb27f87b869a90bad1204f3bf4d9683f597c7c";
const CACHE_READY_MARKER: &str = "bundle-v33.ready";
const CACHE_SCHEMA_VERSION: u32 = 2;
const CACHE_WARMUP_NOTE: &str = concat!(
    "$α β γ δ ε ζ η θ ι κ λ μ ν ξ π ρ σ τ υ φ χ ψ ω$\n\n",
    "$Γ Δ Θ Λ Ξ Π Σ Υ Φ Ψ Ω ϵ ϑ ϖ ϱ$\n\n",
    "$x over 2$ $root of 81$ $integral of x$ $x times y$\n\n",
    "$x is less than or equal to y$ $x is greater than or equal to y$\n\n",
    "$x is not equal to y$ $x plus or minus y$ $infinity$\n",
);

/// Why the cache could not be prepared or opened.
#[derive(Debug)]
pub enum CacheError {
    Directory {
        path: PathBuf,
        message: String,
    },
    Bundle(String),
    /// The bootstrap document failed to typeset; the messages are Tectonic's.
    Bootstrap {
        message: String,
        details: Vec<String>,
    },
}

impl Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Directory { path, message } => write!(
                f,
                "failed to prepare cache directory {}: {message}",
                path.display()
            ),
            Self::Bundle(message) => write!(f, "failed to load Tectonic bundle: {message}"),
            Self::Bootstrap { message, details } => match details.last() {
                Some(detail) => write!(f, "could not prepare LaTeX resources: {message}: {detail}"),
                None => write!(f, "could not prepare LaTeX resources: {message}"),
            },
        }
    }
}

impl std::error::Error for CacheError {}

pub fn cache_is_warmed() -> bool {
    cache_marker_is_valid(&default_cache_dir())
}

/// Make the cache usable: bootstrap it when its ready marker is missing, and rebuild it once
/// when the marked cache no longer opens.
pub fn ensure_cache_warm(cache_dir: &Path) -> Result<(), CacheError> {
    if cache_marker_is_valid(cache_dir) {
        if open_bundle(cache_dir, true).is_ok() {
            return Ok(());
        }
        invalidate_cache(cache_dir);
    }
    warm_cache(cache_dir)
}

/// The pinned bundle digest, as used to name cached format files.
pub fn bundle_digest() -> DigestData {
    BUNDLE_DIGEST
        .parse()
        .expect("BUNDLE_DIGEST is a valid SHA-256 hex digest")
}

/// Typeset the canonical note online, fetching and verifying every resource it needs, then mark
/// the cache ready.
fn warm_cache(cache_dir: &Path) -> Result<(), CacheError> {
    let document = Document::parse(CACHE_WARMUP_NOTE).map_err(|error| CacheError::Bootstrap {
        message: format!("canonical cache warm-up note is invalid: {error}"),
        details: Vec::new(),
    })?;
    let latex = emit_latex(&document);

    let bundle = open_bundle(cache_dir, false)?;
    let mut status = CapturingStatus::default();
    let mut builder = ProcessingSessionBuilder::default();
    builder
        .bundle(bundle)
        .primary_input_buffer(latex.source().as_bytes())
        .tex_input_name("mathnote.tex")
        .filesystem_root(cache_dir.join("sandbox"))
        .format_name("latex")
        .format_cache_path(cache_dir.join("formats"))
        .print_stdout(false)
        .output_format(OutputFormat::Pdf)
        .do_not_write_output_files();
    let result = builder
        .create(&mut status)
        .and_then(|mut session| session.run(&mut status));
    if let Err(error) = result {
        return Err(CacheError::Bootstrap {
            message: error.to_string(),
            details: status.messages,
        });
    }

    let marker = cache_dir.join(CACHE_READY_MARKER);
    fs::write(&marker, cache_marker_contents()).map_err(|error| CacheError::Directory {
        path: marker,
        message: error.to_string(),
    })
}

/// Open the pinned bundle and verify its digest. `only_cached` locks it to the local cache.
pub fn open_bundle(cache_dir: &Path, only_cached: bool) -> Result<Box<dyn Bundle>, CacheError> {
    for path in [
        cache_dir.to_path_buf(),
        cache_dir.join("formats"),
        cache_dir.join("sandbox"),
    ] {
        fs::create_dir_all(&path).map_err(|error| CacheError::Directory {
            path: path.clone(),
            message: error.to_string(),
        })?;
    }

    if only_cached {
        refresh_cached_digest_check(cache_dir).map_err(CacheError::Bundle)?;
    }
    let mut bundle = detect_bundle(
        String::from(BUNDLE_URL),
        only_cached,
        Some(cache_dir.to_path_buf()),
    )
    .map_err(|error| CacheError::Bundle(error.to_string()))?
    .ok_or_else(|| CacheError::Bundle(format!("could not detect bundle source {BUNDLE_URL}")))?;
    let bundle_digest = bundle
        .get_digest()
        .map_err(|error| CacheError::Bundle(format!("could not verify bundle digest: {error}")))?
        .to_string();
    if bundle_digest != BUNDLE_DIGEST {
        return Err(CacheError::Bundle(format!(
            "bundle digest mismatch: expected {BUNDLE_DIGEST}, received {bundle_digest}"
        )));
    }
    Ok(bundle)
}

fn cache_marker_contents() -> String {
    format!("schema={CACHE_SCHEMA_VERSION}\nformat={FORMAT_VERSION}\ndigest={BUNDLE_DIGEST}\n")
}

fn cached_bundle_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("data").join(BUNDLE_DIGEST)
}

/// Keep `tectonic_bundles` from performing its seven-day remote digest refresh in cache-only
/// mode. The digest was verified during bootstrap and is also bound into our ready marker.
///
/// Engine processes and the page renderer open the bundle concurrently, and `tectonic_bundles`
/// reads the lock file back; it is therefore replaced atomically, never rewritten in place.
fn refresh_cached_digest_check(cache_dir: &Path) -> Result<(), String> {
    if !cached_bundle_dir(cache_dir).is_dir() {
        return Err(format!(
            "verified local bundle cache is missing: {}",
            cached_bundle_dir(cache_dir).display()
        ));
    }

    let hashes = cache_dir.join("hashes");
    let entries = fs::read_dir(&hashes)
        .map_err(|error| format!("could not inspect bundle cache metadata: {error}"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?
        .as_secs();

    for entry in entries {
        let path = entry
            .map_err(|error| format!("could not inspect bundle cache metadata: {error}"))?
            .path();
        let skip = path
            .extension()
            .is_some_and(|extension| extension == "lock" || extension == "tmp");
        if skip || !path.is_file() {
            continue;
        }
        if fs::read_to_string(&path).is_ok_and(|contents| contents.trim() == BUNDLE_DIGEST) {
            // Unique per process and call: threads of one process refresh concurrently too.
            static STAGED: AtomicU64 = AtomicU64::new(0);
            let lock = path.with_extension("lock");
            let staged = path.with_extension(format!(
                "lock-{}-{}.tmp",
                std::process::id(),
                STAGED.fetch_add(1, Ordering::Relaxed)
            ));
            fs::write(&staged, now.to_string())
                .and_then(|()| fs::rename(&staged, &lock))
                .map_err(|error| format!("could not lock bundle cache to offline mode: {error}"))?;
            return Ok(());
        }
    }

    Err(String::from(
        "verified bundle digest metadata is missing from the local cache",
    ))
}

fn invalidate_cache(cache_dir: &Path) {
    let _ = fs::remove_file(cache_dir.join(CACHE_READY_MARKER));
    let _ = fs::remove_dir_all(cached_bundle_dir(cache_dir));
    let _ = fs::remove_dir_all(cache_dir.join("formats"));
}

fn cache_marker_is_valid(cache_dir: &Path) -> bool {
    fs::read_to_string(cache_dir.join(CACHE_READY_MARKER))
        .is_ok_and(|contents| contents == cache_marker_contents())
}

pub fn default_cache_dir() -> PathBuf {
    if let Some(project_dirs) = ProjectDirs::from("", "", "mathnote") {
        return project_dirs.cache_dir().join("tectonic");
    }

    if let Ok(dir) = env::var("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("mathnote").join("tectonic");
    }

    env::temp_dir().join("mathnote").join("tectonic")
}

/// Keeps the bootstrap's warning and error messages for [`CacheError::Bootstrap`].
#[derive(Debug, Default)]
struct CapturingStatus {
    messages: Vec<String>,
}

impl StatusBackend for CapturingStatus {
    fn report(&mut self, kind: MessageKind, args: Arguments<'_>, err: Option<&tectonic::Error>) {
        if kind == MessageKind::Note {
            return;
        }
        self.messages.push(match err {
            Some(error) => format!("{args}: {error}"),
            None => args.to_string(),
        });
    }

    fn dump_error_logs(&mut self, output: &[u8]) {
        if let Some(line) = String::from_utf8_lossy(output)
            .lines()
            .find(|line| line.starts_with('!'))
        {
            self.messages.push(line.to_owned());
        }
    }
}
