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

/// One embedded file.
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    /// Bytes to serve.
    pub body: &'static [u8],
    /// `Content-Type` value.
    pub content_type: &'static str,
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

/// Look up `/assets/<path>`.
pub fn asset(path: &str) -> Option<Asset> {
    let (body, extension) = match path {
        "app.css" => (APP_CSS.as_bytes(), "css"),
        "login.js" => (LOGIN_JS.as_bytes(), "js"),
        "console.js" => (CONSOLE_JS.as_bytes(), "js"),
        "uflinux_rds_web.js" => (WASM_JS, "js"),
        "uflinux_rds_web_bg.wasm" => (WASM_BINARY, "wasm"),
        _ => return None,
    };
    let content_type = match extension {
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "text/javascript; charset=utf-8",
    };
    Some(Asset { body, content_type })
}

/// `Content-Type` for the HTML pages.
pub const HTML: &str = "text/html; charset=utf-8";

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
