//! The search service reads the NTFS master file table and the USN journal: Windows only. On other
//! targets, say so instead of failing on the Windows API (#57).

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        println!(
            "cargo::error=arke-indexd is Windows-only (it reads the NTFS master file table). \
             Arke builds for Windows only for now: https://github.com/Koimsurai/Arke#faq"
        );
    }
}
