// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Downloads CockroachDB binary

use anyhow::{bail, Context, Result};
use camino::Utf8Path;
use flate2::bufread::GzDecoder;
use sha2::Digest;
use std::collections::HashMap;
use std::io::Write;
use std::sync::OnceLock;
use std::time::Duration;
use strum::Display;
use tar::Archive;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const RETRY_ATTEMPTS: usize = 3;

#[derive(Display)]
enum Os {
    Illumos,
    Linux,
    Mac,
}

impl Os {
    fn env_name(&self) -> &'static str {
        match self {
            Os::Illumos => "ILLUMOS",
            Os::Linux => "LINUX",
            Os::Mac => "DARWIN",
        }
    }
}

fn os_name() -> Result<Os> {
    let os = match std::env::consts::OS {
        "linux" => Os::Linux,
        "macos" => Os::Mac,
        "solaris" | "illumos" => Os::Illumos,
        other => bail!("OS not supported: {other}"),
    };
    Ok(os)
}

enum ChecksumAlgorithm {
    Sha2,
}

impl ChecksumAlgorithm {
    async fn checksum(&self, path: &Utf8Path) -> Result<String> {
        match self {
            ChecksumAlgorithm::Sha2 => sha2_checksum(path).await,
        }
    }
}

/// Returns the hex, lowercase sha2 checksum of a file at `path`.
async fn sha2_checksum(path: &Utf8Path) -> Result<String> {
    let mut buf = vec![0u8; 65536];
    let mut file = tokio::fs::File::open(path).await?;
    let mut ctx = sha2::Sha256::new();
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        ctx.write_all(&buf[0..n])?;
    }

    let digest = ctx.finalize();
    Ok(format!("{digest:x}"))
}

struct Downloader<'a> {
    /// The path to the "out" directory where the cockroachdb directory should
    /// be placed
    output_dir: &'a Utf8Path,

    /// A string of the CockroachDB version
    version: &'static str,

    /// A string of key/value pairs for the CockroachDB binaries
    checksums_str: &'static str,
}

impl<'a> Downloader<'a> {
    async fn download_cockroach(&self) -> Result<()> {
        let os = os_name()?;

        let download_dir = self.output_dir.join("downloads");
        let destination_dir = self.output_dir.join("cockroachdb");

        let [checksum] = get_values_from_string(
            [&format!("CIDL_SHA256_{}", os.env_name())],
            self.checksums_str,
        )
        .await?;

        let version = self.version.trim();
        let (url_base, suffix) = match os {
            Os::Illumos => (
                "https://oxide-cockroachdb-build.s3.us-west-2.amazonaws.com",
                "tar.gz",
            ),
            Os::Linux | Os::Mac => ("https://binaries.cockroachdb.com", "tgz"),
        };
        let build = match os {
            Os::Illumos => "illumos",
            Os::Linux => "linux-amd64",
            Os::Mac => "darwin-10.9-amd64",
        };

        let version_directory = format!("cockroach-{version}");
        let tarball_name = format!("{version_directory}.{build}");
        let tarball_filename = format!("{tarball_name}.{suffix}");
        let tarball_url = format!("{url_base}/{tarball_filename}");

        let tarball_path = download_dir.join(tarball_filename);

        tokio::fs::create_dir_all(&download_dir).await?;
        tokio::fs::create_dir_all(&destination_dir).await?;

        download_file_and_verify(
            &tarball_path,
            &tarball_url,
            ChecksumAlgorithm::Sha2,
            &checksum,
        )
        .await?;

        // We unpack the tarball in the download directory to emulate the old
        // behavior. This could be a little more consistent with Clickhouse.
        println!("tarball path: {tarball_path}");
        unpack_tarball(&tarball_path, &download_dir).await?;

        // This is where the binary will end up eventually
        let cockroach_binary = destination_dir.join("bin/cockroach");

        // Re-shuffle the downloaded tarball to our "destination" location.
        //
        // This ensures some uniformity, even though different platforms bundle
        // the Cockroach package differently.
        let binary_dir = destination_dir.join("bin");
        tokio::fs::create_dir_all(&binary_dir).await?;
        match os {
            Os::Illumos => {
                let src = tarball_path.with_file_name(version_directory);
                let dst = &destination_dir;
                println!("Copying from {src} to {dst}");
                copy_dir_all(&src, dst)?;
            }
            Os::Linux | Os::Mac => {
                let src = tarball_path.with_file_name(tarball_name).join("cockroach");
                tokio::fs::copy(src, &cockroach_binary).await?;
            }
        }

        println!("Checking that binary works");
        cockroach_confirm_binary_works(&cockroach_binary).await?;

        Ok(())
    }
}

fn copy_dir_all(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in src.read_dir_utf8()? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), &dst.join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Parses a string of the format:
///
/// ```ignore
/// KEY1="value1"
/// KEY2="value2"
/// ```
///
/// And returns an array of the values in the same order as keys.
async fn get_values_from_string<const N: usize>(
    keys: [&str; N],
    content: &'static str,
) -> Result<[String; N]> {
    // Map of "key" => "Position in output".
    let mut keys: HashMap<&str, usize> =
        keys.into_iter().enumerate().map(|(i, s)| (s, i)).collect();

    const EMPTY_STRING: String = String::new();
    let mut values = [EMPTY_STRING; N];

    for line in content.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim_matches('"');
        if let Some(i) = keys.remove(key) {
            values[i] = value.to_string();
        }
    }
    if !keys.is_empty() {
        bail!("Could not find keys: {:?}", keys.keys().collect::<Vec<_>>(),);
    }
    Ok(values)
}

/// Downloads a file and verifies the checksum.
///
/// If the file already exists and the checksum matches,
/// avoids performing the download altogether.
async fn download_file_and_verify(
    path: &Utf8Path,
    url: &str,
    algorithm: ChecksumAlgorithm,
    checksum: &str,
) -> Result<()> {
    let do_download = if path.exists() {
        println!("Already downloaded ({path})");
        if algorithm.checksum(path).await? == checksum {
            println!("Checksum matches already downloaded file - skipping download");
            false
        } else {
            println!("Checksum mismatch - retrying download");
            true
        }
    } else {
        true
    };

    if do_download {
        for attempt in 1..=RETRY_ATTEMPTS {
            println!("Downloading {path} (attempt {attempt}/{RETRY_ATTEMPTS})");
            match streaming_download(url, path).await {
                Ok(()) => break,
                Err(err) => {
                    if attempt == RETRY_ATTEMPTS {
                        return Err(err);
                    } else {
                        eprintln!("Download failed, retrying: {err}");
                    }
                }
            }
        }
    }

    let observed_checksum = algorithm.checksum(path).await?;
    if observed_checksum != checksum {
        bail!("Checksum mismatch (saw {observed_checksum}, expected {checksum})");
    }
    Ok(())
}

async fn cockroach_confirm_binary_works(binary: &Utf8Path) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.arg("version");

    let output = cmd
        .output()
        .await
        .context(format!("Failed to run {binary}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8(output.stderr).unwrap_or_else(|_| String::new());
        bail!("{binary} failed: {} (stderr: {stderr})", output.status);
    }
    Ok(())
}

/// Send a GET request to `url`, downloading the contents to `path`.
///
/// Writes the response to the file as it is received.
async fn streaming_download(url: &str, path: &Utf8Path) -> Result<()> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

    let client = CLIENT.get_or_init(|| {
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(3600))
            .tcp_keepalive(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .unwrap()
    });
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut tarball = tokio::fs::File::create(&path).await?;
    while let Some(chunk) = response.chunk().await? {
        tarball.write_all(chunk.as_ref()).await?;
    }
    Ok(())
}

async fn unpack_tarball(tarball_path: &Utf8Path, destination_dir: &Utf8Path) -> Result<()> {
    println!("Unpacking {tarball_path} to {destination_dir}");
    let tarball_path = tarball_path.to_owned();
    let destination_dir = destination_dir.to_owned();

    let task = tokio::task::spawn_blocking(move || {
        let reader = std::fs::File::open(tarball_path)?;
        let buf_reader = std::io::BufReader::new(reader);
        let gz = GzDecoder::new(buf_reader);
        let mut archive = Archive::new(gz);
        archive.unpack(&destination_dir)?;
        Ok(())
    });
    task.await?
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=tools/cockroachdb_checksums");
    println!("cargo:rerun-if-changed=tools/cockroachdb_version");

    let out_dir = std::env::var("OUT_DIR").expect("Need OUT_DIR");
    let downloader = Downloader {
        output_dir: Utf8Path::new(&out_dir),
        version: include_str!("tools/cockroachdb_version"),
        checksums_str: include_str!("tools/cockroachdb_checksums"),
    };
    downloader.download_cockroach().await
}
