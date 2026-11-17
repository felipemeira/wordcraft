//! Spelling dictionaries served next to the web build.
//!
//! The web version can't read the dictionaries folders or install Language packs, so on its own
//! it proofs English only. A site that hosts WordCraft can put Hunspell dictionaries, decoded to
//! UTF-8, in `dictionaries/` beside `index.html` (for Brazilian Portuguese, `pt_BR.aff` and
//! `pt_BR.dic`, VERO from LibreOffice's dictionaries). They are fetched before the app starts, so
//! the first document is already checked with them; a missing or unreadable pair is logged and
//! that language stays unchecked, as before.

use wasm_bindgen::JsCast as _;

/// Where the dictionaries are, relative to the page.
const DIR: &str = "./dictionaries";

/// A language whose dictionary a site may serve: the file name without extension and the
/// proofing call that takes the `.aff` and `.dic` text.
struct Served {
    file: &'static str,
    install: fn(&str, &str) -> Result<(), String>,
}

const SERVED: &[Served] = &[Served { file: "pt_BR", install: wordcraft_proof::pt::install_dictionary }];

/// Fetch and install every served dictionary the site has.
pub async fn load_all() {
    for d in SERVED {
        let (aff, dic) = match (text(&format!("{DIR}/{}.aff", d.file)).await, text(&format!("{DIR}/{}.dic", d.file)).await) {
            (Ok(aff), Ok(dic)) => (aff, dic),
            (Err(e), _) | (_, Err(e)) => {
                log::info!("dictionary {}: not served ({e})", d.file);
                continue;
            }
        };
        match (d.install)(&aff, &dic) {
            Ok(()) => log::info!("dictionary {}: installed ({} KB)", d.file, (aff.len() + dic.len()) / 1024),
            Err(e) => log::warn!("dictionary {}: unreadable: {e}", d.file),
        }
    }
}

/// The body of `url` as text, or why not.
async fn text(url: &str) -> Result<String, String> {
    let window = web_sys::window().ok_or("no window")?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await.map_err(|e| format!("{e:?}"))?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| "not a response".to_string())?;
    if !response.ok() {
        return Err(format!("HTTP {}", response.status()));
    }
    let body = response.text().map_err(|e| format!("{e:?}"))?;
    let body = wasm_bindgen_futures::JsFuture::from(body).await.map_err(|e| format!("{e:?}"))?;
    body.as_string().ok_or_else(|| "body is not text".to_string())
}
