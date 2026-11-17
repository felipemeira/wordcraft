//! Fonts served next to the web build.
//!
//! The browser gives the app none of the system's fonts, so text in a font the build doesn't embed
//! falls back to what it has; an emoji in a document made on Windows ("Segoe UI Emoji") has no
//! font at all and draws as an empty box. A site that hosts WordCraft can put font files in
//! `fonts/` beside `index.html` (for emoji, Noto Color Emoji, which the fallback for emoji already
//! looks for). They are added before the app starts, like a font the user added, so the first
//! document draws with them; a missing file is logged and skipped.

use wasm_bindgen::JsCast as _;

/// Where the fonts are, relative to the page.
const DIR: &str = "./fonts";

/// Font files a site may serve.
const SERVED: &[&str] = &["Noto-COLRv1-noflags.ttf"];

/// Fetch and add every served font the site has.
pub async fn load_all() {
    for name in SERVED {
        let bytes = match bytes(&format!("{DIR}/{name}")).await {
            Ok(b) => b,
            Err(e) => {
                log::info!("font {name}: not served ({e})");
                continue;
            }
        };
        if bytes.len() > wordcraft_fonts::MAX_FONT_FILE_BYTES {
            log::warn!("font {name}: too large; skipped");
            continue;
        }
        match wordcraft_fonts::FontDb::global().install_font(name, &bytes) {
            Ok(_) => log::info!("font {name}: added ({} KB)", bytes.len() / 1024),
            Err(e) => log::warn!("font {name}: skipped: {e}"),
        }
    }
}

/// The body of `url` as bytes, or why not.
async fn bytes(url: &str) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await.map_err(|e| format!("{e:?}"))?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| "not a response".to_string())?;
    if !response.ok() {
        return Err(format!("HTTP {}", response.status()));
    }
    let body = response.array_buffer().map_err(|e| format!("{e:?}"))?;
    let body = wasm_bindgen_futures::JsFuture::from(body).await.map_err(|e| format!("{e:?}"))?;
    Ok(js_sys::Uint8Array::new(&body).to_vec())
}
