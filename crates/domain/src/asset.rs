//! Preset images (docs/image-and-zip-package.md §1): a library file is named after its own
//! bytes, `<sha256 lowercase hex>.<ext>`, and the extension comes from the file signature, never
//! from the original file name. The host stores the files; everything that decides whether bytes
//! and a name belong together is here, so importing, downloading and running agree.
use sha2::{Digest, Sha256};
use std::fmt;

/// Largest image the library takes.
pub const MAX_ASSET_BYTES: u64 = 10 * 1024 * 1024;
/// Longest `name` of an image action, in characters.
pub const MAX_ASSET_DISPLAY_NAME_CHARS: usize = 200;
/// Diagnostic code of an image action whose `asset` is not an asset name.
pub const IMAGE_ASSET: &str = "image_asset";
/// Diagnostic code of an image action whose `name` is too long.
pub const IMAGE_NAME: &str = "image_name";

const EXTENSIONS: [&str; 5] = ["png", "jpg", "gif", "bmp", "webp"];

/// `^[a-f0-9]{64}\.(png|jpg|gif|bmp|webp)$`. An accepted name is a plain file name: it has no
/// separator and no `..`, so it can be joined to a directory and put in a URL path as is.
pub fn is_asset_name(name: &str) -> bool {
    let Some((hash, extension)) = name.split_once('.') else {
        return false;
    };
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        && EXTENSIONS.contains(&extension)
}

/// The extension of an accepted image format, decided by the file signature.
pub fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.starts_with(b"BM") {
        Some("bmp")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("webp")
    } else {
        None
    }
}

/// Why bytes cannot be (or do not match) a library file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetError {
    /// Over [`MAX_ASSET_BYTES`].
    TooLarge,
    /// The signature is none of PNG, JPEG, GIF, BMP or WebP.
    NotAnImage,
    /// The name does not match [`is_asset_name`].
    InvalidName,
    /// The name's hash is not the SHA-256 of the bytes.
    HashMismatch,
    /// The name's extension is not the one the signature decides.
    ExtensionMismatch,
}

impl AssetError {
    pub fn code(self) -> &'static str {
        match self {
            Self::TooLarge => "image_too_large",
            Self::NotAnImage => "image_format",
            Self::InvalidName => IMAGE_ASSET,
            Self::HashMismatch => "image_hash",
            Self::ExtensionMismatch => "image_extension",
        }
    }
    /// For the user (Chinese); names no file.
    pub fn message(self) -> &'static str {
        match self {
            Self::TooLarge => "图片超过 10 MiB",
            Self::NotAnImage => "只支持 PNG、JPEG、GIF、BMP、WebP 图片",
            Self::InvalidName => "图片资源名无效",
            Self::HashMismatch => "图片内容与文件名中的哈希不一致",
            Self::ExtensionMismatch => "图片扩展名与文件内容不一致",
        }
    }
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for AssetError {}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0xf), 16).unwrap_or('0'));
    }
    hex
}

/// The library name of an image, after checking its size and signature.
pub fn asset_name_for(bytes: &[u8]) -> Result<String, AssetError> {
    if bytes.len() as u64 > MAX_ASSET_BYTES {
        return Err(AssetError::TooLarge);
    }
    let extension = image_extension(bytes).ok_or(AssetError::NotAnImage)?;
    Ok(format!("{}.{extension}", sha256_hex(bytes)))
}

/// Whether `bytes` are the file `name` promises: within the size limit, the signature gives the
/// name's extension and the SHA-256 gives its hash.
pub fn verify_asset(name: &str, bytes: &[u8]) -> Result<(), AssetError> {
    if !is_asset_name(name) {
        return Err(AssetError::InvalidName);
    }
    if bytes.len() as u64 > MAX_ASSET_BYTES {
        return Err(AssetError::TooLarge);
    }
    let extension = image_extension(bytes).ok_or(AssetError::NotAnImage)?;
    if name.rsplit('.').next() != Some(extension) {
        return Err(AssetError::ExtensionMismatch);
    }
    if !name.starts_with(&sha256_hex(bytes)) {
        return Err(AssetError::HashMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    /// `sha256("abc")`, the FIPS 180 example: the hex is what `sha256sum` prints.
    #[test]
    fn the_hash_is_lowercase_sha256_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn asset_names_follow_the_contract_pattern() {
        let hash = "a".repeat(64);
        for extension in EXTENSIONS {
            assert!(is_asset_name(&format!("{hash}.{extension}")), "{extension}");
        }
        for bad in [
            String::new(),
            hash.clone(),
            format!("{hash}.jpeg"),
            format!("{hash}.PNG"),
            format!("{hash}.png.png"),
            format!("{hash}.png\n"),
            format!("{}.png", "A".repeat(64)),
            format!("{}.png", "g".repeat(64)),
            format!("{}.png", "a".repeat(63)),
            format!("{}.png", "a".repeat(65)),
            format!("../{hash}.png"),
            format!("{hash}/x.png"),
            format!("{hash}.png/.."),
        ] {
            assert!(!is_asset_name(&bad), "{bad:?}");
        }
    }

    #[test]
    fn the_signature_decides_the_extension() {
        for (bytes, extension) in [
            (PNG, "png"),
            (b"\xff\xd8\xff\xe0jpeg".as_slice(), "jpg"),
            (b"GIF87a".as_slice(), "gif"),
            (b"GIF89a".as_slice(), "gif"),
            (b"BMxxxx".as_slice(), "bmp"),
            (b"RIFF\0\0\0\0WEBPrest".as_slice(), "webp"),
        ] {
            assert_eq!(image_extension(bytes), Some(extension));
            let name = asset_name_for(bytes).unwrap();
            assert!(is_asset_name(&name), "{name}");
            assert!(name.ends_with(&format!(".{extension}")), "{name}");
            verify_asset(&name, bytes).unwrap();
        }
        for bytes in [
            b"".as_slice(),
            b"plain text",
            b"\x89PNG",
            b"RIFF\0\0\0\0WAVE",
            b"<svg/>",
        ] {
            assert_eq!(asset_name_for(bytes), Err(AssetError::NotAnImage));
        }
    }

    #[test]
    fn verification_needs_the_right_size_extension_and_hash() {
        let name = asset_name_for(PNG).unwrap();
        let hash = name.trim_end_matches(".png");
        assert_eq!(
            verify_asset(&format!("{hash}.jpg"), PNG),
            Err(AssetError::ExtensionMismatch)
        );
        assert_eq!(
            verify_asset(&format!("{}.png", "0".repeat(64)), PNG),
            Err(AssetError::HashMismatch)
        );
        assert_eq!(
            verify_asset("../evil.png", PNG),
            Err(AssetError::InvalidName)
        );
        let mut large = PNG.to_vec();
        large.resize(MAX_ASSET_BYTES as usize + 1, 0);
        assert_eq!(asset_name_for(&large), Err(AssetError::TooLarge));
        assert_eq!(verify_asset(&name, &large), Err(AssetError::TooLarge));
        large.truncate(MAX_ASSET_BYTES as usize);
        assert!(asset_name_for(&large).is_ok());
    }
}
