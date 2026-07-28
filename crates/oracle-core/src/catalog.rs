//! Discovery of captured modules on disk.
//!
//! The captured artifacts are not vendored here: they live in the whatsapp-rust
//! checkout under `docs/captured-js/wasm/`. Keeping them there avoids a second
//! copy drifting from the capture that the protocol docs refer to.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Environment variable pointing at the directory holding captured `.wasm`
/// files. Overrides the default sibling-checkout lookup.
pub const DIR_ENV: &str = "WA_WASM_DIR";

/// Where the captures sit relative to a checkout root.
const CAPTURE_SUFFIX: &str = "whatsapp-rust/docs/captured-js/wasm";

/// How far to walk up looking for a sibling whatsapp-rust checkout. Deep enough
/// to cover a test run, whose working directory is the crate rather than the
/// workspace root.
const MAX_ANCESTOR_WALK: usize = 5;

/// Walks up from the crate directory and the working directory looking for the
/// capture path. Two anchors are needed because `cargo run` and `cargo test`
/// have different working directories, and only the crate directory is stable
/// across both.
fn find_capture_dir() -> Option<PathBuf> {
    let anchors = [
        PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        std::env::current_dir().ok()?,
    ];

    anchors
        .iter()
        .flat_map(|anchor| anchor.ancestors().take(MAX_ANCESTOR_WALK))
        .map(|ancestor| ancestor.join(CAPTURE_SUFFIX))
        .find(|candidate| candidate.is_dir())
}

/// A captured module found on disk.
#[derive(Debug, Clone)]
pub struct CapturedModule {
    /// The file stem, which for WhatsApp Web assets is its content hash id.
    pub id: String,
    pub path: PathBuf,
    pub size: u64,
}

/// The set of captured modules available to this run.
#[derive(Debug, Clone)]
pub struct Catalog {
    dir: PathBuf,
    modules: Vec<CapturedModule>,
}

impl Catalog {
    /// Resolves the capture directory from `WA_WASM_DIR`, then by walking up
    /// from this crate and the working directory looking for a sibling
    /// whatsapp-rust checkout, and lists every `.wasm` in it.
    pub fn discover() -> Result<Self> {
        let dir = match std::env::var_os(DIR_ENV) {
            Some(dir) => PathBuf::from(dir),
            None => find_capture_dir().with_context(|| {
                format!(
                    "no capture directory found; set {DIR_ENV} to the directory holding the \
                     captured .wasm files"
                )
            })?,
        };
        Self::from_dir(dir)
    }

    pub fn from_dir(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("reading capture directory {}", dir.display()))?;

        let mut modules = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "wasm") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_owned();
            modules.push(CapturedModule {
                id,
                size: entry.metadata()?.len(),
                path,
            });
        }
        modules.sort_by_key(|module| module.size);

        Ok(Self { dir, modules })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn modules(&self) -> &[CapturedModule] {
        &self.modules
    }

    /// Resolves a user-supplied target: either a path to a `.wasm` file or the
    /// id of a catalogued module.
    pub fn resolve(&self, target: &str) -> Result<CapturedModule> {
        let as_path = Path::new(target);
        if as_path.is_file() {
            return Ok(CapturedModule {
                id: as_path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or(target)
                    .to_owned(),
                size: as_path.metadata()?.len(),
                path: as_path.to_path_buf(),
            });
        }

        self.modules
            .iter()
            .find(|module| module.id == target)
            .cloned()
            .with_context(|| {
                format!(
                    "no module `{target}` in {} and no such file",
                    self.dir.display()
                )
            })
    }
}
