//! Compression methods and passwords (#341, #346): every method the zip and 7z tools people use
//! write, and archives that ask for a password.
//!
//! The tests never encrypt anything: a test process that writes encrypted files is what
//! anti-ransomware tools quarantine (Trend Micro did, as "Unauthorized file encryption"). The
//! protected archives, and the methods with no writer in Rust (Deflate64, LZMA in a zip), are made
//! by 7-Zip once and kept in `fixtures/`; each holds `a.txt`, [`body`], and the password is
//! "secret". So are the zstd zips, which the zip crate can't write without its C zstd: `zstd.zip`
//! by Python 3.14's zipfile, `zstd-frames.zip` made by hand with its data in two frames.

use super::*;
use zip::CompressionMethod;

const BODY: &[u8] = b"Arke reads this back unchanged. ";

fn body() -> Vec<u8> {
    BODY.repeat(500)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(r"src\archive\tests\fixtures")
        .join(name)
}

fn extract_with(
    path: &Path,
    out: &Path,
    kind: ArchiveKind,
    password: Option<&str>,
) -> Result<ExtractSummary, ArchiveError> {
    let cancel = AtomicBool::new(false);
    let options = ExtractOptions {
        password,
        ..opts(&cancel, &|_| {})
    };
    extract(path, out, kind, &options)
}

#[test]
fn every_zip_method_extracts() {
    let sb = Sandbox::new("zip-methods");
    for method in [
        CompressionMethod::Stored,
        CompressionMethod::Deflated,
        CompressionMethod::Bzip2,
        CompressionMethod::Xz,
        CompressionMethod::Ppmd,
    ] {
        let path = sb.0.join(format!("{method:?}.zip"));
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let options = SimpleFileOptions::default().compression_method(method);
        zip.start_file("a.txt", options).unwrap();
        zip.write_all(&body()).unwrap();
        zip.start_file("b.txt", options).unwrap();
        zip.finish().unwrap();
        let out = sb.0.join(format!("{method:?}"));
        let summary = extract_with(&path, &out, ArchiveKind::Zip, None).unwrap();
        assert_eq!(summary.files, 2, "{method:?}");
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), body(), "{method:?}");
    }
    for name in ["deflate64.zip", "lzma.zip", "zstd.zip", "zstd-frames.zip"] {
        let out = sb.0.join(name);
        extract_with(&fixture(name), &out, ArchiveKind::Zip, None)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), body(), "{name}");
    }
}

#[test]
fn every_7z_method_extracts() {
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter, EncoderMethod};
    let sb = Sandbox::new("7z-methods");
    for method in [
        EncoderMethod::LZMA2,
        EncoderMethod::BZIP2,
        EncoderMethod::DEFLATE,
    ] {
        let name = method.name().to_owned();
        let path = sb.0.join(format!("{name}.7z"));
        let mut writer = ArchiveWriter::create(&path).unwrap();
        writer.set_content_methods(vec![method.into()]);
        let body = body();
        writer
            .push_archive_entry(ArchiveEntry::new_file("a.txt"), Some(&body[..]))
            .unwrap();
        writer.finish().unwrap();
        let out = sb.0.join(&name);
        let summary = extract_with(&path, &out, ArchiveKind::SevenZip, None).unwrap();
        assert_eq!(summary.files, 1, "{name}");
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), body, "{name}");
    }
}

/// Asks for the password, refuses a wrong one before writing anything, then extracts: ZipCrypto
/// and AES-256 zips, and a 7z with its names encrypted too.
#[test]
fn protected_archives_take_their_password() {
    let sb = Sandbox::new("passwords");
    for name in ["zipcrypto.zip", "aes.zip", "aes.7z"] {
        let path = fixture(name);
        let kind = archive_kind(name).unwrap();
        let out = sb.0.join(name);
        let err = extract_with(&path, &out, kind, None).unwrap_err();
        assert!(matches!(err, ArchiveError::Encrypted), "{name}: {err:?}");
        let err = extract_with(&path, &out, kind, Some("wrong")).unwrap_err();
        assert!(
            matches!(err, ArchiveError::WrongPassword),
            "{name}: {err:?}"
        );
        assert!(!out.join("a.txt").exists(), "{name}: nothing was written");
        extract_with(&path, &out, kind, Some("secret")).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), body(), "{name}");
    }
    // The 7z's names are encrypted: the preview can't list it.
    let listing = list_path(ArchiveKind::SevenZip, &fixture("aes.7z"), 10, &fake_big5);
    assert!(
        matches!(listing, Err(ArchiveError::Encrypted)),
        "{listing:?}"
    );
}

/// A method Arke doesn't decode is named, not called damage (#341): the app then offers another
/// program.
#[test]
fn a_zip_method_arke_cant_read_is_named() {
    let sb = Sandbox::new("zip-wavpack");
    let path = sb.0.join("audio.zip");
    let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("a.txt", stored).unwrap();
    zip.write_all(&body()).unwrap();
    zip.finish().unwrap();
    // Say WavPack (97) in the local and central headers, as a tool that writes it does.
    let mut bytes = fs::read(&path).unwrap();
    for (signature, method_at) in [(0x0403_4b50_u32, 8), (0x0201_4b50, 10)] {
        let at = bytes
            .windows(4)
            .position(|w| w == signature.to_le_bytes())
            .unwrap();
        bytes[at + method_at..at + method_at + 2].copy_from_slice(&97u16.to_le_bytes());
    }
    fs::write(&path, bytes).unwrap();
    let err = extract_with(&path, &sb.0.join("out"), ArchiveKind::Zip, None).unwrap_err();
    assert!(
        matches!(&err, ArchiveError::UnsupportedMethod { method } if method == "WavPack"),
        "{err:?}"
    );
}

/// A zstd entry whose data doesn't match its CRC is damage, not a file written wrong.
#[test]
fn a_zstd_entry_with_a_bad_crc_fails() {
    let sb = Sandbox::new("zstd-crc");
    let path = sb.0.join("bad.zip");
    let mut bytes = fs::read(fixture("zstd-frames.zip")).unwrap();
    // The CRC-32 in the local and central headers.
    for (signature, crc_at) in [(0x0403_4b50_u32, 14), (0x0201_4b50, 16)] {
        let at = bytes
            .windows(4)
            .position(|w| w == signature.to_le_bytes())
            .unwrap();
        bytes[at + crc_at] ^= 0xff;
    }
    fs::write(&path, bytes).unwrap();
    let out = sb.0.join("out");
    let err = extract_with(&path, &out, ArchiveKind::Zip, None).unwrap_err();
    assert!(matches!(err, ArchiveError::Read { .. }), "{err:?}");
    assert!(!out.join("a.txt").exists(), "the bad file isn't left");
}
