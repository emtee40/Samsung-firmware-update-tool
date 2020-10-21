mod file;

use std::{
    cmp,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, stderr, Read, Seek, SeekFrom, Stderr, Write},
    ops::Range,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use anyhow::{anyhow, Context, Result};
use clap::Clap;
use crc32fast::Hasher;
use futures::stream::FuturesUnordered;
use log::{debug, Level, log_enabled, trace};
use serde::{Deserialize, Serialize};
use tokio::{
    signal::ctrl_c,
    stream::StreamExt,
    sync::{mpsc, oneshot},
    task,
};

use progresslib::{ProgressBar, ProgressDrawMode};
use samfuslib::{
    crypto::{FusFileAes128, FusKeys},
    fus::{FirmwareInfo, FusClientBuilder},
    range::split_range,
    version::FwVersion,
};

use file::{rename_atomic, write_all_at};

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const STATE_EXT: &str = concat!(env!("CARGO_PKG_NAME"), "_state");
const TEMP_EXT: &str = concat!(env!("CARGO_PKG_NAME"), "_temp");

// Minimum download chunk size per thread
const MIN_CHUNK_SIZE: u64 = 1 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
struct DownloadState {
    remaining: Vec<(u64, u64)>,
}

impl DownloadState {
    fn from_ranges(ranges: &[Range<u64>]) -> Self {
        Self {
            remaining: ranges.iter().map(|r| (r.start, r.end)).collect()
        }
    }

    fn to_ranges(&self) -> Vec<Range<u64>> {
        self.remaining.iter().map(|&(start, end)| start..end).collect()
    }

    fn read_file(path: &Path) -> Result<Self> {
        let f = File::open(path)?;
        let data = serde_json::from_reader(f)?;
        Ok(data)
    }

    fn write_file(&self, path: &Path) -> Result<()> {
        let f = File::create(path)?;
        serde_json::to_writer(f, self)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct TaskId(usize);

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Task#{}", self.0)
    }
}

#[derive(Debug)]
struct ProgressMessage {
    task_id: TaskId,
    bytes: u64,
    // Controller replies with new ending offset
    resp: oneshot::Sender<u64>,
}

/// Download a byte range of a firmware file. The number of bytes downloaded per
/// loop iteration will be sent to the specified channel via a ProgressMessage.
/// The receiver of the message must reply with the new ending offset for this
/// download via the oneshot channel in the `resp` field. An appropriate error
/// will be returned if the full range (subject to modification) cannot be fully
/// downloaded (eg. premature EOF is an error).
async fn download_range(
    task_id: TaskId,
    client_builder: FusClientBuilder,
    mut file: File,
    info: Arc<FirmwareInfo>,
    initial_range: Range<u64>,
    mut channel: mpsc::Sender<ProgressMessage>,
) -> Result<()> {
    debug!("[{}] Starting download with initial range: {:?}", task_id, initial_range);

    let mut client = client_builder.build()
        .context("Could not initialize FUS client")?;
    let mut stream = client.download(&info, initial_range.clone()).await
        .context("Could not start download")?;
    let mut range = initial_range.clone();

    while range.start < range.end {
        let data = match stream.next().await {
            Some(x) => x?,
            None => {
                debug!("[{}] Received unexpected EOF from server", task_id);
                return Err(anyhow!("Unexpected EOF from server"));
            }
        };
        trace!("[{}] Received {} bytes", task_id, data.len());

        // This may overlap with another task's write when a range split occurs,
        // but the same data will be written anyway, so it's not a huge deal.
        task::block_in_place(|| {
            // tokio::fs doesn't implement FileExt, so use the std::fs blocking
            // calls instead
            write_all_at(&mut file, &data, range.start)
        }).with_context(|| format!(
            "Failed to write {} bytes to output file at offset {}",
            data.len(), range.start,
        ))?;

        let consumed = cmp::min(range.end - range.start, data.len() as u64);
        range.start += consumed;

        // Report progress to controller.
        let (tx, rx) = oneshot::channel();
        let msg = ProgressMessage {
            task_id,
            bytes: consumed,
            resp: tx,
        };
        channel.send(msg).await?;

        // Get new ending offset from controller.
        let new_end = rx.await?;
        if new_end != range.end {
            debug!("[{}] Ending offset changed to {:?}", task_id, new_end);
            debug_assert!(new_end <= range.end);
            range.end = new_end;
        }
    }

    Ok(())
}

/// Create download task for a byte range. This just calls download_range() and
/// returns a tuple containing the task ID and the result.
async fn download_task(
    task_id: TaskId,
    client_builder: FusClientBuilder,
    file: File,
    info: Arc<FirmwareInfo>,
    initial_range: Range<u64>,
    channel: mpsc::Sender<ProgressMessage>,
) -> (TaskId, Result<()>) {
    (task_id, download_range(task_id, client_builder, file, info, initial_range, channel).await)
}

/// Download a set of file chunks in parallel. Expected or recoverable errors
/// are printed to stderr. Unrecoverable errors are returned as an Err. Download
/// progress is reported via the specified progress bar. Unless an unrecoverable
/// error occurs, the list of incomplete download ranges is returned. This will
/// be non-empty if the number of recoverable errors exceed the maximum
/// attempts.
async fn download_chunks(
    client_builder: FusClientBuilder,
    file: File,
    info: Arc<FirmwareInfo>,
    chunks: &[Range<u64>],
    max_errors: u8,
) -> Result<Vec<Range<u64>>> {
    let mut bar = create_progress_bar(info.size);
    let remaining: u64 = chunks.iter()
        .map(|r| r.end - r.start)
        .sum();
    bar.set_position(info.size - remaining)?;

    file.set_len(info.size)
        .context(format!("Could not set size of output file"))?;

    let mut task_ranges: Vec<_> = chunks.iter().cloned().collect();
    let mut tasks = FuturesUnordered::new();
    let mut error_count = 0u8;
    let (tx, mut rx) = mpsc::channel(task_ranges.len());

    // Start downloading evenly split chunks.
    for (i, task_range) in task_ranges.iter().enumerate() {
        tasks.push(tokio::spawn(download_task(
            TaskId(i),
            client_builder.clone(),
            file.try_clone().context("Could not duplicate file handle")?,
            info.clone(),
            task_range.clone(),
            tx.clone(),
        )));
    }

    loop {
        tokio::select! {
            // User hit ctrl c
            c = ctrl_c() => {
                c?;

                // The parent will take the remaining chunks and write it to a
                // state file.
                break;
            }

            // Received progress notification.
            p = rx.recv() => {
                // This channel never ends because tx is never dropped in this
                // function.
                let p = p.unwrap();

                bar.advance(p.bytes)?;

                let task_range = &mut task_ranges[p.task_id.0];
                task_range.start += p.bytes;

                p.resp.send(task_range.end).unwrap();
            }

            // Received completion message.
            r = tasks.next() => {
                match r {
                    // All tasks exited
                    None => {
                        debug!("All download tasks have exited");
                        break;
                    },

                    // Download task panicked
                    Some(Err(e)) => {
                        return Err(e).context("Unexpected panic in download task");
                    }

                    // Task completed successfully
                    Some(Ok((task_id, Ok(_)))) => {
                        debug!("[{}] Completed download", task_id);

                        if error_count >= max_errors {
                            debug!("Exceeded max error count: {}", max_errors);
                            continue;
                        }

                        // Otherwise, the task completed successfully. Find the
                        // largest in-progress chunk, split it into two, and
                        // start downloading the second half. This reduces the
                        // effect of one slow stream slowing down the entire
                        // download.
                        let largest_range = task_ranges.iter_mut()
                            .max_by_key(|s| s.end - s.start)
                            .unwrap();
                        if largest_range.start == largest_range.end {
                            debug!("Largest range is empty; download is complete");
                            continue;
                        }

                        debug!("Candidate for range splitting: {:?}", largest_range);

                        let ranges = split_range(largest_range.clone(), 2, Some(MIN_CHUNK_SIZE));
                        if ranges.len() < 2 {
                            debug!("Range is too small to be worth splitting");
                            continue;
                        }

                        largest_range.end = ranges[0].end;
                        let new_range = ranges[1].clone();

                        debug!("[{}] Downloading newly split range {:?}", task_id, new_range);
                        task_ranges[task_id.0] = new_range.clone();

                        tasks.push(tokio::spawn(download_task(
                            task_id,
                            client_builder.clone(),
                            file.try_clone().context("Could not duplicate file handle")?,
                            info.clone(),
                            new_range,
                            tx.clone(),
                        )));
                    }

                    // Task failed
                    Some(Ok((task_id, Err(e)))) => {
                        bar.println(format!("{:?}", e.context("Error encountered during download")))?;
                        error_count += 1;

                        if error_count >= max_errors {
                            debug!("Exceeded max error count: {}", max_errors);
                            continue;
                        }

                        eprintln!("Retrying (attempt {}/{}) ...", error_count, max_errors);
                        debug!("[{}] Retrying incomplete range {:?}", task_id, task_ranges[task_id.0]);

                        tasks.push(tokio::spawn(download_task(
                            task_id,
                            client_builder.clone(),
                            file.try_clone().context("Could not duplicate file handle")?,
                            info.clone(),
                            task_ranges[task_id.0].clone(),
                            tx.clone(),
                        )));
                    }
                }
            }
        }
    }

    let incomplete = task_ranges.into_iter()
        .filter(|r| r.end - r.start > 0)
        .collect();
    Ok(incomplete)
}

/// Query FUS for information about the specified firmware. If no version is
/// provided, the latest available version will be used.
async fn get_firmware_info(
    client_builder: FusClientBuilder,
    model: &str,
    region: &str,
    version: Option<FwVersion>,
) -> Result<FirmwareInfo> {
    let mut client = client_builder.build()
        .context("Could not initialize FUS client")?;
    let fw_version = match version {
        Some(v) => v,
        None => client.get_latest_version(model, region).await?,
    };
    let info = client.get_firmware_info(model, region, &fw_version).await?;

    Ok(info)
}

/// Decrypt file and compute the CRC32 checksum of the input file along the way.
fn crc32_and_decrypt(
    mut input_file: File,
    mut output_file: File,
    key: &[u8],
) -> Result<u32> {
    let mut size = input_file.seek(SeekFrom::End(0))
        .context("Failed to get input file size")?;
    input_file.seek(SeekFrom::Start(0))
        .context("Failed to seek input file")?;
    output_file.seek(SeekFrom::Start(0))
        .context("Failed to seek output file")?;

    let mut bar = create_progress_bar(size);
    let mut buf = [0u8; 1024 * 1024];
    let mut hasher = Hasher::new();
    let cipher = FusFileAes128::new(key);

    // Intentionally don't handle files that grow during reads
    while size > 0 {
        let to_read = cmp::min(size, buf.len() as u64);
        let read_buf = &mut buf[..to_read as usize];
        input_file.read_exact(read_buf)
            .context("Failed to read input file")?;

        hasher.update(read_buf);

        cipher.clone().decrypt_in_place(read_buf)
            .context("Failed to decrypt file")?;

        output_file.write_all(read_buf)
            .context("Failed to write output file")?;

        size -= to_read;
        bar.advance(to_read)?;
    }

    Ok(hasher.finalize())
}

/// Validate that the file's checksum matches the expected value from the
/// firmware info and decrypt the firmware.
async fn decrypt_firmware(
    input_file: File,
    output_file: File,
    info: Arc<FirmwareInfo>,
) -> Result<()> {
    let key = info.encryption_key()
        .context("Failed to compute encryption key")?;

    debug!("Firmware encryption key: {:?}", key);

    let crc32 = task::spawn_blocking(move || crc32_and_decrypt(
        input_file,
        output_file,
        &key,
    )).await??;

    if crc32 != info.crc {
        return Err(anyhow!(
            "Firmware's checksum ({:08X}) does not match expected checksum ({:08X})",
            crc32,
            info.crc,
        ));
    }

    Ok(())
}

/// Create a new progress bar with the specified length. The progress bar is not
/// immediately rendered.
fn create_progress_bar(len: u64) -> ProgressBar<Stderr> {
    let mut bar = ProgressBar::new(stderr(), len);
    if log_enabled!(Level::Debug) {
        // The escape sequences for the interactive progress bar would clobber
        // log messages.
        bar.set_mode(Some(ProgressDrawMode::Append));
    }

    bar
}

/// Open a file, creating it if it doesn't already exist. Returns the file
/// handle and whether the file existed.
fn open_or_create(options: &OpenOptions, path: &Path) -> Result<(File, bool)> {
    match options.open(path) {
        Ok(f) => Ok((f, true)),
        Err(e) => {
            let r = if e.kind() != io::ErrorKind::NotFound {
                Err(e)
            } else {
                options.clone().create(true).open(path)
            };

            Ok((r.context(format!("Could not open file: {:?}", path))?, false))
        }
    }
}

/// Delete a file, but don't error out if the path doesn't exist.
fn delete_if_exists(path: &Path) -> Result<()> {
    if let Err(e) = fs::remove_file(path) {
        if e.kind() != io::ErrorKind::NotFound {
            return Err(e).context(format!("Failed to delete file: {:?}", path));
        }
    }

    Ok(())
}

/// Add an extension to a file path.
fn add_extension(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Load FUS keys from the following list in order:
/// * User-supplied command line arguments
/// * Environment variables
/// * Config file
fn load_keys(opts: &Opts, config: &Option<Config>) -> Result<FusKeys> {
    let fixed_key = opts.fus_fixed_key
        .as_ref()
        .or(config.as_ref().and_then(|c| c.fus_fixed_key.as_ref()))
        .ok_or(anyhow!("No FUS fixed key argument or variable specified"))?
        .as_bytes();
    let flexible_key_suffix = opts.fus_flexible_key_suffix
        .as_ref()
        .or(config.as_ref().and_then(|c| c.fus_flexible_key_suffix.as_ref()))
        .ok_or(anyhow!("No FUS flexible key suffix argument or variable specified"))?
        .as_bytes();

    Ok(FusKeys::new(fixed_key, flexible_key_suffix)?)
}

#[derive(Clap, Clone, Copy, Debug)]
enum LogLevel {
    Debug,
    Trace,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Debug => f.write_str("debug"),
            Self::Trace => f.write_str("trace"),
        }
    }
}

#[derive(Debug)]
struct NumChunks(u64);

impl FromStr for NumChunks {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let n: u64 = s.parse()?;
        if n == 0 {
            return Err(anyhow!("value cannot be 0"));
        } else if n > 16 {
            // Same limit as aria2 to avoid unintentional DoS
            return Err(anyhow!("too many chunks"));
        }

        Ok(Self(n))
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    fus_fixed_key: Option<String>,
    fus_flexible_key_suffix: Option<String>,
}

fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|mut p| {
        p.push(format!("{}.conf", PKG_NAME));
        p
    })
}

fn load_config_file(user_path: Option<&Path>) -> Result<Option<Config>> {
    let default_path = default_config_path();
    let path = user_path.or(default_path.as_ref().map(|p| p.as_path()));

    match path {
        Some(p) => {
            let file = match File::open(p) {
                Ok(f) => f,
                Err(e) => {
                    return if e.kind() == io::ErrorKind::NotFound {
                        Ok(None)
                    } else {
                        Err(e).context(format!("Could not open file: {:?}", p))
                    };
                }
            };

            let config = serde_json::from_reader(file)
                .context(format!("Could not parse config file: {:?}", p))?;

            Ok(Some(config))
        }
        None => Ok(None),
    }
}

/// A simple tool for quickly downloading official firmware files from FUS.
#[derive(Clap, Debug)]
#[clap(author, version)]
struct Opts {
    /// Device's model number (eg. SM-N986U)
    #[clap(short, long)]
    model: String,
    /// Region/CSC code (eg. TMB)
    #[clap(short, long)]
    region: String,
    /// Version number (latest if unspecified)
    ///
    /// This is the version number of the firmware to download. The format is:
    /// "<PDA>/<CSC>[/<Phone>/<Data>]". If <Phone> or <Data> are omitted, then
    /// they're set to the same value as <PDA>. If no version is specified, then
    /// the latest available version is queried from the FOTA server.
    #[clap(short, long)]
    version: Option<FwVersion>,
    /// Output path for decrypted firmware
    ///
    /// By default, the output path is the filename returned by the server. This
    /// does not present a security issue because all path components are
    /// ignored.
    #[clap(short, long, parse(from_os_str))]
    output: Option<PathBuf>,
    /// Allow overwriting the output file if it exists
    ///
    /// By default, the output file is not overwritten if it already exists.
    /// Passing this option causes the check to be skipped. Note that this does
    /// not change how the intermediate (encrypted) file handled. If the
    /// intermediate file already exists, it will not be redownloaded (as usual).
    #[clap(short, long)]
    force: bool,
    /// Set logging verbosity
    ///
    /// By default, no log messages are printed out. If set to 'debug', log
    /// messages of the implementation details (such as how the parallel
    /// download ranges are split) are printed out. If set to 'trace', I/O read
    /// and write messages are also printed out, which can be extremely verbose.
    /// This option overrides the RUST_LOG environment variable, which would
    /// otherwise be respected if this option was not passed.
    #[clap(arg_enum, long)]
    loglevel: Option<LogLevel>,
    /// Number of chunks to download in parallel
    ///
    /// If a chunk downloads quickly and completes first, another in-progress
    /// chunk will be split in half, with both parts downloaded in parallel.
    /// This ensures that there's always <n> chunks downloading in parallel
    /// until the chunks are too small to be worth splitting (1MiB). This also
    /// prevents one slow connection from slowing down the entire download.
    #[clap(short, long, default_value = "4")]
    chunks: NumChunks,
    /// Maximum retries during download
    ///
    /// This only affects errors that occur during the download process. Note
    /// that the download does not immediately stop if the maximum number of
    /// retries are exceeded. New chunks (for parallel downloads) will not begin
    /// downloading, but the remaining in-progress chunks will download to
    /// completion (unless they also error out).
    #[clap(long, default_value = "3")]
    retries: u8,
    /// Keep the downloaded intermediate (encrypted) file
    ///
    /// By default, the encrypted download file is deleted if CRC32 validation
    /// and decryption succeed.
    #[clap(long)]
    keep_encrypted: bool,
    /// Ignore TLS validation for HTTPS connections
    ///
    /// By default, all HTTPS connections (eg. to FUS) will validate the TLS
    /// certificate against the system's CA trust store.
    #[clap(long)]
    ignore_tls_validation: bool,
    /// FUS fixed key
    ///
    /// If unspecified, the key is loaded from the `FUS_FIXED_KEY` environment
    /// variable, followed by the `fus_fixed_key` config file variable.
    #[clap(long, env = "FUS_FIXED_KEY")]
    fus_fixed_key: Option<String>,
    /// FUS flexible key suffix
    ///
    /// If unspecified, the key is loaded from the `FUS_FLEXIBLE_KEY_SUFFIX`
    /// environment variable, followed by the `fux_flexible_key_suffix` config
    /// file variable.
    #[clap(long, env = "FUS_FLEXIBLE_KEY_SUFFIX")]
    fus_flexible_key_suffix: Option<String>,
    /// Config file path
    ///
    /// If unspecified, the default config file path is used. The config file
    /// can store the FUS keys to avoid needing to set environment variables or
    /// pass them as command-line arguments.
    #[clap(long, parse(from_os_str))]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();

    if let Some(l) = opts.loglevel {
        std::env::set_var("RUST_LOG", format!("{}={}", PKG_NAME, l));
    }

    env_logger::init();

    debug!("Arguments: {:#?}", opts);

    let config = load_config_file(opts.config.as_ref().map(|p| p.as_path()))?;
    debug!("Config: {:#?}", config);

    let keys = load_keys(&opts, &config)?;
    debug!("Keys: {:?}", keys);

    let client_builder = FusClientBuilder::new(keys)
        .ignore_tls_validation(opts.ignore_tls_validation);

    debug!("Querying FUS for firmware information");

    let info = Arc::new(get_firmware_info(
        client_builder.clone(), &opts.model, &opts.region, opts.version).await
            .context("Failed to query firmware information")?);

    debug!("Full firmware info: {:#?}", info);

    println!("Firmware info:");
    println!("- Model: {} ({})", info.model, info.model_name);
    println!("- Region: {}", info.region);
    println!("- Version: {}", info.version);
    println!("- OS: {} {}", info.platform, info.version_name);
    println!("- File: {}{}", info.path, info.filename);
    println!("- Size: {} bytes", info.size);
    println!("- CRC32: {:08X}", info.crc);
    println!("- Date: {}", info.last_modified);

    let (default_filename, ext) = info.split_filename();
    let output_path = opts.output.unwrap_or(Path::new(&default_filename).to_owned());
    let temp_path = add_extension(&output_path, TEMP_EXT);
    let download_path = add_extension(&output_path, &ext);
    let state_path = add_extension(&download_path, STATE_EXT);

    debug!("Output path: {:?}", output_path);
    debug!("Temp path: {:?}", temp_path);
    debug!("Download path: {:?}", download_path);
    debug!("Download state path: {:?}", state_path);

    if output_path.exists() && !opts.force {
        eprintln!("{:?} already exists. Use -f/--force to overwrite.", output_path);
        return Ok(());
    }

    // Try to open the state file or split into evenly sized chunks
    let (chunks, resuming) = match DownloadState::read_file(&state_path) {
        Ok(mut s) => {
            debug!("Validating state file data: {:?}", s);

            s.remaining.sort();

            if !s.remaining.windows(2).all(|w| {
                w[0].0 <= w[0].1 && w[0].1 <= w[1].0 && w[1].0 <= w[1].1 && w[1].1 <= info.size
            }) {
                debug!("Download ranges overlap or are not increasing");

                return Err(anyhow!(
                    "State file is corrupted. Delete to download from scratch: {:?}",
                    state_path,
                ));
            }

            (s.to_ranges(), true)
        }
        Err(e) => {
            match e.downcast_ref::<io::Error>() {
                Some(e) if e.kind() == io::ErrorKind::NotFound => {
                    debug!("No existing state file found");

                    (split_range(0..info.size, opts.chunks.0, Some(MIN_CHUNK_SIZE)), false)
                }
                _ => return Err(e).context(format!(
                    "Error when opening state file: {:?}", state_path)),
            }
        }
    };

    // Try to open existing download
    let (file, existed) = open_or_create(
        OpenOptions::new().read(true).write(true), &download_path)?;

    if resuming || !existed {
        debug!("Download ranges: {:#?}", chunks);

        let remaining_chunks = download_chunks(
            client_builder.clone(),
            file.try_clone().context("Could not duplicate file handle")?,
            info.clone(),
            &chunks,
            opts.retries,
        ).await?;

        if !remaining_chunks.is_empty() {
            task::spawn_blocking(move || -> Result<()> {
                DownloadState::from_ranges(&remaining_chunks)
                    .write_file(&state_path)
            }).await??;

            return Err(anyhow!("Download was interrupted. To resume, rerun the current command."));
        }

        delete_if_exists(&state_path)?;
    }

    let decrypted_file = File::create(&temp_path)
        .context(format!("Could not open file: {:?}", temp_path))?;

    debug!("Decrypting firmware and validating CRC32");

    decrypt_firmware(file, decrypted_file, info.clone()).await?;

    if !opts.keep_encrypted {
        delete_if_exists(&download_path)?;
    }

    rename_atomic(&temp_path, &output_path)
        .context(format!("Could not move {:?} to {:?}", temp_path, output_path))?;

    Ok(())
}
