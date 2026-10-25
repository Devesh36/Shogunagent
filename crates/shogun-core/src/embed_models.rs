//! The local embedding model on a server (E24): where its files live, how an operator fetches
//! them, and how a process loads them.
//!
//! **Nothing downloads at run time.** The only download is `shogun models fetch` (CLI, built with
//! `embed-local`), an explicit operator step: it pulls the pinned files of a
//! [`shogun_memory::embed::ModelSpec`] from its repository at its pinned commit through the traced
//! egress ([`crate::model_asset`]), checks each SHA-256, and moves them into place. A process that
//! starts with `embed-local` only *looks* for those files ([`load_configured`]); missing files or
//! a missing ONNX Runtime library leave search lexical with one line on stderr, never an error.
//!
//! Layout: `<models dir>/<model name>/{model.onnx, tokenizer.json}`, the models dir being
//! `SHOGUN_MODELS_DIR`, else `$XDG_DATA_HOME/shogun/models` (`dirs::data_local_dir`), else
//! `./models`. `SHOGUN_EMBED_MODEL` picks another known model by name; the default is
//! [`shogun_memory::embed::DEFAULT_MODEL`].

use std::path::{Path, PathBuf};

use shogun_memory::embed::{model_spec, ModelSpec, DEFAULT_MODEL};

/// Overrides the models directory.
pub const MODELS_DIR_VAR: &str = "SHOGUN_MODELS_DIR";
/// Picks the model by name (`nomic-embed-text-v1.5`); unset = the default.
pub const MODEL_VAR: &str = "SHOGUN_EMBED_MODEL";

/// Where models live: `SHOGUN_MODELS_DIR`, else the user data dir, else `./models`.
pub fn models_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(MODELS_DIR_VAR).filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    dirs::data_local_dir()
        .map(|dir| dir.join("shogun").join("models"))
        .unwrap_or_else(|| PathBuf::from("models"))
}

/// The configured model: `SHOGUN_EMBED_MODEL` when it names a known model, else the default.
/// An unknown name is an error, not a silent fallback to another space.
pub fn configured_spec() -> Result<&'static ModelSpec, String> {
    match std::env::var(MODEL_VAR) {
        Ok(name) if !name.trim().is_empty() => model_spec(name.trim())
            .ok_or_else(|| format!("{MODEL_VAR} names no known model; `shogun models list` shows them")),
        _ => Ok(DEFAULT_MODEL),
    }
}

/// `<root>/<model name>`.
pub fn model_dir(root: &Path, spec: &ModelSpec) -> PathBuf {
    root.join(spec.name)
}

/// Whether every file of `spec` is present under `root`.
pub fn installed(root: &Path, spec: &ModelSpec) -> bool {
    let dir = model_dir(root, spec);
    !spec.files.is_empty() && spec.files.iter().all(|file| dir.join(file.local).is_file())
}

/// The download URL of one file: the repository at the pinned commit (never a moving branch).
pub fn file_url(spec: &ModelSpec, remote: &str) -> String {
    format!("https://huggingface.co/{}/resolve/{}/{remote}", spec.repo, spec.revision)
}

/// Lowercase hex SHA-256 of a file, streamed.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Check every installed file of `spec` against its pinned hash.
pub fn verify(root: &Path, spec: &ModelSpec) -> Result<(), String> {
    let dir = model_dir(root, spec);
    for file in spec.files {
        let path = dir.join(file.local);
        let got = sha256_file(&path).map_err(|_| format!("{} is missing or unreadable", file.local))?;
        if got != file.sha256 {
            return Err(format!("{} does not match its pinned sha256", file.local));
        }
    }
    Ok(())
}

/// What one fetch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReport {
    pub dir: PathBuf,
    /// Files downloaded now.
    pub downloaded: Vec<&'static str>,
    /// Files already present with the pinned hash.
    pub kept: Vec<&'static str>,
}

/// Download `spec`'s files into `<root>/<name>/`, skipping files already present with the pinned
/// hash. Each file streams to a temporary in the same directory, is checked against its SHA-256 and
/// is then renamed into place; a failure leaves no partial file. Every request goes through the
/// traced egress and is recorded in `trace` before it is sent.
#[cfg(feature = "net")]
pub fn fetch(
    root: &Path,
    spec: &'static ModelSpec,
    trace: &dyn crate::llm::traceability::TraceabilitySink,
) -> Result<FetchReport, String> {
    if spec.files.is_empty() {
        return Err(format!("{} is not fetchable; place its files by hand", spec.name));
    }
    let dir = model_dir(root, spec);
    std::fs::create_dir_all(&dir).map_err(|_| "cannot create the models directory".to_string())?;
    let mut report = FetchReport { dir: dir.clone(), downloaded: Vec::new(), kept: Vec::new() };
    for file in spec.files {
        let dest = dir.join(file.local);
        if dest.is_file() && sha256_file(&dest).ok().as_deref() == Some(file.sha256) {
            report.kept.push(file.local);
            continue;
        }
        let tmp = dir.join(format!(".{}.part", file.local));
        let result = crate::model_asset::download_verified(
            &file_url(spec, file.remote),
            &tmp,
            &dest,
            Some(file.sha256),
            trace,
        );
        if let Err(error) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("{}: {error}", file.local));
        }
        report.downloaded.push(file.local);
    }
    Ok(report)
}

/// The fetch command's traceability sink: one stderr line per request (method, host and path of a
/// pinned public model file; nothing of any principal's).
#[cfg(feature = "net")]
pub struct OperatorTrace;

#[cfg(feature = "net")]
impl crate::llm::traceability::TraceabilitySink for OperatorTrace {
    fn record(&self, rec: crate::llm::traceability::TraceRecord) {
        // log-hygiene-ok: a pinned public model URL, never captured text
        eprintln!("shogun models: GET {}", rec.url.as_deref().unwrap_or(&rec.destination));
    }
}

/// Load `spec` from `root` (feature `embed-local`). `Err` names the reason in one line.
#[cfg(feature = "embed-local")]
pub fn load(
    root: &Path,
    spec: &'static ModelSpec,
) -> Result<std::sync::Arc<dyn shogun_memory::embed::Embedder>, String> {
    if !installed(root, spec) {
        return Err(format!(
            "{} is not installed under {}; run `shogun models fetch`",
            spec.name,
            root.display()
        ));
    }
    shogun_memory::embed_onnx::OnnxEmbedder::load_dir(spec, model_dir(root, spec))
        .map(|embedder| std::sync::Arc::new(embedder) as std::sync::Arc<dyn shogun_memory::embed::Embedder>)
        .map_err(|error| format!("{} failed to load: {error}", spec.name))
}

/// The embedder a server process should attach, if any, with a one-line reason for stderr either
/// way. Without `embed-local` the answer is always `None` (lexical search).
pub fn load_configured() -> (Option<std::sync::Arc<dyn shogun_memory::embed::Embedder>>, String) {
    #[cfg(feature = "embed-local")]
    {
        let spec = match configured_spec() {
            Ok(spec) => spec,
            Err(why) => return (None, format!("local embeddings off: {why}")),
        };
        match load(&models_dir(), spec) {
            Ok(embedder) => (Some(embedder), format!("local embeddings on: {}", spec.space_id)),
            Err(why) => (None, format!("local embeddings off: {why}; search is lexical")),
        }
    }
    #[cfg(not(feature = "embed-local"))]
    {
        (None, "local embeddings off: built without `embed-local`; search is lexical".to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use shogun_memory::embed::NOMIC_EMBED_TEXT_V15;

    #[test]
    fn urls_are_pinned_to_the_commit() {
        let url = file_url(&NOMIC_EMBED_TEXT_V15, "tokenizer.json");
        assert!(url.starts_with("https://huggingface.co/nomic-ai/nomic-embed-text-v1.5/resolve/"));
        assert!(url.contains(NOMIC_EMBED_TEXT_V15.revision));
        assert!(!url.contains("/main/"));
    }

    #[test]
    fn installation_and_verification_follow_the_pins() {
        let root = std::env::temp_dir().join(format!("shogun_embed_models_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(!installed(&root, &NOMIC_EMBED_TEXT_V15));
        let dir = model_dir(&root, &NOMIC_EMBED_TEXT_V15);
        std::fs::create_dir_all(&dir).unwrap();
        for file in NOMIC_EMBED_TEXT_V15.files {
            std::fs::write(dir.join(file.local), b"not the model").unwrap();
        }
        assert!(installed(&root, &NOMIC_EMBED_TEXT_V15), "present");
        assert!(verify(&root, &NOMIC_EMBED_TEXT_V15).is_err(), "but not the pinned bytes");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sha256_of_a_known_input() {
        let path = std::env::temp_dir().join(format!("shogun_embed_sha_{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(not(feature = "embed-local"))]
    #[test]
    fn without_the_feature_search_stays_lexical() {
        let (embedder, why) = load_configured();
        assert!(embedder.is_none());
        assert!(why.contains("lexical"));
    }
}
