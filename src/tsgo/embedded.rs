//! Embedded tsgo binary and TypeScript lib files.
#![allow(dead_code)]
//!
//! This module provides access to the bundled tsgo binary when the `bundled-tsgo`
//! feature is enabled. The binary is compressed with zstd to reduce the final
//! executable size.
//!
//! At runtime, the binary is extracted to a cache directory and reused across
//! invocations.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

// Include generated version info from build.rs
// This contains TSGO_COMMIT and TSGO_BUILD_HASH
#[cfg(feature = "bundled-tsgo")]
include!(concat!(env!("OUT_DIR"), "/tsgo_version.rs"));

#[cfg(not(feature = "bundled-tsgo"))]
mod version_fallback {
    pub const TSGO_COMMIT: &str = "unknown";
    pub const TSGO_BUILD_HASH: &str = "unknown";
}
#[cfg(not(feature = "bundled-tsgo"))]
use version_fallback::*;

/// Tsgo version string for display purposes.
pub const TSGO_VERSION: &str = TSGO_COMMIT;

/// Compressed tsgo binary (zstd).
/// Empty if bundled-tsgo feature is disabled or download failed.
#[cfg(feature = "bundled-tsgo")]
static TSGO_BINARY_ZST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tsgo.zst"));

/// Compressed TypeScript lib files (tar + zstd).
/// Empty if bundled-tsgo feature is disabled or collection failed.
#[cfg(feature = "bundled-tsgo")]
static TSGO_LIBS_TAR_ZST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tsgo_libs.tar.zst"));

/// Check if tsgo binary is embedded and available.
#[cfg(feature = "bundled-tsgo")]
pub fn is_embedded() -> bool {
    !TSGO_BINARY_ZST.is_empty()
}

#[cfg(not(feature = "bundled-tsgo"))]
pub fn is_embedded() -> bool {
    false
}

/// Get the cache directory for extracted tsgo binary.
/// Uses TSGO_BUILD_HASH to ensure cache is invalidated when patches change.
fn get_cache_dir() -> io::Result<PathBuf> {
    #[cfg(feature = "bundled-tsgo")]
    {
        // Try multiple cache locations in order of preference:
        // 1. XDG_CACHE_HOME (if set)
        // 2. Standard cache dir (~/.cache on Linux)
        // 3. System temp directory
        let base = std::env::var("XDG_CACHE_HOME")
            .ok()
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(dirs::cache_dir)
            .unwrap_or_else(std::env::temp_dir);

        // Use build hash to invalidate cache when commit or patches change
        let cache_dir = base
            .join("monoripple")
            .join(format!("tsgo-{}", TSGO_BUILD_HASH));
        fs::create_dir_all(&cache_dir).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Failed to create cache directory {}: {}",
                    cache_dir.display(),
                    e
                ),
            )
        })?;
        Ok(cache_dir)
    }

    #[cfg(not(feature = "bundled-tsgo"))]
    {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "bundled-tsgo feature not enabled",
        ))
    }
}

/// Extract the embedded tsgo binary to the cache directory.
///
/// Returns the path to the extracted binary. If the binary is already
/// extracted and valid, returns the cached path without re-extracting.
///
/// Also extracts the TypeScript lib files to the same directory, as tsgo
/// expects them to be alongside the binary.
///
/// # Errors
///
/// Returns an error if:
/// - The `bundled-tsgo` feature is disabled
/// - No binary was embedded (download failed at build time)
/// - Decompression fails
/// - Writing to cache directory fails
#[cfg(feature = "bundled-tsgo")]
pub fn extract_tsgo_binary() -> io::Result<PathBuf> {
    if TSGO_BINARY_ZST.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "No embedded tsgo binary (build-time download may have failed)",
        ));
    }

    let cache_dir = get_cache_dir()?;

    // Determine binary name based on target OS
    let binary_name = if cfg!(windows) { "tsgo.exe" } else { "tsgo" };
    let binary_path = cache_dir.join(binary_name);

    // Check if already extracted (use lib.d.ts as marker for complete extraction)
    let lib_marker = cache_dir.join("lib.d.ts");
    if binary_path.exists() && lib_marker.exists() {
        // Verify binary is non-empty
        let metadata = fs::metadata(&binary_path)?;
        if metadata.len() > 0 {
            return Ok(binary_path);
        }
        // Invalid file, will re-extract below
    }

    // Decompress the binary
    let decompressed = zstd::decode_all(TSGO_BINARY_ZST).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to decompress tsgo binary: {e}"),
        )
    })?;

    // Write to a unique temp file to avoid conflicts with parallel processes/threads.
    // We use both process ID and a random suffix to ensure uniqueness even when
    // multiple threads in the same process run tests concurrently.
    let temp_suffix = format!(
        "{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let temp_path = cache_dir.join(format!("{}.tmp.{}", binary_name, temp_suffix));

    // Write the decompressed binary
    {
        let mut file = File::create(&temp_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to create temp file {}: {}", temp_path.display(), e),
            )
        })?;
        file.write_all(&decompressed)?;
        file.sync_all()?;
    }

    // Set executable permission on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o755)).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Failed to set permissions on {}: {}",
                    temp_path.display(),
                    e
                ),
            )
        })?;
    }

    // Move to final location - use copy+delete as fallback for cross-filesystem scenarios
    if let Err(rename_err) = fs::rename(&temp_path, &binary_path) {
        // Rename failed, try copy+delete instead
        fs::copy(&temp_path, &binary_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Failed to copy {} to {} (rename failed with: {}): {}",
                    temp_path.display(),
                    binary_path.display(),
                    rename_err,
                    e
                ),
            )
        })?;
        let _ = fs::remove_file(&temp_path); // Best effort cleanup

        // Re-set permissions after copy (copy may not preserve them)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary_path, fs::Permissions::from_mode(0o755))?;
        }
    }

    // Also extract lib files to the same directory (tsgo expects them there)
    if !TSGO_LIBS_TAR_ZST.is_empty() {
        extract_libs_to_dir(&cache_dir)?;
    }

    Ok(binary_path)
}

/// Extract TypeScript lib files to the specified directory.
#[cfg(feature = "bundled-tsgo")]
fn extract_libs_to_dir(dir: &Path) -> io::Result<()> {
    // Decompress the tar archive
    let decompressed = zstd::decode_all(TSGO_LIBS_TAR_ZST).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to decompress lib files: {e}"),
        )
    })?;

    // Extract tar archive
    let mut archive = tar::Archive::new(decompressed.as_slice());

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let file_name = path.file_name().unwrap_or_default();
        let dest_path = dir.join(file_name);

        let mut contents = Vec::new();
        entry.read_to_end(&mut contents)?;

        fs::write(&dest_path, &contents)?;
    }

    Ok(())
}

#[cfg(not(feature = "bundled-tsgo"))]
pub fn extract_tsgo_binary() -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "bundled-tsgo feature not enabled",
    ))
}

/// Extract the embedded TypeScript lib files to the cache directory.
///
/// Returns the path to the directory containing the .d.ts files (same as tsgo binary).
/// Note: This is automatically called by `extract_tsgo_binary()`, so you usually
/// don't need to call this directly.
///
/// # Errors
///
/// Returns an error if extraction fails or no lib files were embedded.
#[cfg(feature = "bundled-tsgo")]
pub fn extract_tsgo_libs() -> io::Result<PathBuf> {
    // Extracting the binary also extracts lib files to the same directory
    let binary_path = extract_tsgo_binary()?;
    Ok(binary_path.parent().unwrap().to_path_buf())
}

#[cfg(not(feature = "bundled-tsgo"))]
pub fn extract_tsgo_libs() -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "bundled-tsgo feature not enabled",
    ))
}

/// Clean up extracted tsgo files from cache.
///
/// Useful for forcing a fresh extraction on next use.
pub fn clean_cache() -> io::Result<()> {
    let cache_dir = get_cache_dir()?;
    if cache_dir.exists() {
        fs::remove_dir_all(&cache_dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_embedded() {
        // This will be true if built with bundled-tsgo and download succeeded
        let embedded = is_embedded();
        println!("tsgo embedded: {}", embedded);
    }

    #[test]
    #[cfg(feature = "bundled-tsgo")]
    fn test_extract_binary() {
        if !is_embedded() {
            println!("Skipping test: no embedded binary");
            return;
        }

        let path = extract_tsgo_binary().expect("Failed to extract tsgo binary");
        assert!(path.exists(), "Extracted binary should exist");

        let metadata = fs::metadata(&path).expect("Failed to get metadata");
        assert!(metadata.len() > 0, "Binary should not be empty");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = metadata.permissions();
            assert!(perms.mode() & 0o111 != 0, "Binary should be executable");
        }
    }

    #[test]
    #[cfg(feature = "bundled-tsgo")]
    fn test_extract_libs() {
        if !is_embedded() {
            println!("Skipping test: no embedded binary");
            return;
        }

        match extract_tsgo_libs() {
            Ok(path) => {
                assert!(path.exists(), "Lib directory should exist");
                // Check for at least one lib file
                let lib_d_ts = path.join("lib.d.ts");
                assert!(lib_d_ts.exists(), "lib.d.ts should exist");
            }
            Err(e) => {
                // Lib files might not be embedded
                println!("No lib files embedded: {}", e);
            }
        }
    }
}
