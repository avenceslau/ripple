//! Build script for monoripple - handles building tsgo from source.
//!
//! When the `bundled-tsgo` feature is enabled (default), this script:
//! 1. Clones/updates the typescript-go repository from GitHub
//! 2. Applies any custom patches from patches/tsgo/
//! 3. Cross-compiles tsgo using Go's native cross-compilation
//! 4. Extracts TypeScript lib files needed for type resolution
//! 5. Compresses everything with zstd for smaller binary size
//! 6. Writes to OUT_DIR for inclusion via include_bytes!

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Git repository URL for typescript-go
const TSGO_REPO: &str = "https://github.com/microsoft/typescript-go";

/// Pinned git commit SHA for reproducible builds.
/// The typescript-go repo doesn't use tags, so we pin to a specific commit.
/// Update this when upgrading tsgo.
const TSGO_COMMIT: &str = "89d5d5b2849a0db0957065889ca58536fa6d2e4a";

/// Path to patches relative to crate root
const PATCHES_DIR: &str = "patches/tsgo";

fn main() {
    // Only run for bundled-tsgo feature
    if env::var("CARGO_FEATURE_BUNDLED_TSGO").is_err() {
        println!("cargo:warning=Building without bundled tsgo (feature disabled)");
        create_empty_marker();
        return;
    }

    let target = env::var("TARGET").expect("TARGET env var not set");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR env var not set"));
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));

    // Set up rerun triggers
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BUNDLED_TSGO");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-changed=patches/tsgo");

    // Map Rust target triple to Go GOOS/GOARCH
    let (goos, goarch) = match target.as_str() {
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => ("linux", "amd64"),
        "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => ("linux", "arm64"),
        "arm-unknown-linux-gnueabihf" | "armv7-unknown-linux-gnueabihf" => ("linux", "arm"),
        "x86_64-apple-darwin" => ("darwin", "amd64"),
        "aarch64-apple-darwin" => ("darwin", "arm64"),
        "x86_64-pc-windows-msvc" | "x86_64-pc-windows-gnu" => ("windows", "amd64"),
        "aarch64-pc-windows-msvc" => ("windows", "arm64"),
        _ => {
            println!("cargo:warning=Unsupported target for bundled tsgo: {target}");
            println!(
                "cargo:warning=Build will succeed but typed registry precision will require an external tsgo installation"
            );
            create_empty_marker();
            return;
        }
    };

    // Determine binary name based on target OS
    let binary_name = if target.contains("windows") {
        "tsgo.exe"
    } else {
        "tsgo"
    };

    // Check for required tools
    if let Err(e) = check_required_tools() {
        println!("cargo:warning={e}");
        println!(
            "cargo:warning=Build will succeed but typed registry precision will require an external tsgo installation"
        );
        create_empty_marker();
        return;
    }

    // Get source directory in target/tsgo-source
    let target_dir = manifest_dir.join("target");
    let source_dir = target_dir.join("tsgo-source");

    // Collect patches
    let patches_dir = manifest_dir.join(PATCHES_DIR);
    let patches = match collect_patches(&patches_dir) {
        Ok(p) => p,
        Err(e) => {
            println!("cargo:warning=Failed to collect patches: {e}");
            create_empty_marker();
            return;
        }
    };

    // Compute cache key from tag + patches content
    let cache_key = compute_cache_key(&patches);
    let cache_marker = out_dir.join(format!("tsgo_cache_{}.marker", cache_key));
    let cached_binary = out_dir.join("tsgo.zst");
    let cached_libs = out_dir.join("tsgo_libs.tar.zst");

    // Check if we can use cached build
    if cache_marker.exists() && cached_binary.exists() && cached_libs.exists() {
        println!("cargo:warning=Using cached tsgo build (cache key: {cache_key})");
        // Write the embedded marker
        let marker_path = out_dir.join("tsgo_embedded_marker");
        fs::write(&marker_path, "1").expect("Failed to write marker file");
        return;
    }

    // Clone or update the repository
    if let Err(e) = setup_source_repo(&source_dir) {
        println!("cargo:warning=Failed to setup typescript-go source: {e}");
        create_empty_marker();
        return;
    }

    // Reset to clean state and apply patches
    if let Err(e) = prepare_source(&source_dir, &patches) {
        println!("cargo:warning=Failed to prepare source: {e}");
        create_empty_marker();
        return;
    }

    // Build tsgo
    let built_binary = source_dir.join("tsgo").join(binary_name);
    if let Err(e) = build_tsgo(&source_dir, &built_binary, goos, goarch) {
        println!("cargo:warning=Failed to build tsgo: {e}");
        create_empty_marker();
        return;
    }

    // Read and compress the tsgo binary
    let tsgo_binary = match fs::read(&built_binary) {
        Ok(data) => data,
        Err(e) => {
            println!("cargo:warning=Failed to read built tsgo binary: {e}");
            create_empty_marker();
            return;
        }
    };

    println!(
        "cargo:warning=Compressing tsgo binary ({} bytes)",
        tsgo_binary.len()
    );

    let compressed = match zstd::encode_all(tsgo_binary.as_slice(), 19) {
        Ok(data) => data,
        Err(e) => {
            println!("cargo:warning=Failed to compress tsgo binary: {e}");
            create_empty_marker();
            return;
        }
    };

    println!(
        "cargo:warning=Compressed tsgo: {} -> {} bytes ({:.1}% reduction)",
        tsgo_binary.len(),
        compressed.len(),
        (1.0 - compressed.len() as f64 / tsgo_binary.len() as f64) * 100.0
    );

    // Write compressed binary to OUT_DIR
    if let Err(e) = fs::write(&cached_binary, &compressed) {
        println!("cargo:warning=Failed to write compressed tsgo: {e}");
        create_empty_marker();
        return;
    }

    // Collect and compress TypeScript lib files from source
    let lib_dir = source_dir.join("internal").join("bundled").join("libs");
    let libs_data = match collect_lib_files(&lib_dir) {
        Ok(data) => data,
        Err(e) => {
            println!("cargo:warning=Failed to collect lib files: {e}");
            Vec::new()
        }
    };

    if !libs_data.is_empty() {
        let compressed_libs = match zstd::encode_all(libs_data.as_slice(), 19) {
            Ok(data) => data,
            Err(e) => {
                println!("cargo:warning=Failed to compress lib files: {e}");
                Vec::new()
            }
        };

        if !compressed_libs.is_empty() {
            if let Err(e) = fs::write(&cached_libs, &compressed_libs) {
                println!("cargo:warning=Failed to write lib files: {e}");
            } else {
                println!(
                    "cargo:warning=Embedded {} bytes of TypeScript lib files (compressed)",
                    compressed_libs.len()
                );
            }
        }
    }

    // Write cache marker
    if let Err(e) = fs::write(&cache_marker, &cache_key) {
        println!("cargo:warning=Failed to write cache marker: {e}");
    }

    // Clean up old cache markers
    cleanup_old_cache_markers(&out_dir, &cache_key);

    // Write the build version info file for embedded.rs to use
    // This includes the cache key so the runtime cache is invalidated when patches change
    let version_info = format!(
        "/// Auto-generated by build.rs - do not edit\n\
         pub const TSGO_COMMIT: &str = \"{}\";\n\
         pub const TSGO_BUILD_HASH: &str = \"{}\";\n",
        TSGO_COMMIT, cache_key
    );
    let version_path = out_dir.join("tsgo_version.rs");
    fs::write(&version_path, version_info).expect("Failed to write version info");

    // Write marker file indicating successful embedding
    let marker_path = out_dir.join("tsgo_embedded_marker");
    fs::write(&marker_path, "1").expect("Failed to write marker file");

    println!(
        "cargo:warning=Successfully built and embedded tsgo from source (commit: {TSGO_COMMIT}, hash: {cache_key})"
    );
}

/// Check that required tools (git, go) are installed.
fn check_required_tools() -> Result<(), String> {
    // Check for git
    let git_check = Command::new("git").arg("--version").output();

    match git_check {
        Ok(output) if output.status.success() => {}
        _ => {
            return Err(
                "git is required to build tsgo from source. Please install git.".to_string(),
            );
        }
    }

    // Check for go
    let go_check = Command::new("go").arg("version").output();

    match go_check {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            println!("cargo:warning=Found Go: {}", version.trim());
        }
        _ => {
            return Err(
                "Go 1.24+ is required to build tsgo from source. Install from https://go.dev/dl/"
                    .to_string(),
            );
        }
    }

    Ok(())
}

/// Collect patch files from the patches directory.
fn collect_patches(patches_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut patches = Vec::new();

    if !patches_dir.exists() {
        return Ok(patches);
    }

    for entry in fs::read_dir(patches_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "patch")
        {
            patches.push(path);
        }
    }

    // Sort patches by filename for deterministic order
    patches.sort();

    Ok(patches)
}

/// Compute a cache key based on the commit and patch contents.
fn compute_cache_key(patches: &[PathBuf]) -> String {
    let mut hasher = DefaultHasher::new();

    // Hash the commit SHA
    TSGO_COMMIT.hash(&mut hasher);

    // Hash each patch file's contents
    for patch in patches {
        if let Ok(contents) = fs::read(patch) {
            contents.hash(&mut hasher);
        }
        // Also hash the filename
        if let Some(name) = patch.file_name() {
            name.to_string_lossy().hash(&mut hasher);
        }
    }

    format!("{:016x}", hasher.finish())
}

/// Clone or update the typescript-go repository.
fn setup_source_repo(source_dir: &Path) -> Result<(), String> {
    if source_dir.exists() {
        // Repository exists, fetch updates
        println!("cargo:warning=Updating typescript-go repository...");

        let fetch = Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(source_dir)
            .output()
            .map_err(|e| format!("Failed to run git fetch: {e}"))?;

        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            return Err(format!("git fetch failed: {stderr}"));
        }
    } else {
        // Clone the repository (without submodules - we only need to build)
        println!("cargo:warning=Cloning typescript-go repository (this may take a moment)...");

        // Ensure parent directory exists
        if let Some(parent) = source_dir.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Failed to create target dir: {e}"))?;
        }

        // Clone without depth limit since we need to checkout a specific commit
        // that might not be the latest
        let clone = Command::new("git")
            .args(["clone", TSGO_REPO, &source_dir.to_string_lossy()])
            .output()
            .map_err(|e| format!("Failed to run git clone: {e}"))?;

        if !clone.status.success() {
            let stderr = String::from_utf8_lossy(&clone.stderr);
            return Err(format!("git clone failed: {stderr}"));
        }
    }

    Ok(())
}

/// Prepare the source directory: checkout the correct commit and apply patches.
fn prepare_source(source_dir: &Path, patches: &[PathBuf]) -> Result<(), String> {
    // Reset to clean state at the specified commit
    println!("cargo:warning=Checking out commit {TSGO_COMMIT}...");

    let checkout = Command::new("git")
        .args(["checkout", "--force", TSGO_COMMIT])
        .current_dir(source_dir)
        .output()
        .map_err(|e| format!("Failed to run git checkout: {e}"))?;

    if !checkout.status.success() {
        let stderr = String::from_utf8_lossy(&checkout.stderr);
        return Err(format!("git checkout failed: {stderr}"));
    }

    // Clean any untracked files
    let clean = Command::new("git")
        .args(["clean", "-fdx"])
        .current_dir(source_dir)
        .output()
        .map_err(|e| format!("Failed to run git clean: {e}"))?;

    if !clean.status.success() {
        let stderr = String::from_utf8_lossy(&clean.stderr);
        println!("cargo:warning=git clean warning: {stderr}");
    }

    // Apply patches
    for patch in patches {
        let patch_name = patch.file_name().unwrap_or_default().to_string_lossy();
        println!("cargo:warning=Applying patch: {patch_name}");

        let apply = Command::new("git")
            .args(["apply", "--verbose", &patch.to_string_lossy()])
            .current_dir(source_dir)
            .output()
            .map_err(|e| format!("Failed to run git apply for {patch_name}: {e}"))?;

        if !apply.status.success() {
            let stderr = String::from_utf8_lossy(&apply.stderr);
            return Err(format!("Failed to apply patch {patch_name}: {stderr}"));
        }
    }

    if !patches.is_empty() {
        println!(
            "cargo:warning=Applied {} patch(es) successfully",
            patches.len()
        );
    }

    Ok(())
}

/// Build tsgo using Go cross-compilation.
fn build_tsgo(
    source_dir: &Path,
    output_path: &Path,
    goos: &str,
    goarch: &str,
) -> Result<(), String> {
    println!("cargo:warning=Building tsgo for {goos}/{goarch}...");

    // Ensure output directory exists
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create output dir: {e}"))?;
    }

    let build = Command::new("go")
        .args([
            "build",
            "-trimpath",
            "-ldflags=-s -w",
            "-o",
            &output_path.to_string_lossy(),
            "./cmd/tsgo",
        ])
        .env("GOOS", goos)
        .env("GOARCH", goarch)
        .env("CGO_ENABLED", "0")
        .current_dir(source_dir)
        .output()
        .map_err(|e| format!("Failed to run go build: {e}"))?;

    if !build.status.success() {
        let stderr = String::from_utf8_lossy(&build.stderr);
        let stdout = String::from_utf8_lossy(&build.stdout);
        return Err(format!(
            "go build failed:\nstdout: {stdout}\nstderr: {stderr}"
        ));
    }

    // Verify the binary was created
    if !output_path.exists() {
        return Err(format!(
            "go build completed but binary not found at {}",
            output_path.display()
        ));
    }

    let size = fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    println!("cargo:warning=Built tsgo binary: {} bytes", size);

    Ok(())
}

/// Collect TypeScript lib.*.d.ts files into a tar archive (in memory).
fn collect_lib_files(lib_dir: &Path) -> io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());

    if !lib_dir.exists() {
        println!(
            "cargo:warning=Lib directory not found: {}",
            lib_dir.display()
        );
        return Ok(Vec::new());
    }

    let mut count = 0;
    for entry in fs::read_dir(lib_dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = path.file_name().unwrap().to_string_lossy();

        // Only include lib.*.d.ts files
        if file_name.starts_with("lib.") && file_name.ends_with(".d.ts") {
            let mut file = File::open(&path)?;
            let mut contents = Vec::new();
            file.read_to_end(&mut contents)?;

            let mut header = tar::Header::new_gnu();
            header.set_path(file_name.as_ref())?;
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();

            builder.append(&header, contents.as_slice())?;
            count += 1;
        }
    }

    println!("cargo:warning=Collected {count} TypeScript lib files");
    builder.into_inner()
}

/// Clean up old cache marker files.
fn cleanup_old_cache_markers(out_dir: &Path, current_key: &str) {
    if let Ok(entries) = fs::read_dir(out_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                let name = name.to_string_lossy();
                if name.starts_with("tsgo_cache_")
                    && name.ends_with(".marker")
                    && !name.contains(current_key)
                {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

/// Create an empty marker indicating no embedded tsgo.
fn create_empty_marker() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR env var not set"));

    // Create empty placeholder files so include_bytes! doesn't fail
    let compressed_path = out_dir.join("tsgo.zst");
    fs::write(&compressed_path, &[] as &[u8]).ok();

    let libs_path = out_dir.join("tsgo_libs.tar.zst");
    fs::write(&libs_path, &[] as &[u8]).ok();

    // Do NOT write the marker file - its absence indicates no embedding
}
