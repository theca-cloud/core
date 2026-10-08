//! The browser's password strength: `common`'s scorer, the server's own code
//! and dictionaries, in its own small WASM module so the engine doesn't carry
//! zxcvbn (loaded only by pages choosing a password).

use wasm_bindgen::prelude::*;

/// A password's strength, as the server judges it: JSON
/// `{ score, warning, suggestions }`, the account's email counting against it.
#[wasm_bindgen]
pub fn strength(password: &str, email: &str) -> String {
    serde_json::to_string(&theca_common::password::strength(password, email))
        .expect("plain data serializes")
}
