//! Update signatures (#249): every file Arke downloads to update itself, and the notes it shows
//! about an update, comes with a minisign signature (`<file>.minisig`) made by the release
//! workflow (`cargo xtask publish`). Arke installs or shows it only when the signature is from one
//! of the keys built in here.
//!
//! A `.sha256` next to the file only proves the download wasn't cut short: whoever can change the
//! file can change it too. A signature holds wherever the file came from, which is what lets a
//! user send downloads through a route of their own (a mirror, a proxy) without trusting it.
//!
//! The signature's trusted comment, which the key signs too, names the file and the version
//! (`file:arke-0.9.0.exe version:0.9.0`). Both are checked against what Arke asked for, so a
//! signed older release (or another signed file) can't be passed off as the new version.

use minisign_verify::{PublicKey, Signature};

/// Arke's update keys (minisign public keys). While a key is being replaced, the old and the new
/// one are both here for a few releases.
const KEYS: [&str; 1] = ["RWTaWSr3SFSSunIocrKdQcaM3w3q2qEHs+gw6rfwWnfmm41uuixOzGNm"];

/// Why a download wasn't accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SigError {
    /// The signature isn't one of Arke's keys over these bytes (or isn't a signature at all).
    #[error("bad signature")]
    Invalid,
    /// A valid signature, but for another file or version.
    #[error("signed for another file or version")]
    Mismatch,
}

/// Whether `signature` (a `.minisig` file's text) is Arke's signature over `data` for the release
/// asset `asset` of `version`.
pub(crate) fn verify(
    data: &[u8],
    signature: &str,
    asset: &str,
    version: &str,
) -> Result<(), SigError> {
    verify_with(&KEYS, data, signature, asset, version)
}

fn verify_with(
    keys: &[&str],
    data: &[u8],
    signature: &str,
    asset: &str,
    version: &str,
) -> Result<(), SigError> {
    let Ok(signature) = Signature::decode(signature) else {
        return Err(SigError::Invalid);
    };
    let signed = keys
        .iter()
        .filter_map(|key| PublicKey::from_base64(key).ok())
        .any(|key| key.verify(data, &signature, false).is_ok());
    if !signed {
        return Err(SigError::Invalid);
    }
    let field = |name: &str| {
        signature
            .trusted_comment()
            .split_whitespace()
            .find_map(|part| part.strip_prefix(name))
    };
    let same_file = field("file:").is_some_and(|f| f.eq_ignore_ascii_case(asset));
    let same_version = field("version:") == Some(version);
    if same_file && same_version {
        Ok(())
    } else {
        Err(SigError::Mismatch)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A throwaway key pair; tests sign with it as the release workflow does with the real one.
    pub(crate) struct TestKey {
        pk: minisign::PublicKey,
        sk: minisign::SecretKey,
    }

    impl TestKey {
        pub(crate) fn new() -> Self {
            let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
            Self {
                pk: pair.pk,
                sk: pair.sk,
            }
        }

        pub(crate) fn public(&self) -> String {
            self.pk.to_base64()
        }

        /// The `.minisig` text for `data` published as `asset` of `version`.
        pub(crate) fn sign(&self, data: &[u8], asset: &str, version: &str) -> String {
            minisign::sign(
                None,
                &self.sk,
                data,
                Some(&format!("file:{asset} version:{version}")),
                None,
            )
            .unwrap()
            .to_string()
        }
    }

    #[test]
    fn a_signed_download_passes() {
        let key = TestKey::new();
        let sig = key.sign(b"new arke", "arke-0.9.0.exe", "0.9.0");
        let public = key.public();
        assert_eq!(
            verify_with(&[&public], b"new arke", &sig, "arke-0.9.0.exe", "0.9.0"),
            Ok(())
        );
    }

    /// A changed byte, another key, or something that isn't a signature: refused.
    #[test]
    fn anything_else_is_refused() {
        let key = TestKey::new();
        let other = TestKey::new();
        let sig = key.sign(b"new arke", "arke-0.9.0.exe", "0.9.0");
        let public = key.public();
        assert_eq!(
            verify_with(&[&public], b"new arkE", &sig, "arke-0.9.0.exe", "0.9.0"),
            Err(SigError::Invalid)
        );
        let forged = other.sign(b"new arke", "arke-0.9.0.exe", "0.9.0");
        assert_eq!(
            verify_with(&[&public], b"new arke", &forged, "arke-0.9.0.exe", "0.9.0"),
            Err(SigError::Invalid)
        );
        assert_eq!(
            verify_with(
                &[&public],
                b"new arke",
                "<html>404</html>",
                "arke-0.9.0.exe",
                "0.9.0"
            ),
            Err(SigError::Invalid)
        );
    }

    /// A genuine older release (or another genuine file) handed out as the new version.
    #[test]
    fn a_signature_for_another_version_or_file_is_refused() {
        let key = TestKey::new();
        let public = key.public();
        let old = key.sign(b"old arke", "arke-0.8.1.exe", "0.8.1");
        assert_eq!(
            verify_with(&[&public], b"old arke", &old, "arke-0.9.0.exe", "0.9.0"),
            Err(SigError::Mismatch)
        );
        let setup = key.sign(b"setup", "arke-0.9.0-setup.exe", "0.9.0");
        assert_eq!(
            verify_with(&[&public], b"setup", &setup, "arke-0.9.0.exe", "0.9.0"),
            Err(SigError::Mismatch)
        );
        // "0.9.0" must not match a comment that says 0.9.0-rc.1.
        let rc = key.sign(b"rc", "arke-0.9.0.exe", "0.9.0-rc.1");
        assert_eq!(
            verify_with(&[&public], b"rc", &rc, "arke-0.9.0.exe", "0.9.0"),
            Err(SigError::Mismatch)
        );
    }

    #[test]
    fn the_built_in_key_is_a_key() {
        assert!(KEYS.iter().all(|k| PublicKey::from_base64(k).is_ok()));
    }
}
