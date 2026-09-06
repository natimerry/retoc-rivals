use crate::{
    align_usize,
    chunk_id::FIoChunkIdRaw,
    compression::{compress, CompressionMethod},
    container_header::{EIoContainerHeaderVersion, FIoContainerHeader, StoreEntry},
    AesKey, EIoChunkType, FPackageId, UEPath, UEPathBuf,
};
use crate::{
    ser::*, EIoStoreTocVersion, FIoChunkHash, FIoChunkId, FIoContainerId, FIoOffsetAndLength,
    FIoStoreTocCompressedBlockEntry, FIoStoreTocEntryMeta, FIoStoreTocEntryMetaFlags, Toc,
};
use anyhow::{Context, Result};
use fs_err as fs;
use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

pub(crate) struct IoStoreWriter {
    toc_path: PathBuf,
    toc_stream: BufWriter<fs::File>,
    cas_stream: BufWriter<fs::File>,
    cas_offset: u64,
    toc: Toc,
    container_header: Option<FIoContainerHeader>,
    compression: Option<CompressionMethod>,
    obfuscated: bool,
    aes_key: Option<AesKey>,
}

impl IoStoreWriter {
    pub(crate) fn set_obfuscated(&mut self, obfuscated: bool) {
        self.toc.set_obfuscated(obfuscated);
        self.obfuscated = obfuscated;
    }

    pub(crate) fn new<P: AsRef<Path>>(
        toc_path: P,
        toc_version: EIoStoreTocVersion,
        container_header_version: Option<EIoContainerHeaderVersion>,
        mount_point: UEPathBuf,
        compression: Option<CompressionMethod>,
        obfuscated: bool,
        encryption_key_guid: Option<crate::FGuid>,
        aes_key: Option<AesKey>,
    ) -> Result<Self> {
        let toc_path = toc_path.as_ref().to_path_buf();
        let name = toc_path.file_stem().unwrap().to_string_lossy();
        let toc_stream = BufWriter::new(fs::File::create(&toc_path)?);
        let cas_stream = BufWriter::new(fs::File::create(toc_path.with_extension("ucas"))?);

        let mut toc = Toc::new();
        toc.compression_block_size = 0x20000;
        toc.version = toc_version;
        toc.container_id = FIoContainerId::from_name(&name);
        toc.directory_index.mount_point = mount_point;
        toc.partition_size = u64::MAX;
        if let Some(guid) = encryption_key_guid {
            toc.encryption_key_guid = guid;
        }

        if obfuscated {
            toc.set_obfuscated(true);
        }

        if let Some(method) = compression {
            toc.compression_methods.push(method);
            toc.container_flags |= crate::EIoContainerFlags::Compressed
        }

        let container_header =
            container_header_version.map(|v| FIoContainerHeader::new(v, toc.container_id));

        Ok(Self {
            toc_path,
            toc_stream,
            cas_stream,
            cas_offset: 0,
            toc,
            container_header,
            compression,
            obfuscated,
            aes_key,
        })
    }
    pub(crate) fn write_chunk_raw(
        &mut self,
        chunk_id_raw: FIoChunkIdRaw,
        path: Option<&UEPath>,
        data: &[u8],
    ) -> Result<()> {
        self.write_chunk(
            FIoChunkId::from_raw(chunk_id_raw, self.toc.version),
            path,
            data,
        )
    }

    pub fn write_chunk(
        &mut self,
        chunk_id: FIoChunkId,
        path: Option<&UEPath>,
        data: &[u8],
    ) -> Result<()> {
        self.write_chunk_inner(chunk_id, path, data, self.compression)
    }

    pub fn write_chunk_uncompressed(
        &mut self,
        chunk_id: FIoChunkId,
        path: Option<&UEPath>,
        data: &[u8],
    ) -> Result<()> {
        self.write_chunk_inner(chunk_id, path, data, None)
    }

    fn write_chunk_inner(
        &mut self,
        chunk_id: FIoChunkId,
        path: Option<&UEPath>,
        data: &[u8],
        compression: Option<CompressionMethod>,
    ) -> Result<()> {
        if let Some(path) = path {
            let index = &mut self.toc.directory_index;
            let relative_path = path.strip_prefix(&index.mount_point).with_context(|| {
                format!(
                    "mount point {} does not contain path {path}",
                    index.mount_point
                )
            })?;
            index.add_file(relative_path, self.toc.chunks.len() as u32);
        }

        let mut offset = self.cas_offset;
        let start_block = self.toc.compression_blocks.len();
        let mut hasher = blake3::Hasher::new();

        for block in data.chunks(self.toc.compression_block_size as usize) {
            hasher.update(block);

            let (bytes_to_write, compression_method_index) = match compression {
                Some(method) => {
                    let mut compressed = Vec::new();
                    compress(method, block, &mut compressed)?;
                    if compressed.len() < block.len() {
                        (compressed, 1u8)
                    } else {
                        (block.to_vec(), 0u8)
                    }
                }
                None => (block.to_vec(), 0u8),
            };

            let compressed_size = bytes_to_write.len() as u32;
            let uncompressed_size = block.len() as u32;

            let final_bytes = if self.obfuscated {
                use aes::cipher::BlockEncrypt;
                const DEFAULT_AES_KEY: &str =
                    "0C263D8C22DCB085894899C3A3796383E9BF9DE0CBFB08C9BF2DEF2E84F29D74";
                let default_key;
                let key = if let Some(key) = self.aes_key.as_ref() {
                    key
                } else {
                    default_key = DEFAULT_AES_KEY.parse()?;
                    &default_key
                };
                let padded_len = (bytes_to_write.len() + 15) & !15;
                let mut padded = bytes_to_write;
                padded.resize(padded_len, 0u8);
                for chunk in padded.chunks_mut(16) {
                    let block = aes::Block::from_mut_slice(chunk);
                    key.0.encrypt_block(block);
                }
                padded
            } else {
                bytes_to_write
            };

            self.cas_stream.write_all(&final_bytes)?;
            let written_size = final_bytes.len() as u32;

            self.toc
                .compression_blocks
                .push(FIoStoreTocCompressedBlockEntry::new(
                    offset,
                    compressed_size,
                    uncompressed_size,
                    compression_method_index,
                ));

            offset += written_size as u64;
        }

        self.cas_offset = offset;

        let hash = hasher.finalize();
        let meta = FIoStoreTocEntryMeta {
            chunk_hash: FIoChunkHash::from_blake3(hash.as_bytes()),
            flags: FIoStoreTocEntryMetaFlags::empty(),
        };
        let offset_and_length = FIoOffsetAndLength::new(
            start_block as u64 * self.toc.compression_block_size as u64,
            data.len() as u64,
        );
        self.toc
            .chunks
            .push(chunk_id.with_version(self.toc.version));
        self.toc.chunk_offset_lengths.push(offset_and_length);
        self.toc.chunk_metas.push(meta);

        Ok(())
    }

    pub(crate) fn write_package_chunk(
        &mut self,
        chunk_id: FIoChunkId,
        path: Option<&UEPath>,
        data: &[u8],
        store_entry: &StoreEntry,
    ) -> Result<()> {
        let container_header = self
            .container_header
            .as_mut()
            .expect("FIoContainerHeader is required to write package chunks");
        container_header.add_package(FPackageId(chunk_id.get_chunk_id()), store_entry.clone());
        self.write_chunk(chunk_id, path, data)
    }
    pub(crate) fn add_localized_package(
        &mut self,
        package_culture: &str,
        source_package_name: &str,
        localized_package_id: FPackageId,
    ) -> Result<()> {
        let container_header = self
            .container_header
            .as_mut()
            .expect("FIoContainerHeader is required to add localized packages");
        container_header.add_localized_package(
            package_culture,
            source_package_name,
            localized_package_id,
        )
    }
    pub(crate) fn add_package_redirect(
        &mut self,
        source_package_name: &str,
        redirect_package_id: FPackageId,
    ) -> Result<()> {
        let container_header = self
            .container_header
            .as_mut()
            .expect("FIoContainerHeader is required to add package redirects");
        container_header.add_package_redirect(source_package_name, redirect_package_id)
    }
    pub(crate) fn container_version(&self) -> EIoStoreTocVersion {
        self.toc.version
    }
    pub(crate) fn container_header_version(&self) -> EIoContainerHeaderVersion {
        self.container_header.as_ref().unwrap().version
    }
    pub(crate) fn finalize(mut self) -> Result<()> {
        if let Some(container_header) = &self.container_header {
            let mut chunk_buffer = vec![];
            container_header.ser(&mut chunk_buffer)?;
            // container header is always aligned for AES for some reason
            chunk_buffer.resize(align_usize(chunk_buffer.len(), 16), 0);

            let chunk_id = FIoChunkId::create(
                container_header.container_id.0,
                0,
                EIoChunkType::ContainerHeader,
            );
            self.write_chunk_uncompressed(chunk_id, None, &chunk_buffer)?;
        }
        self.toc_stream.ser(&self.toc)?;
        self.cas_stream.flush()?;
        self.toc_stream.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fs_err as fs;

    #[test]
    fn test_write_container() -> Result<()> {
        fs::create_dir("out").ok();
        let mut writer = IoStoreWriter::new(
            "out/new.utoc",
            EIoStoreTocVersion::PerfectHashWithOverflow,
            Some(EIoContainerHeaderVersion::OptionalSegmentPackages),
            "../../..".into(),
            None,
            false,
            None,
            None,
        )?;

        let data = fs::read("tests/UE5.3/ScriptObjects.bin")?;
        writer.write_chunk_raw(
            FIoChunkIdRaw {
                id: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5],
            },
            Some(UEPath::new("../../../asdf/asdf/dasf/script_objects.bin")),
            &data,
        )?;
        writer.finalize()?;
        Ok(())
    }

    #[test]
    fn writes_custom_guid_and_aes_key() -> Result<()> {
        use aes::cipher::BlockEncrypt;

        let output_dir =
            std::env::temp_dir().join(format!("retoc-iostore-crypto-{}", std::process::id()));
        fs::create_dir_all(&output_dir)?;
        let utoc = output_dir.join("custom.utoc");
        let guid: crate::FGuid = "4D4F44534D415256454C4B4559303031".parse()?;
        let key: AesKey =
            "0102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F20".parse()?;
        let mut writer = IoStoreWriter::new(
            &utoc,
            EIoStoreTocVersion::PerfectHashWithOverflow,
            None,
            "../../..".into(),
            None,
            true,
            Some(guid),
            Some(key.clone()),
        )?;
        let plaintext = [0x5Au8; 16];
        writer.write_chunk_raw(
            FIoChunkIdRaw {
                id: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5],
            },
            None,
            &plaintext,
        )?;
        writer.finalize()?;

        let mut utoc_reader = std::io::BufReader::new(fs::File::open(&utoc)?);
        let header: crate::FIoStoreTocHeader = utoc_reader.de()?;
        assert_eq!(header.encryption_key_guid, guid);

        let mut expected = plaintext;
        key.0.encrypt_block((&mut expected).into());
        assert_eq!(fs::read(utoc.with_extension("ucas"))?, expected);

        fs::remove_dir_all(output_dir)?;
        Ok(())
    }

    #[test]
    fn default_aes_key_applies_to_any_input_guid() -> Result<()> {
        let mut config = crate::Config::default();
        config.aes_keys.insert(
            crate::FGuid::default(),
            "0C263D8C22DCB085894899C3A3796383E9BF9DE0CBFB08C9BF2DEF2E84F29D74".parse()?,
        );
        let arbitrary_guid = "0123456789ABCDEFFEDCBA9876543210".parse()?;
        assert!(config.aes_key(&arbitrary_guid).is_some());
        Ok(())
    }
}
