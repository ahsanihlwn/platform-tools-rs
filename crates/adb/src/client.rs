//! ADB CLI client implementation talking to the ADB Host Server on TCP 5037 (MIT).

use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use crate::auth::generate_adb_keypair;
use crate::config::{
    help_output, parse_cli_args, version_output, AdbCliConfig, AdbCommand, DeviceTarget,
};
use crate::protocol::*;
use crate::server::AdbHostServer;

fn resolve_server_addr(cfg: &AdbCliConfig) -> Result<SocketAddr, AdbError> {
    let spec = format!("{}:{}", cfg.server_host, cfg.server_port);
    spec.to_socket_addrs()?
        .next()
        .ok_or_else(|| AdbError::InvalidArgument(format!("cannot resolve '{spec}'")))
}

/// Connects to the ADB host server, spawning it in the background if not yet running.
pub fn connect_to_server(cfg: &AdbCliConfig, auto_start: bool) -> Result<TcpStream, AdbError> {
    let addr = resolve_server_addr(cfg)?;
    if let Ok(s) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        let _ = s.set_nodelay(true);
        return Ok(s);
    }
    if !auto_start {
        return Err(AdbError::Io(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "cannot connect to daemon",
        )));
    }

    spawn_background_server(cfg)?;

    for _ in 0..40 {
        thread::sleep(Duration::from_millis(100));
        if let Ok(s) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
            let _ = s.set_nodelay(true);
            return Ok(s);
        }
    }
    Err(AdbError::Io(io::Error::new(
        io::ErrorKind::TimedOut,
        "failed to start daemon",
    )))
}

#[cfg(windows)]
fn disable_std_handle_inheritance() {
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

    extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> *mut std::ffi::c_void;
        fn SetHandleInformation(
            h_object: *mut std::ffi::c_void,
            dw_mask: u32,
            dw_flags: u32,
        ) -> i32;
    }

    unsafe {
        for std_id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = GetStdHandle(std_id);
            if !h.is_null() && h != (-1isize as *mut std::ffi::c_void) {
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

fn spawn_background_server(cfg: &AdbCliConfig) -> Result<(), AdbError> {
    eprintln!(
        "* daemon not running; starting now at tcp:{}",
        cfg.server_port
    );
    #[cfg(windows)]
    disable_std_handle_inheritance();

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    if cfg.listen_all {
        cmd.arg("-a");
    }
    cmd.arg("-P")
        .arg(cfg.server_port.to_string())
        .arg("nodaemon")
        .arg("server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()?;
    eprintln!("* daemon started successfully");
    Ok(())
}

/// Sends a one-shot smart request (`host:...`) and returns its length-prefixed string response.
pub fn query_server_string(
    cfg: &AdbCliConfig,
    request: &str,
    auto_start: bool,
) -> Result<String, AdbError> {
    let mut stream = connect_to_server(cfg, auto_start)?;
    send_smart_command_sync(&mut stream, request)?;
    read_length_prefixed_string_sync(&mut stream)
}

/// Connects to the server, switches transport to the target device, and sends `service_cmd`.
pub fn open_device_service(
    cfg: &AdbCliConfig,
    target: &DeviceTarget,
    service_cmd: &str,
) -> Result<TcpStream, AdbError> {
    let mut stream = connect_to_server(cfg, true)?;
    let switch_cmd = target.transport_switch_cmd();
    send_smart_command_sync(&mut stream, &switch_cmd)?;
    send_smart_command_sync(&mut stream, service_cmd)?;
    Ok(stream)
}

fn run_device_stream_to_stdout(
    cfg: &AdbCliConfig,
    service_cmd: &str,
) -> Result<i32, AdbError> {
    let mut stream = open_device_service(cfg, &cfg.target, service_cmd)?;
    let mut out = io::stdout();
    let mut buf = [0u8; 16384];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        out.flush()?;
    }
    Ok(0)
}

fn device_supports_feature(cfg: &AdbCliConfig, feature: &str) -> bool {
    let req = cfg.target.scoped_host_cmd("features");
    if let Ok(feats) = query_server_string(cfg, &req, true) {
        feats.split(',').any(|f| f.trim() == feature)
    } else {
        false
    }
}

fn run_shell_command(
    cfg: &AdbCliConfig,
    raw_mode: bool,
    command: &[String],
) -> Result<i32, AdbError> {
    let cmd_str = command.join(" ");
    if !raw_mode && device_supports_feature(cfg, "shell_v2") {
        let service = format!("shell,v2:{cmd_str}");
        let mut stream = open_device_service(cfg, &cfg.target, &service)?;
        if command.is_empty() {
            if let Ok(mut write_stream) = stream.try_clone() {
                thread::spawn(move || {
                    let mut stdin = io::stdin();
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stdin.read(&mut buf) {
                        if n == 0 {
                            let close_pkt = encode_shell_v2_packet(SHELL_V2_CLOSE_STDIN, &[]);
                            let _ = write_stream.write_all(&close_pkt);
                            let _ = write_stream.flush();
                            break;
                        }
                        let pkt = encode_shell_v2_packet(SHELL_V2_STDIN, &buf[..n]);
                        if write_stream.write_all(&pkt).is_err() {
                            break;
                        }
                        let _ = write_stream.flush();
                    }
                });
            }
        }
        let mut stdout = io::stdout();
        let mut stderr = io::stderr();
        let mut exit_code = 0i32;

        while let Some((id, payload)) = read_shell_v2_packet_sync(&mut stream)? {
            match id {
                SHELL_V2_STDOUT => {
                    stdout.write_all(&payload)?;
                    stdout.flush()?;
                }
                SHELL_V2_STDERR => {
                    stderr.write_all(&payload)?;
                    stderr.flush()?;
                }
                SHELL_V2_EXIT => {
                    if let Some(&code) = payload.first() {
                        exit_code = i32::from(code);
                    }
                    break;
                }
                _ => {}
            }
        }
        return Ok(exit_code);
    }

    let service = format!("shell:{cmd_str}");
    run_device_stream_to_stdout(cfg, &service)
}

/// Compression mode negotiated for `sync:` file transfers (`SEND`/`RECV` v1 vs `SND2`/`RCV2` v2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncCompression {
    /// Classic `SEND` / `RECV` (`v1`, uncompressed).
    V1None,
    /// `SND2` / `RCV2` (`sendrecv_v2`) uncompressed (`SYNC_FLAG_NONE = 0`).
    V2None,
    /// `SND2` / `RCV2` (`sendrecv_v2_brotli`) via `brotli_rs` (`SYNC_FLAG_BROTLI = 1`).
    V2Brotli,
    /// `SND2` / `RCV2` (`sendrecv_v2_lz4`) via `lz4_flex::frame` (`SYNC_FLAG_LZ4 = 2`).
    V2Lz4,
    /// `SND2` / `RCV2` (`sendrecv_v2_zstd`) via `zstd` (`SYNC_FLAG_ZSTD = 4`).
    V2Zstd,
}

fn resolve_sync_compression(cfg: &AdbCliConfig, requested: Option<&str>) -> SyncCompression {
    if !device_supports_feature(cfg, "sendrecv_v2") {
        return SyncCompression::V1None;
    }
    match requested {
        Some("none") => SyncCompression::V1None,
        Some("brotli") if device_supports_feature(cfg, "sendrecv_v2_brotli") => {
            SyncCompression::V2Brotli
        }
        Some("lz4") if device_supports_feature(cfg, "sendrecv_v2_lz4") => SyncCompression::V2Lz4,
        Some("zstd") if device_supports_feature(cfg, "sendrecv_v2_zstd") => SyncCompression::V2Zstd,
        None | Some("any") => {
            if device_supports_feature(cfg, "sendrecv_v2_lz4") {
                SyncCompression::V2Lz4
            } else if device_supports_feature(cfg, "sendrecv_v2_zstd") {
                SyncCompression::V2Zstd
            } else if device_supports_feature(cfg, "sendrecv_v2_brotli") {
                SyncCompression::V2Brotli
            } else {
                SyncCompression::V1None
            }
        }
        _ => SyncCompression::V1None,
    }
}

/// Queries remote file `(mode, size, mtime)` via `STAT` (16-byte v1). Returns `(0, 0, 0)` if path does not exist.
pub fn sync_stat(
    sync_stream: &mut TcpStream,
    remote_path: &str,
) -> Result<(u32, u32, u32), AdbError> {
    let mut hdr = [0u8; 8];
    hdr[0..4].copy_from_slice(SYNC_STAT);
    hdr[4..8].copy_from_slice(&(remote_path.len() as u32).to_le_bytes());
    sync_stream.write_all(&hdr)?;
    sync_stream.write_all(remote_path.as_bytes())?;
    sync_stream.flush()?;

    let mut resp = [0u8; 16];
    sync_stream.read_exact(&mut resp)?;
    if &resp[0..4] != SYNC_STAT {
        return Err(AdbError::Protocol("invalid STAT response".into()));
    }
    let mode = u32::from_le_bytes(resp[4..8].try_into().unwrap());
    let size = u32::from_le_bytes(resp[8..12].try_into().unwrap());
    let mtime = u32::from_le_bytes(resp[12..16].try_into().unwrap());
    Ok((mode, size, mtime))
}

/// Queries remote file 64-bit `(mode, size, mtime)` via `STA2` (72-byte `stat_v2`).
pub fn sync_stat_v2(
    sync_stream: &mut TcpStream,
    remote_path: &str,
) -> Result<(u32, u64, i64), AdbError> {
    let mut hdr = [0u8; 8];
    hdr[0..4].copy_from_slice(SYNC_STA2);
    hdr[4..8].copy_from_slice(&(remote_path.len() as u32).to_le_bytes());
    sync_stream.write_all(&hdr)?;
    sync_stream.write_all(remote_path.as_bytes())?;
    sync_stream.flush()?;

    let mut resp = [0u8; 72];
    sync_stream.read_exact(&mut resp)?;
    if &resp[0..4] != SYNC_STA2 {
        return Err(AdbError::Protocol("invalid STA2 response".into()));
    }
    let err_no = u32::from_le_bytes(resp[4..8].try_into().unwrap());
    if err_no != 0 {
        return Ok((0, 0, 0));
    }
    let mode = u32::from_le_bytes(resp[24..28].try_into().unwrap());
    let size = u64::from_le_bytes(resp[40..48].try_into().unwrap());
    let mtime = i64::from_le_bytes(resp[56..64].try_into().unwrap());
    Ok((mode, size, mtime))
}

/// Lists remote directory entries via `LIST` -> `DENT`* -> `DONE` (20-byte v1).
pub fn sync_list(
    sync_stream: &mut TcpStream,
    remote_dir: &str,
) -> Result<Vec<SyncDent>, AdbError> {
    let mut hdr = [0u8; 8];
    hdr[0..4].copy_from_slice(SYNC_LIST);
    hdr[4..8].copy_from_slice(&(remote_dir.len() as u32).to_le_bytes());
    sync_stream.write_all(&hdr)?;
    sync_stream.write_all(remote_dir.as_bytes())?;
    sync_stream.flush()?;

    let mut entries = Vec::new();
    loop {
        let mut dhdr = [0u8; 20];
        sync_stream.read_exact(&mut dhdr)?;
        let id = &dhdr[0..4];
        if id == SYNC_DONE {
            break;
        }
        if id != SYNC_DENT {
            return Err(AdbError::Protocol(format!(
                "unexpected sync LIST response '{:?}'",
                id
            )));
        }
        let mode = u32::from_le_bytes(dhdr[4..8].try_into().unwrap());
        let size = u64::from(u32::from_le_bytes(dhdr[8..12].try_into().unwrap()));
        let mtime = i64::from(u32::from_le_bytes(dhdr[12..16].try_into().unwrap()));
        let namelen = u32::from_le_bytes(dhdr[16..20].try_into().unwrap()) as usize;
        let mut name_buf = vec![0u8; namelen];
        if namelen > 0 {
            sync_stream.read_exact(&mut name_buf)?;
        }
        let name = String::from_utf8_lossy(&name_buf).into_owned();
        if name != "." && name != ".." {
            entries.push(SyncDent {
                mode,
                size,
                mtime,
                name,
            });
        }
    }
    Ok(entries)
}

/// Lists remote directory entries via `LST2` -> `DNT2`* -> `DONE` (76-byte `ls_v2`).
pub fn sync_list_v2(
    sync_stream: &mut TcpStream,
    remote_dir: &str,
) -> Result<Vec<SyncDent>, AdbError> {
    let mut hdr = [0u8; 8];
    hdr[0..4].copy_from_slice(SYNC_LST2);
    hdr[4..8].copy_from_slice(&(remote_dir.len() as u32).to_le_bytes());
    sync_stream.write_all(&hdr)?;
    sync_stream.write_all(remote_dir.as_bytes())?;
    sync_stream.flush()?;

    let mut entries = Vec::new();
    loop {
        let mut dhdr = [0u8; 76];
        sync_stream.read_exact(&mut dhdr)?;
        let id = &dhdr[0..4];
        if id == SYNC_DONE {
            break;
        }
        if id != SYNC_DNT2 {
            return Err(AdbError::Protocol(format!(
                "unexpected sync LST2 response '{:?}'",
                id
            )));
        }
        let mode = u32::from_le_bytes(dhdr[24..28].try_into().unwrap());
        let size = u64::from_le_bytes(dhdr[40..48].try_into().unwrap());
        let mtime = i64::from_le_bytes(dhdr[56..64].try_into().unwrap());
        let namelen = u32::from_le_bytes(dhdr[72..76].try_into().unwrap()) as usize;
        let mut name_buf = vec![0u8; namelen];
        if namelen > 0 {
            sync_stream.read_exact(&mut name_buf)?;
        }
        let name = String::from_utf8_lossy(&name_buf).into_owned();
        if name != "." && name != ".." {
            entries.push(SyncDent {
                mode,
                size,
                mtime,
                name,
            });
        }
    }
    Ok(entries)
}

/// Sends a file over an open `sync:` stream using `SEND` (v1) or `SND2` (`sendrecv_v2` with LZ4, Brotli, or Zstd compression).
fn sync_send_one_file(
    sync_stream: &mut TcpStream,
    local_path: &Path,
    remote_path: &str,
    dry_run: bool,
    comp: SyncCompression,
) -> Result<u64, AdbError> {
    let meta = fs::metadata(local_path)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);
    let content = fs::read(local_path)?;
    let total_bytes = content.len() as u64;

    if dry_run && comp == SyncCompression::V1None {
        return Ok(total_bytes);
    }

    if comp != SyncCompression::V1None {
        let mut hdr1 = [0u8; 8];
        hdr1[0..4].copy_from_slice(SYNC_SND2);
        hdr1[4..8].copy_from_slice(&(remote_path.len() as u32).to_le_bytes());
        sync_stream.write_all(&hdr1)?;
        sync_stream.write_all(remote_path.as_bytes())?;

        let mut flags = match comp {
            SyncCompression::V2Brotli => SYNC_FLAG_BROTLI,
            SyncCompression::V2Lz4 => SYNC_FLAG_LZ4,
            SyncCompression::V2Zstd => SYNC_FLAG_ZSTD,
            _ => SYNC_FLAG_NONE,
        };
        if dry_run {
            flags |= SYNC_FLAG_DRY_RUN;
        }
        let mut hdr2 = [0u8; 12];
        hdr2[0..4].copy_from_slice(SYNC_SND2);
        hdr2[4..8].copy_from_slice(&33188u32.to_le_bytes()); // 0o100644
        hdr2[8..12].copy_from_slice(&flags.to_le_bytes());
        sync_stream.write_all(&hdr2)?;

        let payload_stream = match comp {
            SyncCompression::V2Brotli => brotli_rs::io::compress(
                &content,
                1,
                22,
                brotli_rs::enc::params::BrotliEncoderMode::Generic,
            )
            .map_err(|e| AdbError::Protocol(format!("Brotli encode error: {e}")))?,
            SyncCompression::V2Lz4 => {
                let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
                enc.write_all(&content)?;
                enc.finish()
                    .map_err(|e| AdbError::Protocol(format!("LZ4 frame encode error: {e}")))?
            }
            SyncCompression::V2Zstd => zstd::stream::encode_all(content.as_slice(), 1)
                .map_err(|e| AdbError::Protocol(format!("Zstd encode error: {e}")))?,
            _ => content,
        };

        for chunk in payload_stream.chunks(SYNC_DATA_MAX) {
            let mut dhdr = [0u8; 8];
            dhdr[0..4].copy_from_slice(SYNC_DATA);
            dhdr[4..8].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
            sync_stream.write_all(&dhdr)?;
            sync_stream.write_all(chunk)?;
        }
    } else {
        let spec = format!("{remote_path},33188"); // 0o100644 = 33188
        let mut hdr = [0u8; 8];
        hdr[0..4].copy_from_slice(SYNC_SEND);
        hdr[4..8].copy_from_slice(&(spec.len() as u32).to_le_bytes());
        sync_stream.write_all(&hdr)?;
        sync_stream.write_all(spec.as_bytes())?;

        for chunk in content.chunks(SYNC_DATA_MAX) {
            let mut dhdr = [0u8; 8];
            dhdr[0..4].copy_from_slice(SYNC_DATA);
            dhdr[4..8].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
            sync_stream.write_all(&dhdr)?;
            sync_stream.write_all(chunk)?;
        }
    }

    let mut done_hdr = [0u8; 8];
    done_hdr[0..4].copy_from_slice(SYNC_DONE);
    done_hdr[4..8].copy_from_slice(&mtime.to_le_bytes());
    sync_stream.write_all(&done_hdr)?;
    sync_stream.flush()?;

    let mut resp = [0u8; 8];
    sync_stream.read_exact(&mut resp)?;
    let id = &resp[0..4];
    let msg_len = u32::from_le_bytes(resp[4..8].try_into().unwrap()) as usize;
    if id == SYNC_OKAY {
        if msg_len > 0 {
            let mut skip = vec![0u8; msg_len];
            sync_stream.read_exact(&mut skip)?;
        }
        Ok(total_bytes)
    } else if id == SYNC_FAIL {
        let mut err_buf = vec![0u8; msg_len];
        if msg_len > 0 {
            sync_stream.read_exact(&mut err_buf)?;
        }
        Err(AdbError::RemoteFail(
            String::from_utf8_lossy(&err_buf).into_owned(),
        ))
    } else {
        Err(AdbError::Protocol(format!(
            "unexpected sync SEND response '{:?}'",
            id
        )))
    }
}

/// Recursively pushes a local file or directory tree over an open `sync:` stream.
fn sync_push_recursive(
    sync_stream: &mut TcpStream,
    local_path: &Path,
    remote_path: &str,
    dry_run: bool,
    comp: SyncCompression,
) -> Result<(u64, usize), AdbError> {
    let meta = fs::metadata(local_path)?;
    if meta.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(local_path)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        let mut total_bytes = 0u64;
        let mut total_files = 0usize;
        let base_remote = remote_path.trim_end_matches('/');
        for entry in entries {
            let child_name = entry.file_name().to_string_lossy().into_owned();
            let child_remote = format!("{base_remote}/{child_name}");
            let (b, c) = sync_push_recursive(
                sync_stream,
                &entry.path(),
                &child_remote,
                dry_run,
                comp,
            )?;
            total_bytes += b;
            total_files += c;
        }
        Ok((total_bytes, total_files))
    } else {
        let b = sync_send_one_file(sync_stream, local_path, remote_path, dry_run, comp)?;
        Ok((b, 1))
    }
}

/// Receives a single file over an open `sync:` stream using `RECV` (v1) or `RCV2` (`sendrecv_v2` with LZ4, Brotli, or Zstd decompression).
fn sync_recv_one_file(
    sync_stream: &mut TcpStream,
    remote_path: &str,
    local_path: &Path,
    comp: SyncCompression,
) -> Result<u64, AdbError> {
    if comp != SyncCompression::V1None {
        let mut hdr1 = [0u8; 8];
        hdr1[0..4].copy_from_slice(SYNC_RCV2);
        hdr1[4..8].copy_from_slice(&(remote_path.len() as u32).to_le_bytes());
        sync_stream.write_all(&hdr1)?;
        sync_stream.write_all(remote_path.as_bytes())?;

        let flags = match comp {
            SyncCompression::V2Brotli => SYNC_FLAG_BROTLI,
            SyncCompression::V2Lz4 => SYNC_FLAG_LZ4,
            SyncCompression::V2Zstd => SYNC_FLAG_ZSTD,
            _ => SYNC_FLAG_NONE,
        };
        let mut hdr2 = [0u8; 8];
        hdr2[0..4].copy_from_slice(SYNC_RCV2);
        hdr2[4..8].copy_from_slice(&flags.to_le_bytes());
        sync_stream.write_all(&hdr2)?;
        sync_stream.flush()?;
    } else {
        let mut hdr = [0u8; 8];
        hdr[0..4].copy_from_slice(SYNC_RECV);
        hdr[4..8].copy_from_slice(&(remote_path.len() as u32).to_le_bytes());
        sync_stream.write_all(&hdr)?;
        sync_stream.write_all(remote_path.as_bytes())?;
        sync_stream.flush()?;
    }

    if let Some(parent) = local_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut raw_stream = Vec::new();
    loop {
        let mut chdr = [0u8; 8];
        sync_stream.read_exact(&mut chdr)?;
        let id = &chdr[0..4];
        let len = u32::from_le_bytes(chdr[4..8].try_into().unwrap()) as usize;
        if id == SYNC_DATA {
            let mut chunk = vec![0u8; len];
            sync_stream.read_exact(&mut chunk)?;
            raw_stream.extend_from_slice(&chunk);
        } else if id == SYNC_DONE {
            break;
        } else if id == SYNC_FAIL {
            let mut err_buf = vec![0u8; len];
            if len > 0 {
                sync_stream.read_exact(&mut err_buf)?;
            }
            return Err(AdbError::RemoteFail(
                String::from_utf8_lossy(&err_buf).into_owned(),
            ));
        } else {
            return Err(AdbError::Protocol(format!(
                "unexpected sync RECV chunk '{:?}'",
                id
            )));
        }
    }
    let out = match comp {
        SyncCompression::V2Brotli => brotli_rs::io::decompress(&raw_stream)
            .map_err(|e| AdbError::Protocol(format!("Brotli decompress error: {e}")))?,
        SyncCompression::V2Lz4 => {
            let mut dec = lz4_flex::frame::FrameDecoder::new(raw_stream.as_slice());
            let mut decompressed = Vec::new();
            dec.read_to_end(&mut decompressed)
                .map_err(|e| AdbError::Protocol(format!("LZ4 frame decompress error: {e}")))?;
            decompressed
        }
        SyncCompression::V2Zstd => zstd::stream::decode_all(raw_stream.as_slice())
            .map_err(|e| AdbError::Protocol(format!("Zstd decompress error: {e}")))?,
        _ => raw_stream,
    };
    let n = out.len() as u64;
    fs::write(local_path, &out)?;
    Ok(n)
}

/// Recursively pulls a remote file or directory tree over an open `sync:` stream.
fn sync_pull_recursive(
    sync_stream: &mut TcpStream,
    remote_path: &str,
    local_path: &Path,
    comp: SyncCompression,
    use_stat_v2: bool,
    use_ls_v2: bool,
) -> Result<(u64, usize), AdbError> {
    let mode_opt = if use_stat_v2 {
        sync_stat_v2(sync_stream, remote_path).ok().map(|(m, _, _)| m)
    } else {
        sync_stat(sync_stream, remote_path).ok().map(|(m, _, _)| m)
    };
    if let Some(mode) = mode_opt {
        if (mode & 0o170000) == 0o040000 {
            fs::create_dir_all(local_path)?;
            let dents = if use_ls_v2 {
                sync_list_v2(sync_stream, remote_path)?
            } else {
                sync_list(sync_stream, remote_path)?
            };
            let mut total_bytes = 0u64;
            let mut total_files = 0usize;
            let base_remote = remote_path.trim_end_matches('/');
            for d in dents {
                let child_remote = format!("{base_remote}/{}", d.name);
                let child_local = local_path.join(&d.name);
                if d.is_dir() {
                    let (b, c) = sync_pull_recursive(
                        sync_stream,
                        &child_remote,
                        &child_local,
                        comp,
                        use_stat_v2,
                        use_ls_v2,
                    )?;
                    total_bytes += b;
                    total_files += c;
                } else {
                    let b = sync_recv_one_file(sync_stream, &child_remote, &child_local, comp)?;
                    total_bytes += b;
                    total_files += 1;
                }
            }
            return Ok((total_bytes, total_files));
        }
    }

    let b = sync_recv_one_file(sync_stream, remote_path, local_path, comp)?;
    Ok((b, 1))
}

fn sync_quit(sync_stream: &mut TcpStream) {
    let mut q = [0u8; 8];
    q[0..4].copy_from_slice(SYNC_QUIT);
    let _ = sync_stream.write_all(&q);
    let _ = sync_stream.flush();
}

/// Top-level CLI runner for `adb`. Returns process exit code.
pub async fn run_cli(args: &[String]) -> i32 {
    let cfg = match parse_cli_args(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("adb: {e}");
            return 1;
        }
    };

    match execute_command(&cfg).await {
        Ok(code) => code,
        Err(e) => {
            let s = e.to_string();
            if s.starts_with("adb:") {
                eprintln!("{s}");
            } else {
                eprintln!("adb: error: {s}");
            }
            1
        }
    }
}

async fn execute_command(cfg: &AdbCliConfig) -> Result<i32, AdbError> {
    match &cfg.command {
        AdbCommand::Version => {
            print!("{}", version_output());
            Ok(0)
        }
        AdbCommand::Help { exit_code } => {
            print!("{}", help_output());
            Ok(*exit_code)
        }
        AdbCommand::RunServerForeground => {
            let bind_ip = if cfg.listen_all {
                "0.0.0.0"
            } else {
                &cfg.server_host
            };
            let addr: SocketAddr = format!("{bind_ip}:{}", cfg.server_port)
                .parse()
                .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], cfg.server_port)));
            AdbHostServer::run_server(addr).await?;
            Ok(0)
        }
        AdbCommand::StartServer => {
            let _ = connect_to_server(cfg, true)?;
            Ok(0)
        }
        AdbCommand::KillServer => {
            if let Ok(mut stream) = connect_to_server(cfg, false) {
                let _ = send_smart_command_sync(&mut stream, "host:kill");
            }
            Ok(0)
        }
        AdbCommand::Devices { long } => {
            let cmd = if *long {
                "host:devices-l"
            } else {
                "host:devices"
            };
            let list = query_server_string(cfg, cmd, true)?;
            println!("List of devices attached");
            print!("{list}");
            if !list.ends_with('\n') && !list.is_empty() {
                println!();
            }
            println!();
            Ok(0)
        }
        AdbCommand::Connect(addr) => {
            let resp = query_server_string(cfg, &format!("host:connect:{addr}"), true)?;
            println!("{resp}");
            Ok(0)
        }
        AdbCommand::Disconnect(addr_opt) => {
            let spec = addr_opt.as_deref().unwrap_or("");
            let resp = query_server_string(cfg, &format!("host:disconnect:{spec}"), true)?;
            println!("{resp}");
            Ok(0)
        }
        AdbCommand::Pair { addr, code } => {
            let pw = code.as_deref().unwrap_or("000000");
            let resp = query_server_string(cfg, &format!("host:pair:{pw}:{addr}"), true)?;
            println!("{resp}");
            Ok(0)
        }
        AdbCommand::MdnsCheck => {
            let resp = query_server_string(cfg, "host:mdns:check", true)?;
            println!("{resp}");
            Ok(0)
        }
        AdbCommand::MdnsServices => {
            let resp = query_server_string(cfg, "host:mdns:services", true)?;
            println!("List of discovered mdns services");
            print!("{resp}");
            Ok(0)
        }
        AdbCommand::ForwardList => {
            let resp = query_server_string(cfg, "host:list-forward", true)?;
            print!("{resp}");
            Ok(0)
        }
        AdbCommand::Forward {
            no_rebind,
            local,
            remote,
        } => {
            let nr = if *no_rebind { "norebind:" } else { "" };
            let sub = format!("forward:{nr}{local};{remote}");
            let req = cfg.target.scoped_host_cmd(&sub);
            let mut stream = connect_to_server(cfg, true)?;
            send_smart_command_sync(&mut stream, &req)?;
            read_status_sync(&mut stream)?;
            if local == "tcp:0" {
                if let Ok(port_str) = read_length_prefixed_string_sync(&mut stream) {
                    println!("{port_str}");
                }
            }
            Ok(0)
        }
        AdbCommand::ForwardRemove(local) => {
            let req = cfg.target.scoped_host_cmd(&format!("killforward:{local}"));
            let mut stream = connect_to_server(cfg, true)?;
            send_smart_command_sync(&mut stream, &req)?;
            read_status_sync(&mut stream)?;
            Ok(0)
        }
        AdbCommand::ForwardRemoveAll => {
            let mut stream = connect_to_server(cfg, true)?;
            send_smart_command_sync(&mut stream, "host:killforward-all")?;
            read_status_sync(&mut stream)?;
            Ok(0)
        }
        AdbCommand::ReverseList => {
            let mut stream = open_device_service(cfg, &cfg.target, "reverse:list-forward")?;
            let list = read_length_prefixed_string_sync(&mut stream)?;
            print!("{list}");
            Ok(0)
        }
        AdbCommand::Reverse {
            no_rebind,
            remote,
            local,
        } => {
            let nr = if *no_rebind { "norebind:" } else { "" };
            let svc = format!("reverse:forward:{nr}{remote};{local}");
            let mut stream = open_device_service(cfg, &cfg.target, &svc)?;
            read_status_sync(&mut stream)?;
            Ok(0)
        }
        AdbCommand::ReverseRemove(remote) => {
            let svc = format!("reverse:killforward:{remote}");
            let mut stream = open_device_service(cfg, &cfg.target, &svc)?;
            read_status_sync(&mut stream)?;
            Ok(0)
        }
        AdbCommand::ReverseRemoveAll => {
            let mut stream = open_device_service(cfg, &cfg.target, "reverse:killforward-all")?;
            read_status_sync(&mut stream)?;
            Ok(0)
        }
        AdbCommand::Push {
            sources,
            remote,
            dry_run,
            quiet,
            compression,
        } => {
            let comp = resolve_sync_compression(cfg, compression.as_deref());
            let mut sync_stream = open_device_service(cfg, &cfg.target, "sync:")?;
            let mut total = 0u64;
            let mut count = 0usize;
            for src in sources {
                let target_remote = if sources.len() > 1 || remote.ends_with('/') {
                    let fname = src
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "file".to_string());
                    format!("{}/{}", remote.trim_end_matches('/'), fname)
                } else {
                    remote.clone()
                };
                let (b, c) = sync_push_recursive(
                    &mut sync_stream,
                    src,
                    &target_remote,
                    *dry_run,
                    comp,
                )?;
                total += b;
                count += c;
            }
            sync_quit(&mut sync_stream);
            if !*quiet {
                let first_name = sources[0].display();
                eprintln!("{first_name}: {count} file pushed, 0 skipped. ({total} bytes)");
            }
            Ok(0)
        }
        AdbCommand::Pull {
            remotes,
            local,
            preserve_attrs: _,
            quiet,
            compression,
        } => {
            let comp = resolve_sync_compression(cfg, compression.as_deref());
            let use_stat_v2 = device_supports_feature(cfg, "stat_v2");
            let use_ls_v2 = device_supports_feature(cfg, "ls_v2");
            let mut sync_stream = open_device_service(cfg, &cfg.target, "sync:")?;
            let mut total = 0u64;
            let mut count = 0usize;
            for r in remotes {
                let dest = if remotes.len() > 1 || local.is_dir() {
                    let fname = r.rsplit('/').next().unwrap_or("file");
                    local.join(fname)
                } else {
                    local.clone()
                };
                let (b, c) = sync_pull_recursive(
                    &mut sync_stream,
                    r,
                    &dest,
                    comp,
                    use_stat_v2,
                    use_ls_v2,
                )?;
                total += b;
                count += c;
            }
            sync_quit(&mut sync_stream);
            if !*quiet {
                eprintln!(
                    "{}: {count} file pulled, 0 skipped. ({total} bytes)",
                    remotes[0]
                );
            }
            Ok(0)
        }
        AdbCommand::Sync {
            partition: _,
            list_only: _,
            dry_run: _,
            quiet: _,
        } => {
            let _out_dir = std::env::var("ANDROID_PRODUCT_OUT").map_err(|_| {
                AdbError::InvalidArgument("ANDROID_PRODUCT_OUT not set".into())
            })?;
            Ok(0)
        }
        AdbCommand::Shell { raw_mode, command } => run_shell_command(cfg, *raw_mode, command),
        AdbCommand::Emu(cmd_tokens) => {
            let joined = cmd_tokens.join(" ");
            run_device_stream_to_stdout(cfg, &format!("emu:{joined}"))
        }
        AdbCommand::Install { flags, apk } => {
            let fname = apk
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "base.apk".to_string());
            let remote_tmp = format!("/data/local/tmp/{fname}");
            {
                let comp = resolve_sync_compression(cfg, None);
                let mut sync_stream = open_device_service(cfg, &cfg.target, "sync:")?;
                let _ = sync_send_one_file(&mut sync_stream, apk, &remote_tmp, false, comp)?;
                sync_quit(&mut sync_stream);
            }
            println!("Performing Push Install");
            let flags_str = flags.join(" ");
            let pm_cmd = if flags_str.is_empty() {
                format!("pm install \"{remote_tmp}\"")
            } else {
                format!("pm install {flags_str} \"{remote_tmp}\"")
            };
            run_shell_command(cfg, false, &[pm_cmd])
        }
        AdbCommand::InstallMultiple { flags, apks }
        | AdbCommand::InstallMultiPackage { flags, apks } => {
            let total_size: u64 = apks
                .iter()
                .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
                .sum();
            let flags_str = flags.join(" ");
            let create_cmd = format!("pm install-create -S {total_size} {flags_str}");
            run_shell_command(cfg, false, &[create_cmd])
        }
        AdbCommand::Uninstall { keep_data, package } => {
            let cmd = if *keep_data {
                format!("pm uninstall -k {package}")
            } else {
                format!("pm uninstall {package}")
            };
            run_shell_command(cfg, false, &[cmd])
        }
        AdbCommand::Bugreport(path_opt) => {
            let _ = path_opt;
            run_shell_command(cfg, false, &["bugreportz".to_string()])
        }
        AdbCommand::Jdwp => run_device_stream_to_stdout(cfg, "jdwp"),
        AdbCommand::Logcat(log_args) => {
            let mut cmd = vec!["logcat".to_string()];
            cmd.extend_from_slice(log_args);
            run_shell_command(cfg, false, &cmd)
        }
        AdbCommand::DisableVerity => run_device_stream_to_stdout(cfg, "disable-verity:"),
        AdbCommand::EnableVerity => run_device_stream_to_stdout(cfg, "enable-verity:"),
        AdbCommand::Keygen(path) => {
            generate_adb_keypair(path)?;
            Ok(0)
        }
        AdbCommand::WaitFor {
            transport: _,
            state,
        } => {
            for _ in 0..100 {
                let req = cfg.target.scoped_host_cmd("get-state");
                if let Ok(cur_state) = query_server_string(cfg, &req, true) {
                    if cur_state.trim() == state {
                        return Ok(0);
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(AdbError::RemoteFail(format!(
                "timed out waiting for '{state}'"
            )))
        }
        AdbCommand::GetState => {
            let req = cfg.target.scoped_host_cmd("get-state");
            let s = query_server_string(cfg, &req, true)?;
            println!("{s}");
            Ok(0)
        }
        AdbCommand::GetSerialNo => {
            let req = cfg.target.scoped_host_cmd("get-serialno");
            let s = query_server_string(cfg, &req, true)?;
            println!("{s}");
            Ok(0)
        }
        AdbCommand::GetDevPath => {
            let req = cfg.target.scoped_host_cmd("get-devpath");
            let s = query_server_string(cfg, &req, true)?;
            println!("{s}");
            Ok(0)
        }
        AdbCommand::Remount { auto_reboot } => {
            let svc = if *auto_reboot {
                "remount:-R"
            } else {
                "remount:"
            };
            run_device_stream_to_stdout(cfg, svc)
        }
        AdbCommand::Reboot(target_opt) => {
            let svc = format!("reboot:{}", target_opt.as_deref().unwrap_or(""));
            let _ = open_device_service(cfg, &cfg.target, &svc)?;
            Ok(0)
        }
        AdbCommand::Sideload(pkg_path) => {
            let data = fs::read(pkg_path)?;
            let svc = format!("sideload:{}:65536", data.len());
            let mut stream = open_device_service(cfg, &cfg.target, &svc)?;
            stream.write_all(&data)?;
            Ok(0)
        }
        AdbCommand::Root => run_device_stream_to_stdout(cfg, "root:"),
        AdbCommand::Unroot => run_device_stream_to_stdout(cfg, "unroot:"),
        AdbCommand::Usb => run_device_stream_to_stdout(cfg, "usb:"),
        AdbCommand::Tcpip(port) => run_device_stream_to_stdout(cfg, &format!("tcpip:{port}")),
        AdbCommand::Reconnect(sub_opt) => {
            let _ = sub_opt;
            let req = cfg.target.scoped_host_cmd("reconnect");
            let s = query_server_string(cfg, &req, true)?;
            println!("{s}");
            Ok(0)
        }
        AdbCommand::Attach | AdbCommand::Detach => Ok(0),
        AdbCommand::Features => {
            let req = cfg.target.scoped_host_cmd("features");
            let s = query_server_string(cfg, &req, true)?;
            println!("{s}");
            Ok(0)
        }
        AdbCommand::HostFeatures => {
            let s = query_server_string(cfg, "host:host-features", true)?;
            println!("{s}");
            Ok(0)
        }
    }
}
