//! File and folder naming for downloaded artifacts: the resolved
//! `filename_mode`/`filename_template` applied to an item's metadata
//! (AUD-53), the fixed per-artifact suffixes, and the sanitizers. Used by
//! `download`, `annotations --save` and `download reorganize` — the single
//! source of the naming rule.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::config::ctx::Ctx;
use crate::config::paths;

/// Resolved download directory: the settings bundle's `download_dir`,
/// else the platform data dir's `downloads` subfolder.
pub(crate) fn download_dir(ctx: &Ctx) -> Result<PathBuf> {
    let dir = ctx
        .settings_view()?
        .download_dir(None, None)
        .unwrap_or_else(|| paths::data_dir().join("downloads"));
    Ok(expand_tilde(&dir))
}

pub(crate) fn expand_tilde(path: &Path) -> PathBuf {
    match std::env::home_dir() {
        Some(home) => expand_tilde_from(path, &home),
        None => path.to_path_buf(),
    }
}

/// Tilde expansion against an explicit home directory — the test seam, so the
/// component logic is exercised without mutating the process environment.
///
/// `Path::strip_prefix` matches whole components, not text: on Windows both `/`
/// and `\` are separators, so `~/sub` and `~\sub` each split into `["~", "sub"]`
/// and expand the same way. A leading `~name` is a single component and is left
/// untouched — we deliberately don't resolve other users' home directories.
fn expand_tilde_from(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Joins a relative filename onto `dir` with native path separators, even when
/// the filename embeds `/` folder markers (custom naming mode nests the title in
/// subfolders). `Path::join` keeps an embedded `/` verbatim, so on Windows it
/// would produce — and the DB would then store — a mixed `C:\dir\sub/file`;
/// pushing each segment yields a clean `C:\dir\sub\file`. A no-op on Unix, where
/// `/` is already the separator.
pub(crate) fn join_relative(dir: &Path, relative: &str) -> PathBuf {
    let mut path = dir.to_path_buf();
    for segment in relative.split('/').filter(|segment| !segment.is_empty()) {
        path.push(segment);
    }
    path
}

/// Computes the base filename (relative to `download_dir`, may contain `/`
/// folders in custom mode) from explicit naming settings and an item's template
/// values. Shared by [`base_filename`] (current settings) and `download
/// reorganize` (target settings). Errors only for `custom` without a template.
pub(crate) fn resolve_base(
    mode: crate::config::schema::FilenameMode,
    template: Option<&str>,
    max_len: usize,
    asin: &str,
    values: &std::collections::HashMap<&'static str, String>,
) -> Result<String> {
    use crate::config::schema::FilenameMode;
    if mode == FilenameMode::Custom {
        let template = template
            .map(str::trim)
            .filter(|template| !template.is_empty())
            .context(
                "filename_mode is \"custom\" but no filename_template is set \
                 (set settings.<name>.filename_template)",
            )?;
        let expanded = expand_template(template, values, max_len)?;
        if !expanded.is_empty() {
            return Ok(expanded);
        }
        // Every segment collapsed (all-empty variables) — never emit nothing.
        return Ok(
            match values.get("fulltitle").filter(|full| !full.is_empty()) {
                Some(full) => truncate_filename(&unicode_clean(full), max_len),
                None => asin.to_owned(),
            },
        );
    }
    let title = values.get("fulltitle").map(String::as_str).unwrap_or("");
    Ok(sanitize_filename(title, mode, asin, max_len))
}

/// Base filename (without extension), relative to `download_dir`, using the
/// current resolved settings. Falls back to the ASIN when the item is not in the
/// local database (no metadata to name it by).
pub(crate) async fn base_filename(ctx: &Ctx, marketplace: &str, asin: &str) -> Result<String> {
    use crate::config::schema::FilenameMode;
    let view = ctx.settings_view().ok();
    let mode = view
        .as_ref()
        .map(|v| v.filename_mode(None, None))
        .unwrap_or(FilenameMode::Ascii);
    let max_len = view
        .as_ref()
        .map(|v| v.filename_max_length(None, None))
        .unwrap_or(crate::config::resolve::DEFAULT_FILENAME_MAX_LENGTH);
    let template = view.as_ref().and_then(|v| v.filename_template());
    let Some(values) = template_context(ctx, marketplace, asin).await else {
        return Ok(asin.to_owned());
    };
    resolve_base(mode, template.as_deref(), max_len, asin, &values)
}

/// The fixed filename suffix (discriminator + extension) for a download
/// artifact. `ext` is the actual on-disk extension (e.g. `aaxc`/`mp3` for
/// audio) — for reorganize it comes from the existing file, so a renamed
/// (`.mp3`) audio keeps its extension.
pub(crate) fn artifact_suffix(kind: &str, content_format: &str, ext: &str) -> String {
    match kind {
        // Audio keeps the quality segment; the decrypted m4b and any reencode
        // are also kind=audio and differ only by the file extension.
        "audio" if content_format.is_empty() => format!(".{ext}"),
        "audio" => format!(".{content_format}.{ext}"),
        "cover" => format!(".cover_{content_format}.{ext}"),
        "chapter" => format!(".chapters_{content_format}.{ext}"),
        _ => format!(".{ext}"), // pdf and any single-extension artifact
    }
}

/// The key/iv sidecar path for an `(audio, original)` file, derived from its
/// extension: `.aaxc` → `.voucher` (AUD-11 key/iv), `.cenc` → `.wvkey` (AUD-56
/// Widevine content key). A plain `.mp3` (Mpeg fallback / podcast) and every
/// non-original artifact carry no sidecar → `None`. Used by both file-lifecycle
/// commands (reorganize move, `--with-files` delete) so the secret sidecars
/// never orphan (AUD-99).
pub(crate) fn sidecar_path(file: &Path) -> Option<PathBuf> {
    match file.extension().and_then(|ext| ext.to_str()) {
        Some("aaxc") => Some(file.with_extension("voucher")),
        Some("cenc") => Some(file.with_extension("wvkey")),
        _ => None,
    }
}

/// Collects the scalar variable values for a `custom` filename template from
/// the stored item document (plus context). `None` when the item is not in the
/// local database. Keys match [`crate::config::filename_template::TEMPLATE_VARS`].
pub(crate) async fn template_context(
    ctx: &Ctx,
    marketplace: &str,
    asin: &str,
) -> Option<std::collections::HashMap<&'static str, String>> {
    let db = ctx.open_library_db().await.ok()?;
    // Books live in `items`; podcast episodes in `episodes`. Fall back to the
    // episode doc so episodes are named/reorganized by title, not bare ASIN
    // (AUD-100); `parent_asin` lets us group them under their show.
    let (doc_str, parent_asin) = match db.item_doc(asin.to_owned(), marketplace.to_owned()).await {
        Ok(Some(doc)) => (doc, None),
        _ => {
            let (doc, parent) = db
                .episode_doc(asin.to_owned(), marketplace.to_owned())
                .await
                .ok()??;
            (doc, Some(parent))
        }
    };
    let doc: serde_json::Value = serde_json::from_str(&doc_str).ok()?;

    let text = |key: &str| {
        doc.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    // Dates are ISO (`2021-02-25`, `2024-04-17T…`); the year is the first four.
    let year = |key: &str| {
        doc.get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| value.len() >= 4 && value.is_char_boundary(4))
            .map(|value| value[..4].to_owned())
            .unwrap_or_default()
    };
    let runtime = doc
        .get("runtime_length_min")
        .and_then(serde_json::Value::as_i64)
        .map(|minutes| minutes.to_string())
        .unwrap_or_default();

    let mut values = std::collections::HashMap::new();
    values.insert("asin", asin.to_owned());
    values.insert("title", text("title"));
    values.insert("subtitle", text("subtitle"));
    values.insert(
        "fulltitle",
        crate::models::library::build_full_title(&doc).unwrap_or_default(),
    );
    values.insert("account", ctx.account_name().unwrap_or_default());
    values.insert("marketplace", marketplace.to_owned());
    values.insert("publisher", text("publisher_name"));
    values.insert("language", text("language"));
    values.insert("release_year", year("release_date"));
    values.insert("purchase_year", year("purchase_date"));
    values.insert("runtime", runtime);
    values.insert("format", text("format_type"));
    values.insert("type", text("content_delivery_type"));
    values.insert("publication", text("publication_name"));

    // For a podcast episode `publication` (the show) is not on the episode
    // doc — fill it from the parent podcast's title, so a `%publication%/…`
    // template groups episodes per show (AUD-100).
    if let Some(parent) = parent_asin
        && values.get("publication").is_some_and(String::is_empty)
        && let Ok(Some(show)) = db.find_title(parent, marketplace.to_owned()).await
    {
        values.insert("publication", show);
    }
    Some(values)
}

/// Expands a `custom` template into a path relative to `download_dir` (may
/// contain `/` folders). Each segment is sanitized and capped to `max_len`;
/// empty / `.` / `..` segments are dropped and a leading `/` is stripped, so the
/// result can never escape `download_dir`. Errors on an unknown variable or a
/// malformed token.
fn expand_template(
    template: &str,
    values: &std::collections::HashMap<&'static str, String>,
    max_len: usize,
) -> Result<String> {
    crate::config::filename_template::validate(template)
        .map_err(|reason| anyhow::anyhow!("invalid filename_template: {reason}"))?;
    let template = template.strip_prefix('/').unwrap_or(template);
    let mut segments: Vec<String> = Vec::new();
    for raw in template.split('/') {
        // The per-segment sanitize also collapses any `/` a variable value
        // introduced into `_`, so values can never add folders.
        let segment = unicode_clean(&expand_segment(raw, values));
        if segment.is_empty() || segment == "." || segment == ".." {
            continue;
        }
        segments.push(truncate_filename(&segment, max_len));
    }
    Ok(segments.join("/"))
}

/// Expands the `%name%` / `%name!a%` / `%name!u%` tokens in one template segment
/// (`%%` is a literal `%`). `!a` transliterates to ASCII, `!u` (the default)
/// keeps Unicode. The template is validated beforehand (see
/// [`crate::config::filename_template::validate`]), so tokens are well-formed.
fn expand_segment(raw: &str, values: &std::collections::HashMap<&'static str, String>) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            // Unreachable after validation; keep the text verbatim just in case.
            out.push('%');
            out.push_str(after);
            return out;
        };
        let token = &after[..end];
        rest = &after[end + 1..];
        if token.is_empty() {
            out.push('%'); // `%%` → a literal percent sign
            continue;
        }
        let (name, ascii) = match token.split_once('!') {
            Some((name, "a")) => (name, true),
            Some((name, _)) => (name, false),
            None => (token, false),
        };
        let value = values.get(name).map(String::as_str).unwrap_or("");
        // Sanitize per value (so a value can never introduce a `/` and add a
        // folder) and substitute the placeholder when it renders empty, so
        // every variable always yields a token (regular, predictable structure).
        let rendered = if ascii {
            ascii_slug(value)
        } else {
            unicode_clean(value)
        };
        out.push_str(if rendered.is_empty() {
            crate::config::filename_template::EMPTY_PLACEHOLDER
        } else {
            rendered.as_str()
        });
    }
    out.push_str(rest);
    out
}

fn sanitize_filename(
    title: &str,
    mode: crate::config::schema::FilenameMode,
    asin: &str,
    max_len: usize,
) -> String {
    use crate::config::schema::FilenameMode;
    let with_asin = matches!(mode, FilenameMode::AsinAscii | FilenameMode::AsinUnicode);

    let cleaned = match mode {
        FilenameMode::Ascii | FilenameMode::AsinAscii => ascii_slug(title),
        // Custom is expanded in `base_filename` and never reaches here; treat it
        // like unicode as a safe fallback rather than panicking.
        FilenameMode::Unicode | FilenameMode::AsinUnicode | FilenameMode::Custom => {
            unicode_clean(title)
        }
    };
    let cleaned = if cleaned.is_empty() {
        asin.to_owned()
    } else {
        cleaned
    };
    let name = if with_asin {
        format!("{asin}_{cleaned}")
    } else {
        cleaned
    };
    truncate_filename(&name, max_len)
}

/// ASCII slug, mirroring audible-cli's `full_title_slugify`: NFKD-normalize
/// so accents fold to their base letter (`ä`→`a`, `ü`→`u`, `é`→`e`), keep
/// only ASCII, turn spaces into `_`, and drop anything outside
/// `-_.()`+alphanumerics (so `:` and friends vanish without leaving a stray
/// separator). Leading/trailing `_`/`.` are trimmed.
fn ascii_slug(title: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let slug: String = title
        .nfkd()
        .filter(char::is_ascii)
        .map(|c| if c == ' ' { '_' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '(' | ')'))
        .collect();
    slug.trim_matches(|c: char| c == '_' || c == '.').to_owned()
}

/// Unicode-preserving clean: keep spaces and Unicode letters, but replace
/// filesystem-unsafe characters and control chars with `_`.
fn unicode_clean(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    cleaned.trim().trim_matches('.').trim().to_owned()
}

/// Hard-truncates a file name to at most `max_len` bytes on a char
/// boundary (`0` disables). The ASIN prefix (when present) stays, so the
/// name remains unique; the extension is added by the caller.
fn truncate_filename(name: &str, max_len: usize) -> String {
    if max_len == 0 || name.len() <= max_len {
        return name.to_owned();
    }
    let mut end = max_len;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].trim_end().trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tilde expansion is component-based, so a Unix CI never exercises the
    // Windows separator path — these assert the real resolved paths on each
    // platform (not just that it compiles). `expand_tilde_from` takes an
    // explicit home so the test is deterministic and env-free.

    #[cfg(not(windows))]
    #[test]
    fn tilde_expands_against_home() {
        let home = PathBuf::from("/home/x");
        // The `~/Audible` default resolves under the home directory.
        assert_eq!(
            expand_tilde_from(Path::new("~/Audible"), &home),
            PathBuf::from("/home/x/Audible")
        );
        // A bare `~` is the home directory itself.
        assert_eq!(expand_tilde_from(Path::new("~"), &home), home);
        // Backslash is not a separator on Unix, so `~\x` is one literal
        // component and is left untouched.
        assert_eq!(
            expand_tilde_from(Path::new(r"~\Audiobooks"), &home),
            PathBuf::from(r"~\Audiobooks")
        );
        // An absolute path and a `~name` component are never rewritten.
        assert_eq!(
            expand_tilde_from(Path::new("/data/x"), &home),
            PathBuf::from("/data/x")
        );
        assert_eq!(
            expand_tilde_from(Path::new("~alice"), &home),
            PathBuf::from("~alice")
        );
    }

    // Windows accepts both separators after the tilde. `~\Audiobooks` is exactly
    // what a Windows user types at the setup prompt (verified live 2026-07-15:
    // `~/Audible` landed in `C:\Users\marcel\Audible`).
    #[cfg(windows)]
    #[test]
    fn tilde_expands_with_both_separators() {
        let home = PathBuf::from(r"C:\Users\x");
        assert_eq!(
            expand_tilde_from(Path::new("~/Audible"), &home),
            PathBuf::from(r"C:\Users\x\Audible")
        );
        assert_eq!(
            expand_tilde_from(Path::new(r"~\Audiobooks"), &home),
            PathBuf::from(r"C:\Users\x\Audiobooks")
        );
        assert_eq!(expand_tilde_from(Path::new("~"), &home), home);
        // An absolute Windows path and a `~name` component are never rewritten.
        assert_eq!(
            expand_tilde_from(Path::new(r"D:\media\x"), &home),
            PathBuf::from(r"D:\media\x")
        );
        assert_eq!(
            expand_tilde_from(Path::new("~alice"), &home),
            PathBuf::from("~alice")
        );
    }

    // Path composition must use native separators. The naming engine's `/`
    // folder markers would otherwise survive verbatim in the joined PathBuf —
    // displayed and stored as a mixed `C:\dir\sub/file` on Windows (verified
    // live 2026-07-15). Component-based `PathBuf` equality hides that, so these
    // assert the exact rendered string as well.

    #[cfg(not(windows))]
    #[test]
    fn join_relative_uses_native_separators() {
        // A nested (custom-mode) filename.
        let joined = join_relative(Path::new("/dl"), "series/title.AAX_44_128.aaxc");
        assert_eq!(joined, PathBuf::from("/dl/series/title.AAX_44_128.aaxc"));
        assert_eq!(joined.to_string_lossy(), "/dl/series/title.AAX_44_128.aaxc");
        // Deeper nesting.
        assert_eq!(
            join_relative(Path::new("/dl"), "a/b/c.aaxc"),
            PathBuf::from("/dl/a/b/c.aaxc")
        );
        // A flat (no-folder) filename is joined unchanged.
        assert_eq!(
            join_relative(Path::new("/dl"), "title.aaxc"),
            PathBuf::from("/dl/title.aaxc")
        );
    }

    #[cfg(windows)]
    #[test]
    fn join_relative_uses_native_separators() {
        // The `/` folder marker becomes a backslash — not a mixed path.
        let joined = join_relative(Path::new(r"C:\dl"), "series/title.AAX_44_128.aaxc");
        assert_eq!(joined, PathBuf::from(r"C:\dl\series\title.AAX_44_128.aaxc"));
        // The exact string form is what a mixed-separator bug would break.
        assert_eq!(
            joined.to_string_lossy(),
            r"C:\dl\series\title.AAX_44_128.aaxc"
        );
        assert!(
            !joined.to_string_lossy().contains('/'),
            "{}",
            joined.display()
        );
        // Deeper nesting stays native throughout.
        assert_eq!(
            join_relative(Path::new(r"C:\dl"), "a/b/c.aaxc"),
            PathBuf::from(r"C:\dl\a\b\c.aaxc")
        );
        // A flat (no-folder) filename is joined unchanged.
        assert_eq!(
            join_relative(Path::new(r"C:\dl"), "title.aaxc"),
            PathBuf::from(r"C:\dl\title.aaxc")
        );
    }

    /// A template context with every catalog variable present (empty by
    /// default), overridden by `pairs`. Building from the catalog also guards
    /// against the expander and the catalog drifting apart.
    fn ctx_of(pairs: &[(&'static str, &str)]) -> std::collections::HashMap<&'static str, String> {
        let mut map = std::collections::HashMap::new();
        for var in crate::config::filename_template::TEMPLATE_VARS {
            map.insert(var.name, String::new());
        }
        for (key, value) in pairs {
            map.insert(*key, (*value).to_owned());
        }
        map
    }

    #[test]
    fn template_expands_scalars_and_charset_modifiers() {
        let ctx = ctx_of(&[("title", "Leviathan fällt"), ("release_year", "2021")]);
        // Default (unicode) keeps the umlaut; `!a` transliterates.
        assert_eq!(
            expand_template("%title% (%release_year%)", &ctx, 230).unwrap(),
            "Leviathan fällt (2021)"
        );
        assert_eq!(
            expand_template("%title!a%", &ctx, 230).unwrap(),
            "Leviathan_fallt"
        );
    }

    #[test]
    fn template_builds_folders_with_placeholder_for_empty_variables() {
        let with_series = ctx_of(&[
            ("publication", "The Expanse"),
            ("fulltitle", "Leviathan Falls"),
        ]);
        assert_eq!(
            expand_template("%publication%/%fulltitle%", &with_series, 230).unwrap(),
            "The Expanse/Leviathan Falls"
        );
        // Missing publication → an `unknown` folder (regular depth, not flat).
        let standalone = ctx_of(&[("publication", ""), ("fulltitle", "Der Astronaut")]);
        assert_eq!(
            expand_template("%publication%/%fulltitle%", &standalone, 230).unwrap(),
            "unknown/Der Astronaut"
        );
        // An empty variable inside a segment also becomes `unknown`.
        let no_year = ctx_of(&[("title", "Der Astronaut"), ("release_year", "")]);
        assert_eq!(
            expand_template("%title% (%release_year%)", &no_year, 230).unwrap(),
            "Der Astronaut (unknown)"
        );
    }

    #[test]
    fn template_cannot_escape_download_dir() {
        // A leading slash is stripped; a `..` template segment is dropped; and a
        // value carrying slashes/`..` collapses to a single safe segment.
        let ctx = ctx_of(&[("title", "../../etc/passwd"), ("publication", "..")]);
        let path = expand_template("/%publication%/%title%", &ctx, 230).unwrap();
        assert!(!path.is_empty());
        assert!(!path.starts_with('/'), "{path}");
        assert!(!path.contains("../"), "{path}");
        assert!(
            !path.split('/').any(|seg| seg == "." || seg == ".."),
            "{path}"
        );
        // A variable value's own `/` never creates a folder.
        assert!(!expand_template("%title%", &ctx, 230).unwrap().contains('/'));
    }

    #[test]
    fn template_rejects_unknown_variables_and_modifiers() {
        let ctx = ctx_of(&[]);
        assert!(expand_template("%author%", &ctx, 230).is_err());
        assert!(expand_template("%title!x%", &ctx, 230).is_err());
        assert!(expand_template("%title", &ctx, 230).is_err());
        // `%%` is a literal percent.
        assert_eq!(expand_template("100%%", &ctx, 230).unwrap(), "100%");
    }

    #[test]
    fn artifact_suffix_per_kind_and_ext() {
        assert_eq!(
            artifact_suffix("audio", "AAX_44_128", "aaxc"),
            ".AAX_44_128.aaxc"
        );
        // A renamed (mp3) audio keeps its actual extension.
        assert_eq!(
            artifact_suffix("audio", "AAX_44_128", "mp3"),
            ".AAX_44_128.mp3"
        );
        assert_eq!(artifact_suffix("audio", "", "aaxc"), ".aaxc");
        assert_eq!(artifact_suffix("cover", "500", "jpg"), ".cover_500.jpg");
        assert_eq!(
            artifact_suffix("chapter", "tree", "json"),
            ".chapters_tree.json"
        );
        assert_eq!(artifact_suffix("pdf", "", "pdf"), ".pdf");
    }

    #[test]
    fn sidecar_path_by_extension() {
        assert_eq!(
            sidecar_path(Path::new("/dl/Book.AAX_44_128.aaxc")),
            Some(PathBuf::from("/dl/Book.AAX_44_128.voucher"))
        );
        assert_eq!(
            sidecar_path(Path::new("/dl/Book.AAC_44_131.cenc")),
            Some(PathBuf::from("/dl/Book.AAC_44_131.wvkey"))
        );
        // Mpeg fallback / podcast mp3, the decrypted m4b, and non-audio
        // artifacts carry no sidecar.
        assert_eq!(sidecar_path(Path::new("/dl/Book.MPEG.mp3")), None);
        assert_eq!(sidecar_path(Path::new("/dl/Book.AAX_44_128.m4b")), None);
        assert_eq!(sidecar_path(Path::new("/dl/Book.pdf")), None);
    }

    #[test]
    fn filename_is_truncated_to_the_byte_limit() {
        use crate::config::schema::FilenameMode;
        let long = "x".repeat(300);
        let name = sanitize_filename(&long, FilenameMode::AsinAscii, "B0ASIN", 50);
        assert!(name.len() <= 50, "{} bytes", name.len());
        assert!(name.starts_with("B0ASIN_"), "{name}");
        // 0 disables the limit.
        assert_eq!(
            sanitize_filename(&long, FilenameMode::Ascii, "B0ASIN", 0).len(),
            300
        );
    }

    #[test]
    fn ascii_filename_transliterates_and_underscores() {
        use crate::config::schema::FilenameMode;
        // Accents fold to base ASCII, `:` vanishes (no stray separator),
        // spaces become `_`.
        assert_eq!(
            sanitize_filename(
                "Steve Jobs: Die autorisierte Biografie des Apple-Gründers",
                FilenameMode::Ascii,
                "B0",
                230,
            ),
            "Steve_Jobs_Die_autorisierte_Biografie_des_Apple-Grunders"
        );
        assert_eq!(
            sanitize_filename("Die Wildgänsen 2", FilenameMode::Ascii, "B0", 230),
            "Die_Wildgansen_2"
        );
        // Leading/trailing separators trimmed; ASIN joined with a single `_`.
        assert_eq!(
            sanitize_filename("  Café  ", FilenameMode::AsinAscii, "B0ASIN", 230),
            "B0ASIN_Cafe"
        );
    }

    #[test]
    fn unicode_filename_keeps_letters_but_replaces_unsafe() {
        use crate::config::schema::FilenameMode;
        assert_eq!(
            sanitize_filename("Café Über/Unter", FilenameMode::Unicode, "B0", 230),
            "Café Über_Unter"
        );
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // Each 'é' is two bytes; a 5-byte cut must land on a boundary.
        let cut = truncate_filename("ééééé", 5);
        assert!(cut.len() <= 5);
        assert!(cut.chars().all(|c| c == 'é'), "{cut}");
    }
}
