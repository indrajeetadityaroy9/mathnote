//! The verified local Tectonic resources: bundle files and the xelatex format.
//!
//! Public API summary:
//! - [`ensure_cache_warm`] makes the cache usable, bootstrapping it online when needed.
//! - [`open_bundle`] opens the pinned bundle; with `only_cached` it never touches the network.
//! - [`default_cache_dir`] locates the cache.
//!
//! Bundle integrity:
//! One exact format-33 URL is used, and the bundle is rejected unless Tectonic reports the
//! expected cryptographic content digest. The bootstrap builds the LaTeX format if it is missing,
//! typesets a canonical note with the same engine and template as the editor, and renders its
//! pages and every math font the template can use through the page renderer's own lookups. All of
//! that runs with the bundle online, so every file the engine, the template and the renderer need
//! is fetched and verified once. Afterwards the verified cache is used offline.

use std::{
    cell::RefCell,
    collections::HashMap,
    env,
    fmt::{self, Arguments, Display},
    fs,
    io::{self, Cursor, ErrorKind, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use tectonic_bridge_core::CoreBridgeLauncher;
use tectonic_bundles::{Bundle, detect_bundle};
use tectonic_engine_xetex::{FORMAT_SERIAL, TexEngine, TexOutcome};
use tectonic_io_base::{InputHandle, InputOrigin, IoProvider, OpenResult, OutputHandle};
use tectonic_status_base::{MessageKind, StatusBackend};

use crate::dvi::{BundleFontFiles, FontStore, XdvDocument, render_page};
use crate::engine::{Hooks, PRIMARY_INPUT_NAME, XDV_OUTPUT_NAME};
use crate::{document::Document, latex::TEMPLATE_MATH_FONTS, latex::emit_latex};

const FORMAT_VERSION: u32 = 33;
// The pinned bundle's format files must match the linked engine.
const _: () = assert!(FORMAT_VERSION == FORMAT_SERIAL);
/// Name of the LaTeX format, as the engine requests it.
pub(crate) const FORMAT_NAME: &str = "latex";
const BUNDLE_URL: &str = "https://data1b.fullyjustified.net/tlextras-2022.0r0.tar";
const BUNDLE_DIGEST: &str = "6ffe055852f8faf66c0acbe1a7fb27f87b869a90bad1204f3bf4d9683f597c7c";
const CACHE_READY_MARKER: &str = "bundle-v33.ready";
const CACHE_SCHEMA_VERSION: u32 = 3;
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
    /// The warm-up failed; `details` are the engine's or the page renderer's messages.
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

/// Whether the cache in `cache_dir` carries a valid ready marker.
pub(crate) fn cache_is_warmed(cache_dir: &Path) -> bool {
    cache_marker_is_valid(cache_dir)
}

/// Make the cache usable: bootstrap it when its ready marker is missing, and rebuild it once
/// when the marked cache no longer opens.
///
/// One caller at a time per process: concurrent warm-ups race on fetching the same bundle
/// files, and the bundle cache reports a file another fetch is still writing as unavailable.
/// Later callers wait, then find the cache ready.
pub fn ensure_cache_warm(cache_dir: &Path) -> Result<(), CacheError> {
    static WARMING: Mutex<()> = Mutex::new(());
    let _warming = WARMING.lock().unwrap_or_else(PoisonError::into_inner);
    if cache_marker_is_valid(cache_dir) {
        if open_bundle(cache_dir, true).is_ok() {
            return Ok(());
        }
        invalidate_cache(cache_dir);
    }
    warm_cache(cache_dir)
}

/// Compiled format files, one per format name for the pinned bundle:
/// `<bundle digest>-<format>-<serial>.fmt`.
pub(crate) struct FormatCache {
    dir: PathBuf,
}

impl FormatCache {
    pub(crate) fn new(cache_dir: &Path) -> Self {
        Self {
            dir: formats_dir(cache_dir),
        }
    }

    /// Path of format `name`, ignoring any extensions (`latex.fmt` is `latex`).
    fn path(&self, name: &str) -> PathBuf {
        let stem = name.split('.').next().unwrap_or(name);
        self.dir
            .join(format!("{BUNDLE_DIGEST}-{stem}-{FORMAT_VERSION}.fmt"))
    }
}

impl IoProvider for FormatCache {
    fn input_open_format(
        &mut self,
        name: &str,
        _status: &mut dyn StatusBackend,
    ) -> OpenResult<InputHandle> {
        match fs::read(self.path(name)) {
            Ok(data) => OpenResult::Ok(InputHandle::new_read_only(
                name,
                Cursor::new(data),
                InputOrigin::Other,
            )),
            Err(error) if error.kind() == ErrorKind::NotFound => OpenResult::NotAvailable,
            Err(error) => OpenResult::Err(error.into()),
        }
    }

    /// Written to a unique temporary file and renamed into place, since engine processes may
    /// read the format concurrently.
    fn write_format(
        &mut self,
        name: &str,
        data: &[u8],
        _status: &mut dyn StatusBackend,
    ) -> tectonic_errors::Result<()> {
        fs::create_dir_all(&self.dir)?;
        replace_atomically(&self.path(name), "fmt", data)?;
        Ok(())
    }
}

/// Build the format if it is missing, typeset the canonical note and render its pages, and load
/// the template's math fonts, all with the bundle online, then mark the cache ready.
fn warm_cache(cache_dir: &Path) -> Result<(), CacheError> {
    let document = Document::parse(CACHE_WARMUP_NOTE).map_err(|error| CacheError::Bootstrap {
        message: format!("canonical cache warm-up note is invalid: {error}"),
        details: Vec::new(),
    })?;
    let latex = emit_latex(&document);

    let mut status = CapturingStatus::default();
    if FormatCache::new(cache_dir)
        .input_open_format(FORMAT_NAME, &mut status)
        .is_not_available()
    {
        // Tectonic's format recipe: initex on `\input tectonic-format-<name>.tex`.
        let primary = format!("\\input tectonic-format-{FORMAT_NAME}.tex");
        let outputs = warmup_tex(
            cache_dir,
            primary.into_bytes(),
            true,
            "UNUSED.fmt",
            "texput",
        )?;
        let format = outputs
            .into_iter()
            .find_map(|(name, data)| name.ends_with(".fmt").then_some(data))
            .ok_or_else(|| CacheError::Bootstrap {
                message: String::from("the format build wrote no format file"),
                details: Vec::new(),
            })?;
        FormatCache::new(cache_dir)
            .write_format(FORMAT_NAME, &format, &mut status)
            .map_err(|error| CacheError::Directory {
                path: formats_dir(cache_dir),
                message: format!("{error:#}"),
            })?;
    }

    let outputs = warmup_tex(
        cache_dir,
        latex.source().as_bytes().to_vec(),
        false,
        FORMAT_NAME,
        PRIMARY_INPUT_NAME,
    )?;
    let xdv = outputs
        .get(XDV_OUTPUT_NAME)
        .ok_or_else(|| CacheError::Bootstrap {
            message: String::from("the warm-up note produced no XDV output"),
            details: Vec::new(),
        })?;

    // Render through the page renderer's own lookups, then load the math fonts at every size
    // the template can produce (script-script digits use cmr6), so their files are cached too.
    let mut fonts = FontStore::new(BundleFontFiles::new(open_bundle(cache_dir, false)?));
    let mut pages = XdvDocument::new();
    pages.update(xdv, None);
    for index in 0..pages.page_count() {
        if let Some(page) = pages.page(index) {
            render_page(page, &mut fonts, 0.25);
        }
    }
    fonts.preload(TEMPLATE_MATH_FONTS);
    let mut missing = pages.take_warnings();
    missing.extend(fonts.take_warnings());
    if pages.page_count() == 0 || !missing.is_empty() {
        return Err(CacheError::Bootstrap {
            message: String::from("the bundle lacks files the page renderer needs"),
            details: missing,
        });
    }

    let marker = cache_dir.join(CACHE_READY_MARKER);
    fs::write(&marker, cache_marker_contents()).map_err(|error| CacheError::Directory {
        path: marker,
        message: error.to_string(),
    })
}

/// Run XeTeX (in initex mode to build a format) in this process on `primary` with the bundle
/// online; returns every file it wrote. Its terminal output is discarded.
fn warmup_tex(
    cache_dir: &Path,
    primary: Vec<u8>,
    initex: bool,
    format_name: &str,
    input_name: &str,
) -> Result<HashMap<String, Vec<u8>>, CacheError> {
    let outputs = Rc::new(RefCell::new(HashMap::new()));
    let mut hooks = Hooks(WarmupIo {
        primary,
        outputs: Rc::clone(&outputs),
        formats: FormatCache::new(cache_dir),
        bundle: open_bundle(cache_dir, false)?,
    });
    let mut status = CapturingStatus::default();
    let outcome = {
        let mut launcher = CoreBridgeLauncher::new(&mut hooks, &mut status);
        TexEngine::default()
            .initex_mode(initex)
            .halt_on_error_mode(true)
            .process(&mut launcher, format_name, input_name)
    };
    match outcome {
        Ok(TexOutcome::Spotless | TexOutcome::Warnings) => {}
        Ok(TexOutcome::Errors) => {
            return Err(CacheError::Bootstrap {
                message: String::from("the TeX engine reported errors"),
                details: status.messages,
            });
        }
        Err(error) => {
            return Err(CacheError::Bootstrap {
                message: format!("{error:#}"),
                details: status.messages,
            });
        }
    }
    drop(hooks);
    Ok(Rc::try_unwrap(outputs)
        .map(RefCell::into_inner)
        .unwrap_or_default())
}

type Outputs = Rc<RefCell<HashMap<String, Vec<u8>>>>;

/// The warm-up runs' files: the primary input and outputs in memory (outputs can be read back,
/// as LaTeX re-reads its `.aux`), formats from the cache, everything else from the bundle.
struct WarmupIo {
    primary: Vec<u8>,
    outputs: Outputs,
    formats: FormatCache,
    bundle: Box<dyn Bundle>,
}

impl IoProvider for WarmupIo {
    fn output_open_name(&mut self, name: &str) -> OpenResult<OutputHandle> {
        OpenResult::Ok(OutputHandle::new(
            name,
            MemoryOutput {
                name: name.to_owned(),
                data: Vec::new(),
                outputs: Rc::clone(&self.outputs),
            },
        ))
    }

    fn output_open_stdout(&mut self) -> OpenResult<OutputHandle> {
        OpenResult::Ok(OutputHandle::new("stdout", io::sink()))
    }

    fn input_open_name(
        &mut self,
        name: &str,
        status: &mut dyn StatusBackend,
    ) -> OpenResult<InputHandle> {
        if let Some(data) = self.outputs.borrow().get(name) {
            return OpenResult::Ok(InputHandle::new(
                name,
                Cursor::new(data.clone()),
                InputOrigin::Other,
            ));
        }
        self.bundle.input_open_name(name, status)
    }

    fn input_open_primary(&mut self, _status: &mut dyn StatusBackend) -> OpenResult<InputHandle> {
        OpenResult::Ok(InputHandle::new(
            PRIMARY_INPUT_NAME,
            Cursor::new(self.primary.clone()),
            InputOrigin::Other,
        ))
    }

    fn input_open_format(
        &mut self,
        name: &str,
        status: &mut dyn StatusBackend,
    ) -> OpenResult<InputHandle> {
        self.formats.input_open_format(name, status)
    }
}

/// An output file kept in memory; stored in [`WarmupIo::outputs`] when the engine closes it.
struct MemoryOutput {
    name: String,
    data: Vec<u8>,
    outputs: Outputs,
}

impl Write for MemoryOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for MemoryOutput {
    fn drop(&mut self) {
        self.outputs.borrow_mut().insert(
            std::mem::take(&mut self.name),
            std::mem::take(&mut self.data),
        );
    }
}

/// Open the pinned bundle and verify its digest. `only_cached` locks it to the local cache.
pub fn open_bundle(cache_dir: &Path, only_cached: bool) -> Result<Box<dyn Bundle>, CacheError> {
    for path in [cache_dir.to_path_buf(), formats_dir(cache_dir)] {
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

fn formats_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("formats")
}

/// Write `data` to a temporary file unique per process and call, then rename it over `path`, so
/// concurrent readers (threads and other processes) see the old or the new file, never a partial
/// one. The temporary file's extension is `<staged_ext>-<pid>-<n>.tmp`.
fn replace_atomically(path: &Path, staged_ext: &str, data: &[u8]) -> io::Result<()> {
    static STAGED: AtomicU64 = AtomicU64::new(0);
    let staged = path.with_extension(format!(
        "{staged_ext}-{}-{}.tmp",
        std::process::id(),
        STAGED.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&staged, data)?;
    fs::rename(&staged, path)
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
            replace_atomically(
                &path.with_extension("lock"),
                "lock",
                now.to_string().as_bytes(),
            )
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
    let _ = fs::remove_dir_all(formats_dir(cache_dir));
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
    fn report(
        &mut self,
        kind: MessageKind,
        args: Arguments<'_>,
        err: Option<&tectonic_errors::Error>,
    ) {
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
