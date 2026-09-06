//! Container-only transforms: keep Zen packages, hashes, paths and container identity intact.
use std::{fs, io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write}, path::Path, sync::Arc};
use aes::cipher::{BlockDecrypt, BlockEncrypt};
use anyhow::{bail, Context, Result};
use crate::{compression::{compress, decompress, CompressionMethod}, logging::{emit_log, emit_progress},
    ser::{ReadExt, WriteExt}, Config, EIoContainerFlags, FIoStoreTocCompressedBlockEntry,
    FIoStoreTocEntryMetaFlags, Toc};

#[derive(Debug, Default)]
pub struct IoStoreRepackStats {
    pub blocks: usize,
    pub reused_blocks: usize,
    pub recompressed_blocks: usize,
    pub output_bytes: u64,
}

/// A container layout that must use the existing asset-rebuild path instead.
#[derive(Debug)]
pub struct DirectRepackUnsupported;
impl std::fmt::Display for DirectRepackUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Direct repacking requires an unsigned, single-part container without memory-mapped chunks")
    }
}
impl std::error::Error for DirectRepackUnsupported {}

/// Repack a single-part, unsigned mod without converting its Zen assets to legacy.
/// Reuses compressed blocks when the codec already matches. Stages the complete
/// bundle beside the destination and restores existing files if publication fails.
/// The companion PAK (including any loose files) is preserved byte-for-byte.
pub fn repack_iostore(
    input: &Path, output: &Path, compression: Option<CompressionMethod>,
    encrypted: bool, config: Arc<Config>,
) -> Result<IoStoreRepackStats> {
    let mut toc: Toc = BufReader::new(fs::File::open(input)?).de_ctx(config.clone())?;
    if toc.partition_count > 1 || toc.container_flags.contains(EIoContainerFlags::Signed) {
        return Err(DirectRepackUnsupported.into());
    }
    if toc.compression_block_size == 0 || (toc.compression_blocks.is_empty() && !toc.chunks.is_empty()) {
        bail!("Direct repacking requires a valid compression block table");
    }
    if toc.chunk_metas.iter().any(|m| m.flags.contains(FIoStoreTocEntryMetaFlags::MemoryMapped)) {
        return Err(DirectRepackUnsupported.into());
    }
    let input_encrypted = toc.container_flags.contains(EIoContainerFlags::Encrypted);
    let read_key = if input_encrypted {
        Some(config.aes_key(&toc.encryption_key_guid).context("Missing input AES key")?.clone())
    } else { None };
    let write_key = if encrypted {
        Some(config.write_aes_key.as_ref().or_else(|| config.aes_key(&toc.encryption_key_guid))
            .context("Missing output AES key")?.clone())
    } else { None };
    let parent = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new().prefix(".retoc-repack-").tempdir_in(parent)?;
    let staged_toc = staging.path().join("bundle.utoc");
    let mut source = BufReader::with_capacity(1024 * 1024, fs::File::open(input.with_extension("ucas"))?);
    let source_len = source.get_ref().metadata()?.len();
    let mut destination = BufWriter::with_capacity(1024 * 1024, fs::File::create(staged_toc.with_extension("ucas"))?);
    let source_methods = toc.compression_methods.clone();
    let preserve_uncompressed = compression.is_none() || compression.is_some_and(|m| source_methods.contains(&m));
    let mut stats = IoStoreRepackStats { blocks: toc.compression_blocks.len(), ..Default::default() };
    let mut source_position = 0u64;
    let mut encoded = Vec::new();
    let mut decoded = Vec::new();
    let mut recompressed = Vec::new();
    emit_log("Repacking IoStore blocks directly (preserving Zen packages)");
    for (index, block) in toc.compression_blocks.iter_mut().enumerate() {
        let method_index = block.get_compression_method_index() as usize;
        let method = if method_index == 0 { None } else {
            Some(*source_methods.get(method_index - 1).context("Invalid compression method index")?)
        };
        let compressed_size = block.get_compressed_size() as usize;
        let uncompressed_size = block.get_uncompressed_size() as usize;
        if uncompressed_size > toc.compression_block_size as usize || compressed_size == 0
            || (method.is_none() && compressed_size != uncompressed_size) {
            bail!("Invalid compression block {index}");
        }
        let read_size = if input_encrypted { crate::align_usize(compressed_size, 16) } else { compressed_size };
        let end = block.get_offset().checked_add(read_size as u64).context("Block offset overflow")?;
        if end > source_len { bail!("Compression block {index} extends beyond UCAS"); }
        if source_position != block.get_offset() { source.seek(SeekFrom::Start(block.get_offset()))?; }
        encoded.resize(read_size, 0);
        source.read_exact(&mut encoded)?;
        source_position = end;
        if let Some(key) = &read_key {
            for bytes in encoded.chunks_exact_mut(16) { key.0.decrypt_block(bytes.into()); }
        }
        encoded.truncate(compressed_size);
        let reuse = method == compression || (method.is_none() && preserve_uncompressed);
        let output_method = if reuse {
            stats.reused_blocks += 1;
            method
        } else {
            decoded.resize(uncompressed_size, 0);
            if let Some(method) = method {
                decompress(method, &encoded, &mut decoded)?;
            } else {
                if encoded.len() != decoded.len() { bail!("Invalid uncompressed block length"); }
                decoded.copy_from_slice(&encoded);
            }
            recompressed.clear();
            if let Some(method) = compression { compress(method, &decoded, &mut recompressed)?; }
            stats.recompressed_blocks += 1;
            if compression.is_some() && recompressed.len() < decoded.len() {
                std::mem::swap(&mut encoded, &mut recompressed);
                compression
            } else {
                std::mem::swap(&mut encoded, &mut decoded);
                None
            }
        };
        let size = encoded.len();
        if stats.output_bytes >= (1u64 << 40) || size >= (1usize << 24) {
            bail!("Repacked block exceeds the IoStore format limits");
        }
        if let Some(key) = &write_key {
            encoded.resize(crate::align_usize(size, 16), 0);
            for bytes in encoded.chunks_exact_mut(16) { key.0.encrypt_block(bytes.into()); }
        }
        *block = FIoStoreTocCompressedBlockEntry::new(stats.output_bytes, size as u32,
            uncompressed_size as u32, u8::from(output_method.is_some()));
        destination.write_all(&encoded)?;
        stats.output_bytes += encoded.len() as u64;
        if index % 256 == 0 || index + 1 == stats.blocks { emit_progress((index + 1) as u64, stats.blocks as u64); }
    }
    destination.flush()?;
    drop(destination);
    drop(source);
    toc.compression_methods = compression.into_iter().collect();
    // The rewritten UCAS is a single partition, even if decompression expanded it.
    toc.partition_size = u64::MAX;
    toc.container_flags.set(EIoContainerFlags::Compressed, compression.is_some());
    toc.set_obfuscated(encrypted);
    if let Some(guid) = config.write_encryption_key_guid { toc.encryption_key_guid = guid; }
    let mut writer = BufWriter::new(fs::File::create(&staged_toc)?);
    writer.ser(&toc)?;
    writer.flush()?;
    drop(writer);
    let mut extensions = vec!["ucas", "utoc"];
    if input.with_extension("pak").is_file() {
        fs::copy(input.with_extension("pak"), staged_toc.with_extension("pak"))?;
        extensions.push("pak");
    }
    publish(staging, output, &extensions)?;
    Ok(stats)
}

fn publish(staging: tempfile::TempDir, output: &Path, extensions: &[&str]) -> Result<()> {
    let mut backups = Vec::new();
    let mut published = Vec::new();
    let result = (|| -> Result<()> {
        for ext in extensions {
            let target = output.with_extension(ext);
            if target.exists() {
                if !target.is_file() { bail!("Destination is not a file: {}", target.display()); }
                let backup = staging.path().join(format!("original.{ext}"));
                fs::rename(&target, &backup)?;
                backups.push((backup, target.clone()));
            }
            fs::rename(staging.path().join(format!("bundle.{ext}")), &target)?;
            published.push(target);
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut rollback_errors = Vec::new();
        for target in published.iter().rev() {
            if let Err(e) = fs::remove_file(target) { rollback_errors.push(e.to_string()); }
        }
        for (backup, target) in backups.iter().rev() {
            if let Err(e) = fs::rename(backup, target) { rollback_errors.push(e.to_string()); }
        }
        if !rollback_errors.is_empty() {
            let recovery = staging.into_path();
            bail!("{error}; rollback failed: {rollback_errors:?}. Recovery files: {}", recovery.display());
        }
        return Err(error.context("Failed to publish repacked bundle; original files restored"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AesKey, EIoChunkType, EIoStoreTocVersion, FGuid, FIoChunkId, iostore_writer::IoStoreWriter};

    fn config() -> Arc<Config> {
        let mut config = Config::default();
        config.aes_keys.insert(FGuid::default(),
            "0102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F20".parse().unwrap());
        Arc::new(config)
    }
    fn fixture(path: &Path) -> Result<Vec<Vec<u8>>> {
        let mut writer = IoStoreWriter::new(path, EIoStoreTocVersion::PerfectHashWithOverflow,
            None, "../../../".into(), Some(CompressionMethod::Zlib), false, None, None)?;
        let payloads = vec![vec![0x53; 262177], (0..131079).map(|i| (i * 17 % 251) as u8).collect()];
        for (i, data) in payloads.iter().enumerate() {
            let id = FIoChunkId::create(i as u64 + 42, 0, EIoChunkType::BulkData);
            let path = crate::UEPathBuf::from(format!("../../../Marvel/Content/test{i}.ubulk"));
            writer.write_chunk(id, Some(&path), data)?;
        }
        writer.finalize()?;
        fs::write(path.with_extension("pak"), b"companion containing loose files")?;
        Ok(payloads)
    }
    fn verify(path: &Path, config: Arc<Config>, expected: &[Vec<u8>], original: &Toc) -> Result<Toc> {
        let toc: Toc = BufReader::new(fs::File::open(path)?).de_ctx(config)?;
        let mut cas = fs::File::open(path.with_extension("ucas"))?;
        assert_eq!(toc.container_id, original.container_id);
        assert_eq!(toc.chunks, original.chunks);
        assert_eq!(toc.file_map, original.file_map);
        for (i, bytes) in expected.iter().enumerate() {
            assert_eq!(&toc.read(&mut cas, i as u32)?, bytes);
            assert_eq!(toc.chunk_metas[i].chunk_hash.0, original.chunk_metas[i].chunk_hash.0);
        }
        assert_eq!(fs::read(path.with_extension("pak"))?, b"companion containing loose files");
        Ok(toc)
    }
    #[test]
    fn encryption_reuses_blocks_and_preserves_all_payloads() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("source.utoc");
        let expected = fixture(&input)?;
        let config = config();
        let original: Toc = BufReader::new(fs::File::open(&input)?).de_ctx(config.clone())?;
        let output = dir.path().join("encrypted.utoc");
        let stats = repack_iostore(&input, &output, Some(CompressionMethod::Zlib), true, config.clone())?;
        assert_eq!(stats.recompressed_blocks, 0);
        assert_eq!(stats.reused_blocks, original.compression_blocks.len());
        let encrypted = verify(&output, config.clone(), &expected, &original)?;
        assert!(encrypted.container_flags.contains(EIoContainerFlags::Encrypted));
        // In-place decrypt also exercises replacement of an existing three-file bundle.
        repack_iostore(&output, &output, Some(CompressionMethod::Zlib), false, config.clone())?;
        verify(&output, config, &expected, &original)?;
        assert_eq!(fs::read(input.with_extension("ucas"))?, fs::read(output.with_extension("ucas"))?);
        Ok(())
    }
    #[test]
    fn codec_changes_and_custom_key_rotation_preserve_payloads() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("source.utoc");
        let expected = fixture(&input)?;
        let read_config = config();
        let original: Toc = BufReader::new(fs::File::open(&input)?).de_ctx(read_config.clone())?;
        let guid: FGuid = "0123456789ABCDEFFEDCBA9876543210".parse()?;
        let key: AesKey = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".parse()?;
        let mut write_config = Config::default();
        write_config.write_aes_key = Some(key.clone());
        write_config.write_encryption_key_guid = Some(guid);
        let mut rotated_config = Config::default();
        rotated_config.aes_keys.insert(guid, key);
        let rotated_config = Arc::new(rotated_config);
        let output = dir.path().join("rotated.utoc");
        repack_iostore(&input, &output, Some(CompressionMethod::LZ4), true, Arc::new(write_config))?;
        let toc = verify(&output, rotated_config.clone(), &expected, &original)?;
        assert_eq!(toc.encryption_key_guid, guid);
        repack_iostore(&output, &output, None, false, rotated_config.clone())?;
        let toc = verify(&output, rotated_config, &expected, &original)?;
        assert!(toc.compression_methods.is_empty());
        assert!(!toc.container_flags.contains(EIoContainerFlags::Encrypted));
        Ok(())
    }
    #[test]
    fn failed_transform_and_publication_restore_existing_outputs() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("source.utoc");
        fixture(&input)?;
        let output = dir.path().join("target.utoc");
        fs::write(output.with_extension("ucas"), b"original cas")?;
        // A directory at the TOC destination forces failure after UCAS publication.
        fs::create_dir(&output)?;
        assert!(repack_iostore(&input, &output, Some(CompressionMethod::Zlib), true, config()).is_err());
        assert_eq!(fs::read(output.with_extension("ucas"))?, b"original cas");
        assert!(output.is_dir());
        fs::File::options().write(true).open(input.with_extension("ucas"))?.set_len(1)?;
        assert!(repack_iostore(&input, &output, Some(CompressionMethod::Zlib), true, config()).is_err());
        assert_eq!(fs::read(output.with_extension("ucas"))?, b"original cas");
        Ok(())
    }
}
