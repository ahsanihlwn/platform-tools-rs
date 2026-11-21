//! Execution of Fastboot CLI tasks against a [`AnyFastbootTransport`] (MIT).

use std::fs;
use std::io::{Cursor, Read};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use android_avb::{is_vbmeta_partition, patch_vbmeta_flags};
use android_bootimg::{build_boot_image, repack_vendor_boot_image};
use android_lp::build_empty_super_metadata;
use android_sparse::{
    is_sparse_image, split_raw, split_sparse, ParsedChunk, SparseReader, SplitPart,
};
use extfs_mkfs::{format_extfs, ExtMkfsConfig};
use f2fs_mkfs::spec::{
    F2FS_ENC_UTF8_12_1, F2FS_FEATURE_CASEFOLD, F2FS_FEATURE_COMPRESSION, F2FS_FEATURE_ENCRYPT,
    F2FS_FEATURE_EXTRA_ATTR, F2FS_FEATURE_PRJQUOTA, F2FS_FEATURE_QUOTA_INO, F2FS_FEATURE_VERITY,
};
use f2fs_mkfs::{format_f2fs, MkfsConfig as F2fsMkfsConfig};

use crate::config::{
    help_output, parse_cli_args, parse_fastboot_info, version_output, FastbootCliAction,
    FastbootInfoStep, FastbootSessionConfig, FastbootTask,
};
use crate::protocol::{parse_u64_auto, FastbootError};
use crate::transport::{list_usb_devices, AnyFastbootTransport};

fn serialize_split_part(part: &SplitPart, src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(part.sparse_size() as usize);
    out.extend_from_slice(&part.header.to_bytes());
    for c in &part.chunks {
        out.extend_from_slice(&c.header.to_bytes());
        if c.data_size > 0 {
            let start = c.input_offset as usize;
            let end = (start + c.data_size as usize).min(src.len());
            out.extend_from_slice(&src[start..end]);
            let pad = c.data_size as usize - (end - start);
            if pad > 0 {
                out.resize(out.len() + pad, 0);
            }
        }
    }
    out
}

fn print_status(action: &str, dur: Duration) {
    eprintln!("{:<50} OKAY [{:7.3}s]", action, dur.as_secs_f64());
}

/// Resolves target partition name(s) accounting for `has-slot:<part>`, `current-slot`, and `--slot`.
async fn resolve_target_partitions(
    transport: &mut AnyFastbootTransport,
    raw_part: &str,
    slot_override: Option<&str>,
) -> Result<Vec<String>, FastbootError> {
    if raw_part.ends_with("_a") || raw_part.ends_with("_b") {
        return Ok(vec![raw_part.to_string()]);
    }
    let has_slot = transport
        .get_var_optional(&format!("has-slot:{raw_part}"))
        .await?
        .unwrap_or_default();
    if has_slot != "yes" {
        return Ok(vec![raw_part.to_string()]);
    }

    if slot_override == Some("all") {
        return Ok(vec![format!("{raw_part}_a"), format!("{raw_part}_b")]);
    }

    let cur_slot_raw = transport
        .get_var_optional("current-slot")
        .await?
        .unwrap_or_else(|| "a".to_string());
    let cur_slot = cur_slot_raw.trim_start_matches('_');
    let cur_slot = if cur_slot.is_empty() { "a" } else { cur_slot };

    let chosen_slot = match slot_override {
        Some("other") => {
            if cur_slot == "a" {
                "b"
            } else {
                "a"
            }
        }
        Some(s) => s.trim_start_matches('_'),
        None => cur_slot,
    };

    Ok(vec![format!("{raw_part}_{chosen_slot}")])
}

async fn get_max_download_size(
    transport: &mut AnyFastbootTransport,
    cfg: &FastbootSessionConfig,
) -> Result<u32, FastbootError> {
    if let Some(lim) = cfg.sparse_limit {
        return Ok(lim.min(u64::from(u32::MAX)) as u32);
    }
    if let Some(val) = transport.get_var_optional("max-download-size").await? {
        if let Ok(parsed) = parse_u64_auto(&val) {
            if parsed > 0 {
                return Ok(parsed.min(u64::from(u32::MAX)) as u32);
            }
        }
    }
    Ok(256 * 1024 * 1024)
}

/// Flashes an in-memory image buffer to `part`, automatically patching `vbmeta` flags if requested
/// and splitting oversized raw or sparse images into `max_download_size` sparse chunks.
async fn flash_buffer_to_partition(
    transport: &mut AnyFastbootTransport,
    cfg: &FastbootSessionConfig,
    part: &str,
    mut data: Vec<u8>,
) -> Result<(), FastbootError> {
    if is_vbmeta_partition(part) && (cfg.disable_verity || cfg.disable_verification) {
        let _ = patch_vbmeta_flags(&mut data, cfg.disable_verity, cfg.disable_verification)?;
    }

    let max_dl = get_max_download_size(transport, cfg).await?;
    let mut cursor = Cursor::new(&data);
    let is_sp = is_sparse_image(&mut cursor)?;

    if data.len() as u64 > u64::from(max_dl) {
        let parts: Vec<SplitPart> = if is_sp {
            let mut sr = SparseReader::new(Cursor::new(&data), false)?;
            let hdr = sr.header().clone();
            let mut ch_hdrs = Vec::new();
            while let Some(c) = sr.next_chunk()? {
                match c {
                    ParsedChunk::Raw { header, .. } => {
                        sr.skip_raw_data()?;
                        ch_hdrs.push(header);
                    }
                    ParsedChunk::Fill { header, .. }
                    | ParsedChunk::DontCare { header }
                    | ParsedChunk::Crc32 { header, .. } => ch_hdrs.push(header),
                }
            }
            split_sparse(&hdr, &ch_hdrs, u64::from(max_dl))?
        } else {
            split_raw(
                data.len() as u64,
                NonZeroU32::new(4096).unwrap(),
                u64::from(max_dl),
            )?
        };
        let total = parts.len();
        for (idx, part_item) in parts.into_iter().enumerate() {
            let chunk_bytes = serialize_split_part(&part_item, &data);
            let dl_dur = transport.download_data(&chunk_bytes).await?;
            print_status(
                &format!(
                    "Sending sparse '{}' {}/{} ({} KB)",
                    part,
                    idx + 1,
                    total,
                    chunk_bytes.len() / 1024
                ),
                dl_dur,
            );
            let (_, fl_dur) = transport.execute_command(&format!("flash:{part}")).await?;
            print_status(&format!("Writing '{part}'"), fl_dur);
        }
    } else {
        let dl_dur = transport.download_data(&data).await?;
        print_status(
            &format!("Sending '{}' ({} KB)", part, (data.len().max(1024)) / 1024),
            dl_dur,
        );
        let (_, fl_dur) = transport.execute_command(&format!("flash:{part}")).await?;
        print_status(&format!("Writing '{part}'"), fl_dur);
    }
    Ok(())
}

async fn fetch_partition_bytes(
    transport: &mut AnyFastbootTransport,
    part: &str,
    offset: Option<u64>,
    size: Option<u64>,
) -> Result<(Vec<u8>, Duration), FastbootError> {
    let cmd = match (offset, size) {
        (Some(off), Some(sz)) => format!("fetch:{part}:0x{off:08x}:0x{sz:08x}"),
        (Some(off), None) => format!("fetch:{part}:0x{off:08x}"),
        _ => format!("fetch:{part}"),
    };
    transport.upload_data(&cmd).await
}

async fn execute_format_partition(
    transport: &mut AnyFastbootTransport,
    cfg: &FastbootSessionConfig,
    raw_part: &str,
    fs_type_override: Option<&str>,
    size_override: Option<u64>,
) -> Result<(), FastbootError> {
    let targets =
        resolve_target_partitions(transport, raw_part, cfg.slot_override.as_deref()).await?;
    for part in targets {
        let fs_type = match fs_type_override {
            Some(t) => t.to_string(),
            None => transport
                .get_var_optional(&format!("partition-type:{part}"))
                .await?
                .unwrap_or_else(|| "ext4".to_string()),
        };
        let part_size = match size_override {
            Some(s) => s,
            None => {
                let s_str = transport
                    .get_var_optional(&format!("partition-size:{part}"))
                    .await?
                    .unwrap_or_else(|| "0x04000000".to_string());
                parse_u64_auto(&s_str).unwrap_or(64 * 1024 * 1024)
            }
        };

        let (_, er_dur) = transport.execute_command(&format!("erase:{part}")).await?;
        print_status(&format!("Erasing '{part}'"), er_dur);

        if fs_type == "raw" || part_size == 0 {
            continue;
        }

        let tmp_dir = tempfile::tempdir()?;
        let sp_img_path = tmp_dir.path().join("formatted_sparse.img");

        let has_casefold = cfg.fs_options.iter().any(|o| o == "casefold");
        let has_projid = cfg.fs_options.iter().any(|o| o == "projid");
        let has_compress = cfg.fs_options.iter().any(|o| o == "compress");

        match fs_type.as_str() {
            "ext4" | "ext3" | "ext2" => {
                let mut ecfg = ExtMkfsConfig {
                    device_path: sp_img_path.clone(),
                    blocks_count_arg: Some(part_size / 4096),
                    block_size: 4096,
                    block_size_explicit: true,
                    inode_size: 256,
                    inode_size_explicit: true,
                    android_sparse: true,
                    quiet: true,
                    force: true,
                    fs_type: fs_type.clone(),
                    ..ExtMkfsConfig::default()
                };
                extfs_mkfs::config::apply_fs_type_profile(&mut ecfg, &fs_type);
                if has_casefold {
                    extfs_mkfs::config::apply_features(&mut ecfg, "casefold")
                        .map_err(|e| FastbootError::InvalidArgument(e.to_string()))?;
                }
                if has_projid {
                    extfs_mkfs::config::apply_features(&mut ecfg, "project")
                        .map_err(|e| FastbootError::InvalidArgument(e.to_string()))?;
                }
                format_extfs(ecfg).map_err(|e| FastbootError::InvalidArgument(e.to_string()))?;
            }
            "f2fs" => {
                let mut fcfg = F2fsMkfsConfig {
                    device_path: sp_img_path.clone(),
                    sparse_size: Some(part_size),
                    trim: false,
                    force: true,
                    quiet: true,
                    nat_bits: false,
                    features: F2FS_FEATURE_ENCRYPT
                        | F2FS_FEATURE_EXTRA_ATTR
                        | F2FS_FEATURE_PRJQUOTA
                        | F2FS_FEATURE_QUOTA_INO
                        | F2FS_FEATURE_VERITY,
                    usr_grp_quota: true,
                    prj_quota: true,
                    ..F2fsMkfsConfig::default()
                };
                if has_casefold {
                    fcfg.features |= F2FS_FEATURE_CASEFOLD;
                    fcfg.encoding = F2FS_ENC_UTF8_12_1;
                }
                if has_projid {
                    fcfg.features |= F2FS_FEATURE_PRJQUOTA | F2FS_FEATURE_EXTRA_ATTR;
                    fcfg.prj_quota = true;
                }
                if has_compress {
                    fcfg.features |= F2FS_FEATURE_COMPRESSION | F2FS_FEATURE_EXTRA_ATTR;
                }
                format_f2fs(&fcfg).map_err(|e| FastbootError::InvalidArgument(e.to_string()))?;
            }
            other => {
                eprintln!("fastboot: error: Formatting is not supported for filesystem with type '{other}'.");
                continue;
            }
        }

        let sp_bytes = fs::read(&sp_img_path)?;
        flash_buffer_to_partition(transport, cfg, &part, sp_bytes).await?;
    }
    Ok(())
}

fn resolve_product_out_image(part_base: &str) -> Result<PathBuf, FastbootError> {
    let out_dir = std::env::var("ANDROID_PRODUCT_OUT").map_err(|_| {
        FastbootError::InvalidArgument(
            "ANDROID_PRODUCT_OUT not set and no image filename specified".into(),
        )
    })?;
    let stripped = part_base
        .strip_suffix("_a")
        .or_else(|| part_base.strip_suffix("_b"))
        .unwrap_or(part_base);
    Ok(Path::new(&out_dir).join(format!("{stripped}.img")))
}

/// Executes all tasks in [`FastbootSessionConfig`].
pub async fn execute_session(cfg: FastbootSessionConfig) -> Result<(), FastbootError> {
    let session_start = Instant::now();
    let mut transport = AnyFastbootTransport::connect(cfg.serial.as_deref()).await?;

    if let Some(slot_opt) = &cfg.set_active_slot {
        let slot = match slot_opt {
            Some(s) => s.clone(),
            None => cfg
                .slot_override
                .clone()
                .unwrap_or_else(|| "a".to_string()),
        };
        execute_set_active(&mut transport, &slot).await?;
    }

    for task in &cfg.tasks {
        match task {
            FastbootTask::GetVar(name) => {
                if name == "all" {
                    let (lines, dur) = transport.get_all_vars().await?;
                    for line in lines {
                        eprintln!("(bootloader) {line}");
                    }
                    eprintln!("all: ");
                    let _ = dur;
                } else {
                    let (val, _) = transport.get_var(name).await?;
                    eprintln!("{name}: {val}");
                }
            }
            FastbootTask::Erase(raw_part) => {
                let targets = resolve_target_partitions(
                    &mut transport,
                    raw_part,
                    cfg.slot_override.as_deref(),
                )
                .await?;
                for part in targets {
                    let _ = transport
                        .get_var_optional(&format!("partition-type:{part}"))
                        .await?;
                    let (_, dur) = transport.execute_command(&format!("erase:{part}")).await?;
                    print_status(&format!("Erasing '{part}'"), dur);
                }
            }
            FastbootTask::Format {
                partition,
                fs_type,
                size_override,
            } => {
                execute_format_partition(
                    &mut transport,
                    &cfg,
                    partition,
                    fs_type.as_deref(),
                    *size_override,
                )
                .await?;
            }
            FastbootTask::Flash {
                partition,
                filename,
            } => {
                if let Some((vb_base, ramdisk_name)) = partition.split_once(':') {
                    let img_path = match filename {
                        Some(p) => p.clone(),
                        None => resolve_product_out_image("vendor_ramdisk")?,
                    };
                    let new_ramdisk = fs::read(&img_path)?;
                    let dtb_bytes = match &cfg.dtb_path {
                        Some(p) => Some(fs::read(p)?),
                        None => None,
                    };
                    let targets = resolve_target_partitions(
                        &mut transport,
                        vb_base,
                        cfg.slot_override.as_deref(),
                    )
                    .await?;
                    for part in targets {
                        let (existing_vb, fetch_dur) =
                            fetch_partition_bytes(&mut transport, &part, None, None).await?;
                        print_status(&format!("Fetching '{part}'"), fetch_dur);
                        let repacked = repack_vendor_boot_image(
                            &existing_vb,
                            ramdisk_name,
                            &new_ramdisk,
                            dtb_bytes.as_deref(),
                        )?;
                        flash_buffer_to_partition(&mut transport, &cfg, &part, repacked).await?;
                    }
                } else {
                    let img_path = match filename {
                        Some(p) => p.clone(),
                        None => resolve_product_out_image(partition)?,
                    };
                    let data = fs::read(&img_path)?;
                    let targets = resolve_target_partitions(
                        &mut transport,
                        partition,
                        cfg.slot_override.as_deref(),
                    )
                    .await?;
                    for part in targets {
                        flash_buffer_to_partition(&mut transport, &cfg, &part, data.clone())
                            .await?;
                    }
                }
            }
            FastbootTask::FlashRaw {
                partition,
                kernel,
                ramdisk,
                second,
            } => {
                let k_bytes = fs::read(kernel)?;
                let r_bytes = match ramdisk {
                    Some(p) => Some(fs::read(p)?),
                    None => None,
                };
                let s_bytes = match second {
                    Some(p) => Some(fs::read(p)?),
                    None => None,
                };
                let d_bytes = match &cfg.dtb_path {
                    Some(p) => Some(fs::read(p)?),
                    None => None,
                };
                let boot_img = build_boot_image(
                    &cfg.boot_args,
                    &k_bytes,
                    r_bytes.as_deref(),
                    s_bytes.as_deref(),
                    d_bytes.as_deref(),
                )?;
                let targets = resolve_target_partitions(
                    &mut transport,
                    partition,
                    cfg.slot_override.as_deref(),
                )
                .await?;
                for part in targets {
                    flash_buffer_to_partition(&mut transport, &cfg, &part, boot_img.clone())
                        .await?;
                }
            }
            FastbootTask::Boot {
                kernel,
                ramdisk,
                second,
            } => {
                let k_bytes = fs::read(kernel)?;
                let r_bytes = match ramdisk {
                    Some(p) => Some(fs::read(p)?),
                    None => None,
                };
                let s_bytes = match second {
                    Some(p) => Some(fs::read(p)?),
                    None => None,
                };
                let d_bytes = match &cfg.dtb_path {
                    Some(p) => Some(fs::read(p)?),
                    None => None,
                };
                let boot_img = build_boot_image(
                    &cfg.boot_args,
                    &k_bytes,
                    r_bytes.as_deref(),
                    s_bytes.as_deref(),
                    d_bytes.as_deref(),
                )?;
                let dl_dur = transport.download_data(&boot_img).await?;
                print_status(
                    &format!("Sending 'boot.img' ({} KB)", boot_img.len().max(1024) / 1024),
                    dl_dur,
                );
                let (_, b_dur) = transport.execute_command("boot").await?;
                print_status("Booting", b_dur);
            }
            FastbootTask::FlashAll => {
                let out_dir = PathBuf::from(std::env::var("ANDROID_PRODUCT_OUT").map_err(
                    |_| FastbootError::InvalidArgument("ANDROID_PRODUCT_OUT not set".into()),
                )?);
                execute_flash_from_dir(&mut transport, &cfg, &out_dir).await?;
            }
            FastbootTask::Update(zip_path) => {
                let zip_file = fs::File::open(zip_path)?;
                let mut archive = zip::ZipArchive::new(zip_file)?;
                let tmp_dir = tempfile::tempdir()?;
                for idx in 0..archive.len() {
                    let mut entry = archive.by_index(idx)?;
                    if entry.is_dir() {
                        continue;
                    }
                    let fname = Path::new(entry.name())
                        .file_name()
                        .map(|s| s.to_os_string())
                        .unwrap_or_default();
                    if fname.is_empty() {
                        continue;
                    }
                    let out_path = tmp_dir.path().join(fname);
                    let mut out_f = fs::File::create(out_path)?;
                    std::io::copy(&mut entry, &mut out_f)?;
                }
                execute_flash_from_dir(&mut transport, &cfg, tmp_dir.path()).await?;
            }
            FastbootTask::Reboot(target_opt) => {
                let (wire_cmd, label) = match target_opt.as_deref() {
                    Some("bootloader") => ("reboot-bootloader", "Rebooting into bootloader"),
                    Some("fastboot") => ("reboot-fastboot", "Rebooting into fastboot"),
                    Some("recovery") => ("reboot-recovery", "Rebooting into recovery"),
                    Some(other) => {
                        let c = format!("reboot-{other}");
                        let (_, dur) = transport.execute_command(&c).await?;
                        print_status(&format!("Rebooting into {other}"), dur);
                        continue;
                    }
                    None => ("reboot", "Rebooting"),
                };
                let (_, dur) = transport.execute_command(wire_cmd).await?;
                print_status(label, dur);
            }
            FastbootTask::Continue => {
                let (_, dur) = transport.execute_command("continue").await?;
                print_status("Resuming boot", dur);
            }
            FastbootTask::SetActive(slot) => {
                execute_set_active(&mut transport, slot).await?;
            }
            FastbootTask::Flashing(sub) => {
                let cmd = format!("flashing {sub}");
                let (_, dur) = transport.execute_command(&cmd).await?;
                eprintln!("OKAY [{:7.3}s]", dur.as_secs_f64());
            }
            FastbootTask::Oem(args_str) => {
                let cmd = format!("oem {args_str}");
                let (_, dur) = transport.execute_command(&cmd).await?;
                eprintln!("OKAY [{:7.3}s]", dur.as_secs_f64());
            }
            FastbootTask::Gsi(sub) => {
                let cmd = format!("gsi:{sub}");
                let (_, dur) = transport.execute_command(&cmd).await?;
                eprintln!("OKAY [{:7.3}s]", dur.as_secs_f64());
            }
            FastbootTask::WipeSuper(super_empty_opt) => {
                let meta_bytes = match super_empty_opt {
                    Some(p) => fs::read(p)?,
                    None => {
                        if let Ok(out_dir) = std::env::var("ANDROID_PRODUCT_OUT") {
                            let p = Path::new(&out_dir).join("super_empty.img");
                            if p.exists() {
                                fs::read(p)?
                            } else {
                                build_empty_super_metadata(65536, 2, 4096, 4 * 1024 * 1024 * 1024)
                            }
                        } else {
                            build_empty_super_metadata(65536, 2, 4096, 4 * 1024 * 1024 * 1024)
                        }
                    }
                };
                flash_buffer_to_partition(&mut transport, &cfg, "super", meta_bytes).await?;
            }
            FastbootTask::CreateLogicalPartition { name, size } => {
                let cmd = format!("create-logical-partition:{name}:{size}");
                let (_, dur) = transport.execute_command(&cmd).await?;
                print_status(&format!("Creating '{name}'"), dur);
            }
            FastbootTask::DeleteLogicalPartition(name) => {
                let cmd = format!("delete-logical-partition:{name}");
                let (_, dur) = transport.execute_command(&cmd).await?;
                print_status(&format!("Deleting '{name}'"), dur);
            }
            FastbootTask::ResizeLogicalPartition { name, size } => {
                let cmd = format!("resize-logical-partition:{name}:{size}");
                let (_, dur) = transport.execute_command(&cmd).await?;
                print_status(&format!("Resizing '{name}'"), dur);
            }
            FastbootTask::SnapshotUpdate(sub) => {
                let cmd = format!("snapshot-update:{sub}");
                let (_, dur) = transport.execute_command(&cmd).await?;
                print_status(&format!("Snapshot {sub}"), dur);
            }
            FastbootTask::Fetch {
                partition,
                out_file,
                offset,
                size,
            } => {
                let targets = resolve_target_partitions(
                    &mut transport,
                    partition,
                    cfg.slot_override.as_deref(),
                )
                .await?;
                let part = targets.first().map(String::as_str).unwrap_or(partition);
                let (data, dur) =
                    fetch_partition_bytes(&mut transport, part, *offset, *size).await?;
                print_status(&format!("Fetching {part}"), dur);
                fs::write(out_file, &data)?;
            }
            FastbootTask::Stage(in_file) => {
                let data = fs::read(in_file)?;
                let dur = transport.download_data(&data).await?;
                print_status(
                    &format!("Sending '{}' ({} KB)", in_file.display(), data.len().max(1024) / 1024),
                    dur,
                );
            }
            FastbootTask::GetStaged(out_file) => {
                let (data, dur) = transport.upload_data("upload").await?;
                print_status(&format!("Uploading '{}'", out_file.display()), dur);
                fs::write(out_file, &data)?;
            }
        }
    }

    if cfg.wipe_userdata {
        execute_format_partition(&mut transport, &cfg, "userdata", None, None).await?;
        if transport
            .get_var_optional("partition-type:metadata")
            .await?
            .is_some()
        {
            execute_format_partition(&mut transport, &cfg, "metadata", None, None).await?;
        }
    }

    eprintln!(
        "Finished. Total time: {:.3}s",
        session_start.elapsed().as_secs_f64()
    );
    Ok(())
}

async fn execute_set_active(
    transport: &mut AnyFastbootTransport,
    slot: &str,
) -> Result<(), FastbootError> {
    let slot_count_str = transport
        .get_var_optional("slot-count")
        .await?
        .unwrap_or_default();
    let slot_count: u32 = slot_count_str.trim().parse().unwrap_or(0);
    if slot_count < 2 {
        eprintln!("fastboot: error: Device does not support slots");
        return Err(FastbootError::InvalidArgument(
            "Device does not support slots".into(),
        ));
    }
    let clean_slot = slot.trim_start_matches('_');
    let (_, dur) = transport
        .execute_command(&format!("set_active:{clean_slot}"))
        .await?;
    print_status(&format!("Setting current slot to '{clean_slot}'"), dur);
    Ok(())
}

async fn execute_flash_from_dir(
    transport: &mut AnyFastbootTransport,
    cfg: &FastbootSessionConfig,
    dir: &Path,
) -> Result<(), FastbootError> {
    let info_path = dir.join("fastboot-info.txt");
    let steps = if !cfg.disable_fastboot_info && info_path.exists() {
        let mut txt = String::new();
        fs::File::open(&info_path)?.read_to_string(&mut txt)?;
        parse_fastboot_info(&txt)
    } else {
        let default_parts = [
            "boot",
            "init_boot",
            "vendor_boot",
            "dtbo",
            "vbmeta",
            "system",
            "vendor",
            "product",
        ];
        default_parts
            .iter()
            .map(|p| FastbootInfoStep::Flash {
                partition: (*p).to_string(),
                image_name: Some(format!("{p}.img")),
                slot_other: false,
                if_wipe: false,
            })
            .collect()
    };

    for step in steps {
        match step {
            FastbootInfoStep::Version(_) => {}
            FastbootInfoStep::Flash {
                partition,
                image_name,
                slot_other,
                if_wipe,
            } => {
                if if_wipe && !cfg.wipe_userdata {
                    continue;
                }
                if slot_other && cfg.skip_secondary {
                    continue;
                }
                let img_file = image_name.unwrap_or_else(|| format!("{partition}.img"));
                let img_path = dir.join(&img_file);
                if !img_path.exists() {
                    continue;
                }
                let data = fs::read(&img_path)?;
                let slot_ov = if slot_other {
                    Some("other")
                } else {
                    cfg.slot_override.as_deref()
                };
                let targets = resolve_target_partitions(transport, &partition, slot_ov).await?;
                for part in targets {
                    flash_buffer_to_partition(transport, cfg, &part, data.clone()).await?;
                }
            }
            FastbootInfoStep::Reboot(target) => {
                let wire_cmd = match target.as_deref() {
                    Some("bootloader") => "reboot-bootloader",
                    Some("fastboot") => "reboot-fastboot",
                    _ => "reboot",
                };
                let (_, dur) = transport.execute_command(wire_cmd).await?;
                print_status("Rebooting", dur);
            }
            FastbootInfoStep::UpdateSuper => {
                let super_empty = dir.join("super_empty.img");
                if super_empty.exists() {
                    let data = fs::read(super_empty)?;
                    let dur = transport.download_data(&data).await?;
                    print_status("Sending 'super_empty.img'", dur);
                    let (_, u_dur) = transport.execute_command("update-super:super").await?;
                    print_status("Updating super partition", u_dur);
                }
            }
            FastbootInfoStep::Erase(part) => {
                let targets =
                    resolve_target_partitions(transport, &part, cfg.slot_override.as_deref())
                        .await?;
                for t in targets {
                    let (_, dur) = transport.execute_command(&format!("erase:{t}")).await?;
                    print_status(&format!("Erasing '{t}'"), dur);
                }
            }
        }
    }

    if !cfg.skip_reboot {
        let (_, dur) = transport.execute_command("reboot").await?;
        print_status("Rebooting", dur);
    }

    Ok(())
}

/// Top-level CLI runner for `fastboot`. Returns process exit code.
pub async fn run_cli(args: &[String]) -> i32 {
    match parse_cli_args(args) {
        Ok(FastbootCliAction::ShowVersion) => {
            print!("{}", version_output());
            0
        }
        Ok(FastbootCliAction::ShowHelp { exit_code }) => {
            print!("{}", help_output());
            exit_code
        }
        Ok(FastbootCliAction::ListDevices { long }) => match list_usb_devices().await {
            Ok(devs) => {
                for d in devs {
                    if long {
                        println!("{:<22} {:<16} {}", d.serial, d.mode, d.device_path);
                    } else {
                        println!("{}\t{}", d.serial, d.mode);
                    }
                }
                0
            }
            Err(_) => 0,
        },
        Ok(FastbootCliAction::Execute(cfg)) => match execute_session(cfg).await {
            Ok(()) => 0,
            Err(FastbootError::InvalidArgument(msg)) if msg == "Device does not support slots" => 1,
            Err(e) => {
                eprintln!("fastboot: error: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("fastboot: error: {e}");
            1
        }
    }
}
