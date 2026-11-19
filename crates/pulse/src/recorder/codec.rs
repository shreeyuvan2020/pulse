//! Gets Cisco's prebuilt OpenH264 library. Cisco pays the H.264 patent
//! royalties for its own binaries only, and only when each user downloads
//! them straight from Cisco, so Pulse never ships or builds OpenH264 itself.
//! The library is downloaded once into Pulse's data folder, and the
//! `openh264` crate refuses to load it unless its SHA-256 matches a known
//! Cisco release.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use openh264::OpenH264API;

pub const NOTICE: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

/// Cisco's file name for this OS and CPU, or `None` where Cisco has no build.
fn file_name() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "openh264-2.7.0-win64.dll",
        ("windows", "x86") => "openh264-2.7.0-win32.dll",
        ("windows", "aarch64") => "openh264-2.7.0-win-arm64.dll",
        ("macos", "aarch64") => "libopenh264-2.7.0-mac-arm64.dylib",
        ("macos", "x86_64") => "libopenh264-2.7.0-mac-x64.dylib",
        ("linux", "x86_64") => "libopenh264-2.7.0-linux64.8.so",
        ("linux", "x86") => "libopenh264-2.7.0-linux32.8.so",
        ("linux", "aarch64") => "libopenh264-2.7.0-linux-arm64.8.so",
        ("linux", "arm") => "libopenh264-2.7.0-linux-arm.8.so",
        _ => return None,
    })
}

/// Path of the library, downloading it from Cisco the first time.
pub fn ensure(dir: &Path) -> Result<PathBuf, String> {
    let name = file_name().ok_or("Cisco publishes no OpenH264 build for this computer")?;
    let path = dir.join(name);
    if path.exists() && load(&path).is_ok() {
        return Ok(path);
    }
    eprintln!("pulse: downloading the video codec from Cisco ({NOTICE})");
    let url = format!("https://ciscobinary.openh264.org/{name}.bz2");
    let fail = |error: String| format!("cannot download the video codec from {url}: {error}");
    let mut compressed = Vec::new();
    ureq::get(&url)
        .call()
        .map_err(|error| fail(error.to_string()))?
        .into_body()
        .into_reader()
        .take(8 << 20)
        .read_to_end(&mut compressed)
        .map_err(|error| fail(error.to_string()))?;
    let mut library = Vec::new();
    bzip2::read::BzDecoder::new(compressed.as_slice())
        .read_to_end(&mut library)
        .map_err(|error| fail(error.to_string()))?;

    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let partial = path.with_extension("partial");
    fs::write(&partial, &library).map_err(|error| error.to_string())?;
    if let Err(error) = load(&partial) {
        let _ = fs::remove_file(&partial);
        return Err(fail(format!(
            "the download is not a known Cisco build ({error})"
        )));
    }
    fs::rename(&partial, &path).map_err(|error| error.to_string())?;
    Ok(path)
}

/// Loads the library, checking its hash first.
pub fn load(path: &Path) -> Result<OpenH264API, String> {
    OpenH264API::from_blob_path(path).map_err(|error| error.to_string())
}
