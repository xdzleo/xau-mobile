//! Windows DPAPI wrapper for the session file at rest.
//!
//! `auth.json` holds a live Microsoft refresh token plus two XSTS
//! bearers, under `%LOCALAPPDATA%/XAU/`.
//!
//! An earlier draft of this comment justified the encryption with
//! OneDrive: the file used to live in `Documents/XAU/`, which most
//! machines sync. `settings::credential_path` already migrated it out
//! of there, so that argument no longer applies and is not the reason
//! this exists. The reason is narrower and still worth it: a live
//! refresh token should not sit greppable on disk for anything that can
//! read the user's files — backup tooling, a shared machine, an
//! over-broad "scan my documents" utility.
//!
//! [`CryptProtectData`] ties the ciphertext to the current **user**
//! account on the current machine: copying the file to another PC, or
//! reading it as another user, fails to decrypt. That is the whole
//! threat model — it is not protection against code already running
//! as this user, which could just call `CryptUnprotectData` itself.
//!
//! Mirrors the scheme upstream adopted in Fumo-Unlockers/XAU#480
//! (magic header + DPAPI blob + plaintext fallback on read), with our
//! own magic because the payload struct is not theirs.

/// Additional entropy mixed into the DPAPI key. Not a secret — it only
/// ensures a blob produced by this app cannot be decrypted by another
/// app that also calls `CryptUnprotectData` with default entropy.
#[cfg(windows)]
const ENTROPY: &[u8] = b"xau-rs-auth-v1";

/// File magic marking a DPAPI-encrypted session file. A file that does
/// NOT start with this is treated as legacy plaintext JSON and read as
/// such — that is the upgrade path, and it must never be removed
/// without a migration, or every existing install silently logs out.
pub const MAGIC: &[u8] = b"XAURS1";

/// Magic the **upstream C# XAU** writes. Both apps keep their session at
/// `%LOCALAPPDATA%/XAU/auth.json`, so on a machine with both installed
/// each will find the other's file there.
///
/// Its payload is a different struct under a different DPAPI entropy, so
/// it is not readable here — but recognising it turns "your session file
/// is corrupt" into an accurate message, and lets callers refuse to
/// clobber a working sign-in belonging to the other app.
pub const UPSTREAM_MAGIC: &[u8] = b"XAU1";

/// Encrypt `plain` for the current user.
///
/// # Errors
/// Returns [`crate::error::Error::Unsupported`] off Windows, and an OS
/// error when `CryptProtectData` fails.
#[cfg(windows)]
pub fn protect(plain: &[u8]) -> crate::error::Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    // SAFETY: both input blobs point at live slices for the duration of
    // the call; `out` is zeroed and only read when the call returns
    // non-zero, and its buffer is released with `LocalFree` exactly once.
    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(plain.len())
                .map_err(|_| crate::error::Error::other("session too large to encrypt"))?,
            pbData: plain.as_ptr().cast_mut(),
        };
        let mut entropy = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(ENTROPY.len()).unwrap_or(0),
            pbData: ENTROPY.as_ptr().cast_mut(),
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = CryptProtectData(
            &raw const input,
            std::ptr::null(),
            &raw const entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut out,
        );
        // Touch the locals so the optimiser cannot claim they are dead
        // before the FFI call reads through the raw pointers.
        let _ = (&mut input, &mut entropy);
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData.cast());
        Ok(bytes)
    }
}

/// Decrypt a blob produced by [`protect`].
///
/// # Errors
/// Returns [`crate::error::Error::Unsupported`] off Windows, and an OS
/// error when the blob was written by another user/machine or is corrupt.
#[cfg(windows)]
pub fn unprotect(blob: &[u8]) -> crate::error::Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    // SAFETY: same contract as `protect` above.
    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(blob.len())
                .map_err(|_| crate::error::Error::other("session blob too large"))?,
            pbData: blob.as_ptr().cast_mut(),
        };
        let mut entropy = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(ENTROPY.len()).unwrap_or(0),
            pbData: ENTROPY.as_ptr().cast_mut(),
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = CryptUnprotectData(
            &raw const input,
            std::ptr::null_mut(),
            &raw const entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut out,
        );
        let _ = (&mut input, &mut entropy);
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData.cast());
        Ok(bytes)
    }
}

#[cfg(not(windows))]
pub fn protect(_plain: &[u8]) -> crate::error::Result<Vec<u8>> {
    Err(crate::error::Error::Unsupported(
        "DPAPI is Windows-only".into(),
    ))
}

#[cfg(not(windows))]
pub fn unprotect(_blob: &[u8]) -> crate::error::Result<Vec<u8>> {
    Err(crate::error::Error::Unsupported(
        "DPAPI is Windows-only".into(),
    ))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn round_trip_returns_the_original_bytes() {
        let secret = br#"{"refresh_token":"M.C123_BAY.0.U.abcdef","expires_in":86400}"#;
        let blob = protect(secret).expect("protect must succeed for the current user");
        assert_ne!(
            blob.as_slice(),
            secret.as_slice(),
            "ciphertext must not equal plaintext"
        );
        // The whole point: the token must not be greppable on disk.
        assert!(
            !blob.windows(9).any(|w| w == b"M.C123_BA"),
            "refresh token leaked verbatim into the DPAPI blob"
        );
        let back = unprotect(&blob).expect("unprotect must round-trip");
        assert_eq!(back.as_slice(), secret.as_slice());
    }

    #[test]
    fn a_corrupt_blob_fails_instead_of_returning_garbage() {
        let mut blob = protect(b"hello").expect("protect");
        // Flip a byte in the middle of the ciphertext.
        let mid = blob.len() / 2;
        blob[mid] ^= 0xFF;
        assert!(
            unprotect(&blob).is_err(),
            "a tampered blob must fail, not decrypt to garbage"
        );
    }

    #[test]
    fn wrong_entropy_cannot_decrypt() {
        // Proves the entropy is actually bound into the key: a blob made
        // with default (empty) entropy must not open with ours.
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{
            CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        };
        let plain = b"entropy-check";
        let foreign = unsafe {
            let input = CRYPT_INTEGER_BLOB {
                cbData: plain.len() as u32,
                pbData: plain.as_ptr().cast_mut(),
            };
            let mut out = CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: std::ptr::null_mut(),
            };
            let ok = CryptProtectData(
                &raw const input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &raw mut out,
            );
            assert_ne!(ok, 0, "control CryptProtectData failed");
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData.cast());
            v
        };
        assert!(
            unprotect(&foreign).is_err(),
            "a blob sealed without our entropy must not open with it"
        );
    }
}
