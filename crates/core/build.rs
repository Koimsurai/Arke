//! Rebuilds the crate when a translation changes: `rust_i18n::i18n!` reads `locales/` at compile
//! time, and Cargo does not know to look there.

fn main() {
    println!("cargo:rerun-if-changed=locales");
}
