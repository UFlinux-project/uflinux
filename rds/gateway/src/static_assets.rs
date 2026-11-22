//! Embedded static assets.
//!
//! `include_dir` is not in the workspace, so the small fixed set of files is
//! embedded with `include_str!`/`include_bytes!`. The content security policy
//! forbids inline scripts and inline styles, so every page links one external
//! script and the shared stylesheet.
//!
//! The two `uflinux_rds_web*` files are build products of the wasm client
//! crate: `rds/scripts/build-wasm.sh` writes them here. Committed placeholders
//! keep the gateway compilable without a wasm toolchain, but a real image must
//! ship both, or the console page cannot start a session.
//!
//! Each served file may also have a brotli-compressed twin (`<name>.br`) from
//! the same script. Those twins are gitignored build products, so `build.rs`
//! copies them into `OUT_DIR` — empty when absent — and they are embedded from
//! there: the crate always compiles, and an empty body means "no brotli
//! variant, serve identity".

/// Number of files in [`asset`]'s whitelist.
const ASSET_COUNT: usize = 5;

/// Which representation of an asset a client is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Uncompressed bytes.
    Identity,
    /// `Content-Encoding: br`, when a `.br` was built.
    Brotli,
}

/// One embedded file, with an optional brotli variant.
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    /// Identity bytes.
    pub body: &'static [u8],
    /// Brotli bytes, or empty when none was built.
    pub brotli: &'static [u8],
    /// `Content-Type` value.
    pub content_type: &'static str,
    /// Position in the ETag cache, i.e. which whitelisted file this is.
    index: usize,
}

impl Asset {
    /// Whether a brotli variant was built for this asset.
    pub fn has_brotli(&self) -> bool {
        !self.brotli.is_empty()
    }

    /// Bytes to serve for `encoding`, falling back to identity.
    pub fn body_for(&self, encoding: Encoding) -> &'static [u8] {
        match encoding {
            Encoding::Brotli if self.has_brotli() => self.brotli,
            _ => self.body,
        }
    }

    /// Strong `ETag` for one representation.
    ///
    /// Hashed from the bytes actually served, so the brotli and identity
    /// representations of a file differ: a strong validator must not be shared
    /// between encodings. Memoised because the wasm module is megabytes.
    pub fn etag(&self, encoding: Encoding) -> &'static str {
        let body = self.body_for(encoding);
        let variant = usize::from(encoding == Encoding::Brotli && self.has_brotli());
        etag_at(self.index * 2 + variant, body)
    }
}

/// FNV-1a over the payload and its length. A fingerprint for an entity tag
/// over a handful of build products, not anything security-critical.
fn finger(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash ^ (bytes.len() as u64)
}

/// Two slots per whitelisted file (`0 = identity`, `1 = brotli`) plus two
/// spare slots for an [`Asset`] built outside the whitelist, i.e. in tests.
static ETAGS: [std::sync::OnceLock<String>; ASSET_COUNT * 2 + 2] =
    [const { std::sync::OnceLock::new() }; ASSET_COUNT * 2 + 2];

fn etag_at(slot: usize, body: &[u8]) -> &'static str {
    ETAGS[slot]
        .get_or_init(|| format!("\"{:016x}\"", finger(body)))
        .as_str()
}

/// Login page (`GET /`).
pub const LOGIN_HTML: &str = include_str!("../templates/login.html");
/// Console page (`GET /session`).
pub const SESSION_HTML: &str = include_str!("../templates/session.html");
/// Stylesheet shared by both pages.
pub const APP_CSS: &str = include_str!("../assets/app.css");
/// Login form handler.
pub const LOGIN_JS: &str = include_str!("../assets/login.js");
/// Console script: hands the one-time token to the wasm RDP client.
pub const CONSOLE_JS: &str = include_str!("../assets/console.js");
/// wasm-bindgen glue for the RDP client (build product).
pub const WASM_JS: &[u8] = include_bytes!("../assets/uflinux_rds_web.js");
/// The RDP client itself (build product).
pub const WASM_BINARY: &[u8] = include_bytes!("../assets/uflinux_rds_web_bg.wasm");

/// Brotli variants, staged in `OUT_DIR` by `build.rs`. Empty when the asset
/// was built without the `brotli` CLI.
const APP_CSS_BR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/app.css.br"));
const LOGIN_JS_BR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/login.js.br"));
const CONSOLE_JS_BR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/console.js.br"));
const WASM_JS_BR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/uflinux_rds_web.js.br"));
const WASM_BINARY_BR: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/uflinux_rds_web_bg.wasm.br"));

/// `Content-Type` for the HTML pages.
pub const HTML: &str = "text/html; charset=utf-8";

/// An asset that exists only in tests: it uses the two spare ETag slots, so it
/// cannot collide with a whitelisted file.
#[cfg(test)]
pub(crate) fn synthetic(body: &'static [u8], brotli: &'static [u8]) -> Asset {
    Asset {
        body,
        brotli,
        content_type: "text/plain; charset=utf-8",
        index: ASSET_COUNT,
    }
}

/// Look up `/assets/<path>`.
pub fn asset(path: &str) -> Option<Asset> {
    let (body, brotli, index, extension) = match path {
        "app.css" => (APP_CSS.as_bytes(), APP_CSS_BR, 0, "css"),
        "login.js" => (LOGIN_JS.as_bytes(), LOGIN_JS_BR, 1, "js"),
        "console.js" => (CONSOLE_JS.as_bytes(), CONSOLE_JS_BR, 2, "js"),
        "uflinux_rds_web.js" => (WASM_JS, WASM_JS_BR, 3, "js"),
        "uflinux_rds_web_bg.wasm" => (WASM_BINARY, WASM_BINARY_BR, 4, "wasm"),
        _ => return None,
    };
    let content_type = match extension {
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "text/javascript; charset=utf-8",
    };
    Some(Asset {
        body,
        brotli,
        content_type,
        index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_assets_are_served_with_a_type() {
        let css = asset("app.css").expect("app.css embedded");
        let sheet = std::str::from_utf8(css.body).expect("app.css is UTF-8");
        assert!(sheet.contains("--bg"));
        assert_eq!(css.content_type, "text/css; charset=utf-8");
        for name in ["login.js", "console.js", "uflinux_rds_web.js"] {
            let js = asset(name).expect(name);
            assert_eq!(js.content_type, "text/javascript; charset=utf-8");
        }
    }

    #[test]
    fn wasm_binary_is_embedded_as_a_module() {
        let wasm = asset("uflinux_rds_web_bg.wasm").expect("wasm embedded");
        assert_eq!(wasm.content_type, "application/wasm");
        assert_eq!(&wasm.body[..4], b"\0asm", "must be a wasm module");
    }

    #[test]
    fn unknown_and_traversing_paths_are_404() {
        assert!(asset("../etc/passwd").is_none());
        assert!(asset("").is_none());
        assert!(asset("app.css/../../x").is_none());
    }

    #[test]
    fn every_asset_has_its_own_strong_etag() {
        let mut tags = Vec::new();
        for name in ["app.css", "login.js", "console.js", "uflinux_rds_web.js"] {
            let asset = asset(name).expect(name);
            let tag = asset.etag(Encoding::Identity);
            // Strong validator: quoted, no `W/` prefix.
            assert!(tag.starts_with('"') && tag.ends_with('"') && !tag.starts_with("W/"));
            assert_eq!(tag, asset.etag(Encoding::Identity), "etag is stable");
            // Without a `.br` there is no second representation to differ from.
            assert_eq!(asset.has_brotli(), !asset.brotli.is_empty());
            tags.push(tag.to_owned());
        }
        tags.sort();
        tags.dedup();
        assert_eq!(tags.len(), 4, "each file has its own tag");
    }

    #[test]
    fn pages_are_embedded_and_self_contained() {
        assert!(LOGIN_HTML.contains("<form"));
        assert!(LOGIN_HTML.contains("/assets/login.js"));
        assert!(SESSION_HTML.contains("/assets/console.js"));
        // The CSP we ship forbids inline scripts, inline styles and inline
        // event handlers, so the pages must not use any.
        for page in [LOGIN_HTML, SESSION_HTML] {
            assert!(
                !page.contains("<script>")
                    && !page.contains("onclick=")
                    && !page.contains("style="),
                "no inline script, handler or style: the CSP forbids them"
            );
        }
    }
}
