//! Secrets (SSH passwords, key passphrases) at rest, encrypted with Windows DPAPI:
//! only the same Windows user on the same PC can decrypt them.

use anyhow::{Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
};

const ENTROPY: &[u8] = b"Pingless ssh secret v1";

pub fn protect(plain: &str) -> Result<String> {
    if plain.is_empty() {
        return Ok(String::new());
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: input buffers outlive the call; output is freed with LocalFree.
    unsafe {
        if CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut out,
        ) == 0
        {
            bail!("DPAPI encryption failed");
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData.cast());
        Ok(STANDARD.encode(bytes))
    }
}

pub fn unprotect(stored: &str) -> Result<String> {
    if stored.is_empty() {
        return Ok(String::new());
    }
    let data = STANDARD.decode(stored)?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: as above.
    unsafe {
        if CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut out,
        ) == 0
        {
            bail!("cannot decrypt the saved secret (another Windows user or PC?)");
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData.cast());
        Ok(String::from_utf8(bytes)?)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let enc = super::protect("hunter2 пароль").unwrap();
        assert_ne!(enc, "hunter2 пароль");
        assert_eq!(super::unprotect(&enc).unwrap(), "hunter2 пароль");
        assert_eq!(super::unprotect("").unwrap(), "");
    }
}
