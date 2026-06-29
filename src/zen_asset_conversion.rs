use crate::container_header::{EIoContainerHeaderVersion, StoreEntry};
use crate::iostore_writer::IoStoreWriter;
use crate::legacy_asset::{
    convert_localized_package_name_to_source, get_package_object_full_name, get_public_export_hash,
    EPackageFlags, FLegacyPackageFileSummary, FLegacyPackageHeader, FPackageNameMap,
    FSerializedAssetBundle,
};
use crate::logging::{emit_log, log, Log};
use crate::name_map::{EMappedNameType, FNameMap};
use crate::script_objects::{
    FPackageImportReference, FPackageObjectIndex, FPackageObjectIndexType,
};
use crate::ser::{ReadExt, WriteExt};
use crate::version_heuristics::heuristic_zen_version_from_package_file_version;
use crate::zen::{
    EExportCommandType, EExportFilterFlags, EObjectFlags, EZenPackageVersion,
    ExternalPackageDependency, FBulkDataMapEntry, FDependencyBundleEntry, FDependencyBundleHeader,
    FExportBundleEntry, FExportBundleHeader, FExportMapEntry, FExternalDependencyArc,
    FInternalDependencyArc, FPackageFileVersion, FPackageIndex, FZenPackageHeader,
    FZenPackageVersioningInfo,
};
use crate::{EIoChunkType, FIoChunkId, FPackageId, FSHAHash, UEPath, UEPathBuf};
use anyhow::{anyhow, bail};
use byteorder::{ReadBytesExt, LE};
use std::cmp::{max, Ordering};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, RwLock};
use zstd::bulk;

#[derive(Debug, Clone, Default)]
struct ZenLegacyPackageExternalArcFixupData {
    fixup_from_bundle_id: i32,
    from_package_id: FPackageId,
    from_import_index: FPackageObjectIndex,
    from_command_type: EExportCommandType,
    debug_full_import_name: Option<String>,
}
#[derive(Debug, Clone, Default)]
struct ZenLegacyPackageExportBundleMapping {
    export_index: FPackageObjectIndex,
    export_command_type: EExportCommandType,
    export_bundle_index: i32,
    debug_full_export_name: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct MaterialSlotTagData {
    slot_name: String,
    tag_names: Vec<String>,
}

fn find_skeletal_mesh_export(legacy_package: &FLegacyPackageHeader) -> Option<usize> {
    for (export_idx, export) in legacy_package.exports.iter().enumerate() {
        let class_index = export.class_index;
        if class_index.is_import() {
            let import_idx = class_index.to_import_index() as usize;
            if import_idx < legacy_package.imports.len() {
                let import = &legacy_package.imports[import_idx];
                let class_name = legacy_package.name_map.get(import.object_name).to_string();
                if class_name == "SkeletalMesh" {
                    return Some(export_idx);
                }
            }
        }
    }
    None
}

fn find_material_tag_user_data_export(legacy_package: &FLegacyPackageHeader) -> Option<usize> {
    for (export_idx, export) in legacy_package.exports.iter().enumerate() {
        let object_name = legacy_package.name_map.get(export.object_name);
        // to-legacy remaps the removed MaterialTagAssetUserData class to AssetUserData,
        // but deliberately preserves the export object name. Use that stable name when
        // repacking so the slot tags survive a Retoc round trip.
        if object_name == "MaterialTagAssetUserData"
            || object_name.starts_with("MaterialTagAssetUserData_")
        {
            return Some(export_idx);
        }

        let class_index = export.class_index;
        if class_index.is_import() {
            let import_idx = class_index.to_import_index() as usize;
            if import_idx < legacy_package.imports.len() {
                let import = &legacy_package.imports[import_idx];
                let class_name = legacy_package.name_map.get(import.object_name).to_string();
                if class_name == "MaterialTagAssetUserData" {
                    return Some(export_idx);
                }
            }
        }
    }
    None
}

fn parse_name_from_fname(
    data: &[u8],
    name_map: &FPackageNameMap,
    offset: &mut usize,
) -> Option<String> {
    if data.len() < 4 {
        return None;
    }
    let index = i32::from_le_bytes([
        data[*offset],
        data[*offset + 1],
        data[*offset + 2],
        data[*offset + 3],
    ]) as usize;
    *offset += 4;
    if index > 0 && (index as usize) < name_map.get_all_names().len() {
        Some(name_map.get_all_names()[index as usize].clone())
    } else {
        None
    }
}

fn parse_material_slot_tags_from_binary(
    export_data: &[u8],
    name_map: &FPackageNameMap,
    is_ue5: bool,
    package_name: &str,
) -> Vec<MaterialSlotTagData> {
    if let Some(scanned) = scan_material_slot_tags_from_export(export_data, name_map, package_name)
    {
        return scanned;
    }

    let mut result = Vec::new();
    let data_len = export_data.len();
    let all_names = name_map.get_all_names();

    let mut offset = 0;
    let mut scanned_props = 0;

    let has_material_slot_tags = all_names.iter().any(|n| n.contains("MaterialSlotTags"));
    emit_log(&format!(
        "[MaterialTags] {} - Name map has 'MaterialSlotTags': {}",
        package_name, has_material_slot_tags
    ));
    if has_material_slot_tags {
        for (i, name) in all_names.iter().enumerate() {
            if name.contains("MaterialSlot") {
                emit_log(&format!(
                    "[MaterialTags] {} - Name map[{}] = '{}'",
                    package_name, i, name
                ));
            }
        }
    }

    while offset + 12 < data_len {
        let name_idx = i32::from_le_bytes([
            export_data[offset],
            export_data[offset + 1],
            export_data[offset + 2],
            export_data[offset + 3],
        ]) as i32;
        offset += 4;

        if name_idx <= 0 || (name_idx as usize) >= all_names.len() {
            break;
        }

        let prop_name = all_names[name_idx as usize].clone();

        if scanned_props < 3 {
            emit_log(&format!(
                "[MaterialTags] {} - Scanning property[{}]: '{}' at offset {}",
                package_name,
                scanned_props,
                prop_name,
                offset - 4
            ));
            scanned_props += 1;
        }

        if prop_name.contains("MaterialSlot") {
            emit_log(&format!(
                "[MaterialTags] {} - Found MaterialSlotTags at offset {}",
                package_name,
                offset - 4
            ));

            if offset + 4 > data_len {
                break;
            }

            let value_size = i32::from_le_bytes([
                export_data[offset],
                export_data[offset + 1],
                export_data[offset + 2],
                export_data[offset + 3],
            ]) as i32;
            offset += 4;

            if is_ue5 && offset + 8 <= data_len {
                let type_idx = i32::from_le_bytes([
                    export_data[offset],
                    export_data[offset + 1],
                    export_data[offset + 2],
                    export_data[offset + 3],
                ]);
                offset += 4;
                let struct_idx = i32::from_le_bytes([
                    export_data[offset],
                    export_data[offset + 1],
                    export_data[offset + 2],
                    export_data[offset + 3],
                ]);
                offset += 4;
                emit_log(&format!(
                    "[MaterialTags] {} - Type: {}, Struct: {}",
                    package_name, type_idx, struct_idx
                ));
            }

            if offset + 4 > data_len {
                break;
            }

            let array_len = i32::from_le_bytes([
                export_data[offset],
                export_data[offset + 1],
                export_data[offset + 2],
                export_data[offset + 3],
            ]) as usize;
            offset += 4;

            if !is_ue5 && offset + 4 <= data_len {
                let inner_name_idx = i32::from_le_bytes([
                    export_data[offset],
                    export_data[offset + 1],
                    export_data[offset + 2],
                    export_data[offset + 3],
                ]) as i32;
                offset += 4;
            }

            emit_log(&format!(
                "[MaterialTags] {} - Array has {} elements",
                package_name, array_len
            ));

            for _ in 0..array_len {
                if offset + 8 > data_len {
                    break;
                }

                let slot_name_idx = i32::from_le_bytes([
                    export_data[offset],
                    export_data[offset + 1],
                    export_data[offset + 2],
                    export_data[offset + 3],
                ]) as i32;
                offset += 4;

                let struct_name_idx = i32::from_le_bytes([
                    export_data[offset],
                    export_data[offset + 1],
                    export_data[offset + 2],
                    export_data[offset + 3],
                ]) as i32;
                offset += 4;

                let slot_name = if slot_name_idx > 0 && (slot_name_idx as usize) < all_names.len() {
                    all_names[slot_name_idx as usize].clone()
                } else {
                    String::new()
                };

                let mut tag_names = Vec::new();

                while offset + 4 <= data_len {
                    let next_name_idx = i32::from_le_bytes([
                        export_data[offset],
                        export_data[offset + 1],
                        export_data[offset + 2],
                        export_data[offset + 3],
                    ]) as i32;

                    if next_name_idx <= 0 || (next_name_idx as usize) >= all_names.len() {
                        break;
                    }

                    let next_prop_name = all_names[next_name_idx as usize].clone();

                    if next_prop_name == "GameplayTags" || next_prop_name.is_empty() {
                        if next_prop_name == "GameplayTags" {
                            offset += 4;

                            if offset + 4 > data_len {
                                break;
                            }
                            let tag_array_len = i32::from_le_bytes([
                                export_data[offset],
                                export_data[offset + 1],
                                export_data[offset + 2],
                                export_data[offset + 3],
                            ]) as usize;
                            offset += 4;

                            for _ in 0..tag_array_len {
                                if offset + 4 > data_len {
                                    break;
                                }
                                let tag_name_idx = i32::from_le_bytes([
                                    export_data[offset],
                                    export_data[offset + 1],
                                    export_data[offset + 2],
                                    export_data[offset + 3],
                                ]) as i32;
                                offset += 4;

                                if tag_name_idx > 0 && (tag_name_idx as usize) < all_names.len() {
                                    tag_names.push(all_names[tag_name_idx as usize].clone());
                                }
                            }
                        }
                        break;
                    }

                    if !slot_name.is_empty() {
                        result.push(MaterialSlotTagData {
                            slot_name: slot_name.clone(),
                            tag_names: tag_names.clone(),
                        });
                    }
                }
                break;
            }
        }

        if offset + 12 > data_len {
            break;
        }

        let value_size = i32::from_le_bytes([
            export_data[offset],
            export_data[offset + 1],
            export_data[offset + 2],
            export_data[offset + 3],
        ]) as i32;
        offset += 4;

        let _struct_name_idx = i32::from_le_bytes([
            export_data[offset],
            export_data[offset + 1],
            export_data[offset + 2],
            export_data[offset + 3],
        ]) as i32;
        offset += 4;

        let _prop_type_idx = i32::from_le_bytes([
            export_data[offset],
            export_data[offset + 1],
            export_data[offset + 2],
            export_data[offset + 3],
        ]) as i32;
        offset += 4;

        offset += value_size as usize;

        if value_size <= 0 {
            break;
        }
    }

    result
}

fn read_name_string_at(data: &[u8], name_map: &FPackageNameMap, offset: usize) -> Option<String> {
    let index = read_i32_at(data, offset)?;
    let number = read_i32_at(data, offset + 4)?;
    if index < 0 || index as usize >= name_map.get_all_names().len() {
        return None;
    }

    let bare_name = &name_map.get_all_names()[index as usize];
    if number != 0 {
        Some(format!("{}_{}", bare_name, number - 1))
    } else {
        Some(bare_name.clone())
    }
}

fn scan_material_slot_tags_from_export(
    export_data: &[u8],
    name_map: &FPackageNameMap,
    package_name: &str,
) -> Option<Vec<MaterialSlotTagData>> {
    let all_names = name_map.get_all_names();
    let material_slot_name_idx = all_names
        .iter()
        .position(|n| n.eq_ignore_ascii_case("MaterialSlotName"))?
        as i32;
    let name_property_idx = all_names
        .iter()
        .position(|n| n.eq_ignore_ascii_case("NameProperty"))? as i32;

    let mut slot_property_offsets = Vec::new();
    for offset in 0..export_data.len().saturating_sub(16) {
        if read_i32_at(export_data, offset) == Some(material_slot_name_idx)
            && read_i32_at(export_data, offset + 4) == Some(0)
            && read_i32_at(export_data, offset + 8) == Some(name_property_idx)
            && read_i32_at(export_data, offset + 12) == Some(0)
        {
            slot_property_offsets.push(offset);
        }
    }

    if slot_property_offsets.is_empty() {
        return None;
    }

    let value_offsets = [24usize, 25, 32, 33];
    let mut entries = Vec::new();

    for (slot_idx, slot_property_offset) in slot_property_offsets.iter().copied().enumerate() {
        let next_slot_offset = slot_property_offsets
            .get(slot_idx + 1)
            .copied()
            .unwrap_or(export_data.len());

        let mut slot_name = None;
        for value_offset in value_offsets {
            let candidate_offset = slot_property_offset + value_offset;
            let Some(candidate_name) = read_name_string_at(export_data, name_map, candidate_offset)
            else {
                continue;
            };
            if candidate_name != "None" && !candidate_name.starts_with("MaterialTag.") {
                slot_name = Some(candidate_name);
                break;
            }
        }

        let Some(slot_name) = slot_name else {
            continue;
        };

        let mut tag_names = Vec::new();
        let scan_start = slot_property_offset.saturating_add(24);
        let scan_end = next_slot_offset.min(export_data.len());
        for offset in scan_start..scan_end.saturating_sub(8) {
            let Some(tag_name) = read_name_string_at(export_data, name_map, offset) else {
                continue;
            };
            if tag_name.starts_with("MaterialTag.") && !tag_names.contains(&tag_name) {
                tag_names.push(tag_name);
            }
        }

        entries.push(MaterialSlotTagData {
            slot_name,
            tag_names,
        });
    }

    if entries.is_empty() {
        None
    } else {
        let tagged_slots = entries
            .iter()
            .filter(|entry| !entry.tag_names.is_empty())
            .count();
        let total_tags: usize = entries.iter().map(|entry| entry.tag_names.len()).sum();
        emit_log(&format!(
            "[MaterialTags] {} - Scanned MaterialSlotTags: {} slot(s), {} tagged slot(s), {} tag(s)",
            package_name,
            entries.len(),
            tagged_slots,
            total_tags
        ));
        Some(entries)
    }
}

fn patch_skeletal_mesh_materials(
    export_data: &mut Vec<u8>,
    name_map: &FPackageNameMap,
    tag_data: &[MaterialSlotTagData],
    package_name: &str,
) -> bool {
    let expected_slot_names: Vec<&str> =
        tag_data.iter().map(|tag| tag.slot_name.as_str()).collect();
    let material_array = match find_skeletal_material_array(
        export_data,
        name_map,
        package_name,
        &expected_slot_names,
    ) {
        Some(result) => result,
        None => {
            emit_log(&format!(
                "[MaterialTags] {} - Could not find valid FSkeletalMaterial array",
                package_name
            ));
            return false;
        }
    };

    if material_array.layout == MaterialArrayLayout::PaddedTagged {
        emit_log(&format!(
            "[MaterialTags] {} - Skipping (prepatched, {} material(s))",
            package_name, material_array.count
        ));
        return false;
    }

    let mut new_data = Vec::new();
    let mut total_injected_tags = 0usize;
    let mut tagged_materials = 0usize;
    let source_stride = match material_array.layout {
        MaterialArrayLayout::Legacy => LEGACY_SKELETAL_MATERIAL_SIZE,
        MaterialArrayLayout::PaddedEmpty => EMPTY_TAG_SKELETAL_MATERIAL_SIZE,
        MaterialArrayLayout::PaddedTagged => unreachable!(),
    };
    let materials_end = material_array.offset + material_array.count * source_stride;

    new_data.extend_from_slice(&export_data[..material_array.offset]);

    for material_index in 0..material_array.count {
        let entry_offset = material_array.offset + material_index * source_stride;
        let entry_end = entry_offset + LEGACY_SKELETAL_MATERIAL_SIZE;
        new_data.extend_from_slice(&export_data[entry_offset..entry_end]);

        let slot_name =
            read_name_string_at(export_data, name_map, entry_offset + 4).unwrap_or_default();

        let mut tags_for_slot = Vec::new();
        for tag_info in tag_data
            .iter()
            .filter(|t| t.slot_name.eq_ignore_ascii_case(&slot_name))
        {
            for tag_name in &tag_info.tag_names {
                if let Some(tag_name_idx) =
                    name_map.get_all_names().iter().position(|n| n == tag_name)
                {
                    tags_for_slot.push(tag_name_idx as i32);
                }
            }
        }

        if !tags_for_slot.is_empty() {
            tagged_materials += 1;
            total_injected_tags += tags_for_slot.len();
        }

        new_data.extend_from_slice(&(tags_for_slot.len() as i32).to_le_bytes());
        for tag_name_idx in tags_for_slot {
            new_data.extend_from_slice(&tag_name_idx.to_le_bytes());
            new_data.extend_from_slice(&0i32.to_le_bytes());
        }
    }

    new_data.extend_from_slice(&export_data[materials_end..]);

    let size_diff = new_data.len() as isize - export_data.len() as isize;
    emit_log(&format!(
        "[MaterialTags] {} - Added FGameplayTagContainer to {} material(s), injected {} tag(s) into {} material(s), size change: +{} bytes",
        package_name,
        material_array.count,
        total_injected_tags,
        tagged_materials,
        size_diff
    ));

    *export_data = new_data;
    true
}

const LEGACY_SKELETAL_MATERIAL_SIZE: usize = 40;
const EMPTY_TAG_SKELETAL_MATERIAL_SIZE: usize = 44;
const MAX_SKELETAL_MATERIALS: i32 = 128;
const MAX_MATERIAL_TAGS_PER_SLOT: i32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaterialArrayLayout {
    Legacy,
    PaddedEmpty,
    PaddedTagged,
}

#[derive(Debug, Clone, Copy)]
struct MaterialArrayCandidate {
    offset: usize,
    count: usize,
    layout: MaterialArrayLayout,
    score: usize,
    byte_len: usize,
}

fn read_i32_at(data: &[u8], offset: usize) -> Option<i32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}

fn is_valid_material_entry(data: &[u8], offset: usize, name_count: usize) -> bool {
    let Some(package_index) = read_i32_at(data, offset) else {
        return false;
    };
    if package_index > 0 || package_index < -10_000 {
        return false;
    }

    let Some(slot_name_index) = read_i32_at(data, offset + 4) else {
        return false;
    };
    if slot_name_index < 0 || slot_name_index as usize >= name_count {
        return false;
    }

    let Some(imported_slot_name_index) = read_i32_at(data, offset + 12) else {
        return false;
    };
    if imported_slot_name_index < 0 || imported_slot_name_index as usize >= name_count {
        return false;
    }

    true
}

fn validate_material_array(
    data: &[u8],
    materials_offset: usize,
    count: i32,
    layout: MaterialArrayLayout,
    name_count: usize,
) -> bool {
    if count <= 0 || count > MAX_SKELETAL_MATERIALS {
        return false;
    }

    let stride = match layout {
        MaterialArrayLayout::Legacy => LEGACY_SKELETAL_MATERIAL_SIZE,
        MaterialArrayLayout::PaddedEmpty => EMPTY_TAG_SKELETAL_MATERIAL_SIZE,
        MaterialArrayLayout::PaddedTagged => {
            return validate_tagged_material_array(data, materials_offset, count, name_count)
                .is_some()
        }
    };
    let count = count as usize;
    if materials_offset + count * stride > data.len() {
        return false;
    }

    for material_index in 0..count {
        let entry_offset = materials_offset + material_index * stride;
        if !is_valid_material_entry(data, entry_offset, name_count) {
            return false;
        }

        if layout == MaterialArrayLayout::PaddedEmpty {
            let Some(tag_count) = read_i32_at(data, entry_offset + LEGACY_SKELETAL_MATERIAL_SIZE)
            else {
                return false;
            };
            if tag_count != 0 {
                return false;
            }
        }
    }

    true
}

fn material_array_score(
    data: &[u8],
    materials_offset: usize,
    count: usize,
    layout: MaterialArrayLayout,
    name_map: &FPackageNameMap,
    expected_slot_names: &[&str],
) -> usize {
    match layout {
        MaterialArrayLayout::PaddedTagged => score_tagged_material_array_slots(
            data,
            materials_offset,
            count,
            name_map,
            expected_slot_names,
        ),
        MaterialArrayLayout::Legacy => score_material_array_slots(
            data,
            materials_offset,
            count,
            LEGACY_SKELETAL_MATERIAL_SIZE,
            name_map,
            expected_slot_names,
        ),
        MaterialArrayLayout::PaddedEmpty => score_material_array_slots(
            data,
            materials_offset,
            count,
            EMPTY_TAG_SKELETAL_MATERIAL_SIZE,
            name_map,
            expected_slot_names,
        ),
    }
}

fn first_material_package_index_is_import(data: &[u8], materials_offset: usize) -> bool {
    matches!(
        read_i32_at(data, materials_offset),
        Some(package_index) if package_index < 0 && package_index >= -10_000
    )
}

fn find_first_material_array_by_layout(
    export_data: &[u8],
    name_map: &FPackageNameMap,
    expected_slot_names: &[&str],
    layout: MaterialArrayLayout,
) -> Option<MaterialArrayCandidate> {
    let name_count = name_map.get_all_names().len();
    let stride = match layout {
        MaterialArrayLayout::Legacy => LEGACY_SKELETAL_MATERIAL_SIZE,
        MaterialArrayLayout::PaddedEmpty => EMPTY_TAG_SKELETAL_MATERIAL_SIZE,
        MaterialArrayLayout::PaddedTagged => EMPTY_TAG_SKELETAL_MATERIAL_SIZE,
    };
    if export_data.len() < 4 + stride {
        return None;
    }

    let max_count_offset = export_data.len() - 4 - stride;
    for count_offset in 4..=max_count_offset {
        let Some(count) = read_i32_at(export_data, count_offset) else {
            continue;
        };
        if count <= 0 || count > MAX_SKELETAL_MATERIALS {
            continue;
        }

        let materials_offset = count_offset + 4;
        if !first_material_package_index_is_import(export_data, materials_offset) {
            continue;
        }

        let byte_len = if layout == MaterialArrayLayout::PaddedTagged {
            let Some(byte_len) =
                validate_tagged_material_array(export_data, materials_offset, count, name_count)
            else {
                continue;
            };
            byte_len
        } else {
            if !validate_material_array(export_data, materials_offset, count, layout, name_count) {
                continue;
            }
            count as usize * stride
        };

        let score = material_array_score(
            export_data,
            materials_offset,
            count as usize,
            layout,
            name_map,
            expected_slot_names,
        );
        if !expected_slot_names.is_empty() && score == 0 {
            continue;
        }

        return Some(MaterialArrayCandidate {
            offset: materials_offset,
            count: count as usize,
            layout,
            score,
            byte_len,
        });
    }

    None
}

fn validate_tagged_material_array(
    data: &[u8],
    materials_offset: usize,
    count: i32,
    name_count: usize,
) -> Option<usize> {
    if count <= 0 || count > MAX_SKELETAL_MATERIALS {
        return None;
    }

    let mut cursor = materials_offset;
    for _ in 0..count {
        if cursor + EMPTY_TAG_SKELETAL_MATERIAL_SIZE > data.len() {
            return None;
        }

        if !is_valid_material_entry(data, cursor, name_count) {
            return None;
        }

        let tag_count = read_i32_at(data, cursor + LEGACY_SKELETAL_MATERIAL_SIZE)?;
        if tag_count < 0 || tag_count > MAX_MATERIAL_TAGS_PER_SLOT {
            return None;
        }

        let tags_offset = cursor + EMPTY_TAG_SKELETAL_MATERIAL_SIZE;
        let tags_size = tag_count as usize * 8;
        if tags_offset + tags_size > data.len() {
            return None;
        }

        for tag_index in 0..tag_count as usize {
            let tag_offset = tags_offset + tag_index * 8;
            let name_index = read_i32_at(data, tag_offset)?;
            let number = read_i32_at(data, tag_offset + 4)?;
            if name_index < 0 || name_index as usize >= name_count || number < 0 {
                return None;
            }
        }

        cursor = tags_offset + tags_size;
    }

    Some(cursor - materials_offset)
}

fn score_material_array_slots(
    data: &[u8],
    materials_offset: usize,
    count: usize,
    stride: usize,
    name_map: &FPackageNameMap,
    expected_slot_names: &[&str],
) -> usize {
    if expected_slot_names.is_empty() {
        return 0;
    }

    let mut score = 0usize;
    for material_index in 0..count {
        let entry_offset = materials_offset + material_index * stride;
        let Some(slot_name) = read_name_string_at(data, name_map, entry_offset + 4) else {
            continue;
        };
        if expected_slot_names
            .iter()
            .any(|expected| expected.eq_ignore_ascii_case(&slot_name))
        {
            score += 1;
        }
    }
    score
}

fn score_tagged_material_array_slots(
    data: &[u8],
    materials_offset: usize,
    count: usize,
    name_map: &FPackageNameMap,
    expected_slot_names: &[&str],
) -> usize {
    if expected_slot_names.is_empty() {
        return 0;
    }

    let mut cursor = materials_offset;
    let mut score = 0usize;
    for _ in 0..count {
        let Some(slot_name) = read_name_string_at(data, name_map, cursor + 4) else {
            break;
        };
        if expected_slot_names
            .iter()
            .any(|expected| expected.eq_ignore_ascii_case(&slot_name))
        {
            score += 1;
        }

        let Some(tag_count) = read_i32_at(data, cursor + LEGACY_SKELETAL_MATERIAL_SIZE) else {
            break;
        };
        if tag_count < 0 || tag_count > MAX_MATERIAL_TAGS_PER_SLOT {
            break;
        }

        cursor += EMPTY_TAG_SKELETAL_MATERIAL_SIZE + tag_count as usize * 8;
    }
    score
}

fn find_skeletal_material_array(
    export_data: &[u8],
    name_map: &FPackageNameMap,
    package_name: &str,
    expected_slot_names: &[&str],
) -> Option<MaterialArrayCandidate> {
    if let Some(candidate) = find_first_material_array_by_layout(
        export_data,
        name_map,
        expected_slot_names,
        MaterialArrayLayout::PaddedTagged,
    ) {
        let has_existing_tags =
            candidate.byte_len > candidate.count * EMPTY_TAG_SKELETAL_MATERIAL_SIZE;
        if has_existing_tags {
            emit_log(&format!(
                "[MaterialTags] {} - Found prepatched tagged FSkeletalMaterial array at {:#X}: {} material(s), matched {} slot(s)",
                package_name, candidate.offset, candidate.count, candidate.score
            ));
            return Some(candidate);
        }
    }

    if let Some(candidate) = find_first_material_array_by_layout(
        export_data,
        name_map,
        expected_slot_names,
        MaterialArrayLayout::PaddedEmpty,
    ) {
        emit_log(&format!(
            "[MaterialTags] {} - Found prepatched FSkeletalMaterial array at {:#X}: {} material(s), matched {} slot(s)",
            package_name, candidate.offset, candidate.count, candidate.score
        ));
        return Some(candidate);
    }

    if let Some(candidate) = find_first_material_array_by_layout(
        export_data,
        name_map,
        expected_slot_names,
        MaterialArrayLayout::Legacy,
    ) {
        emit_log(&format!(
            "[MaterialTags] {} - Found legacy FSkeletalMaterial array at {:#X}: {} material(s), matched {} slot(s)",
            package_name, candidate.offset, candidate.count, candidate.score
        ));
        return Some(candidate);
    }

    None
}

fn patch_material_tags(builder: &mut ZenPackageBuilder) -> bool {
    let package_name = &builder.legacy_package.summary.package_name;
    let name_map = &builder.legacy_package.name_map;
    let is_ue5 = builder
        .legacy_package
        .summary
        .versioning_info
        .package_file_version
        .file_version_ue5
        != 0;

    let skeletal_mesh_idx = match find_skeletal_mesh_export(&builder.legacy_package) {
        Some(idx) => idx,
        None => {
            return false;
        }
    };

    let mesh_export = &builder.legacy_package.exports[skeletal_mesh_idx];
    let mesh_offset = mesh_export.serial_offset as usize
        - builder.legacy_package.summary.total_header_size as usize;
    let mesh_size = mesh_export.serial_size as usize;

    if mesh_offset + mesh_size > builder.exports_file_buffer.len() {
        emit_log(&format!(
            "[MaterialTags] {} - SkeletalMesh export out of bounds",
            package_name
        ));
        return false;
    }

    let mut tag_data: Vec<MaterialSlotTagData> = Vec::new();

    if let Some(tag_user_data_idx) = find_material_tag_user_data_export(&builder.legacy_package) {
        let tag_export = &builder.legacy_package.exports[tag_user_data_idx];
        let tag_offset = tag_export.serial_offset as usize
            - builder.legacy_package.summary.total_header_size as usize;
        let tag_size = tag_export.serial_size as usize;

        if tag_offset + tag_size <= builder.exports_file_buffer.len() {
            let tag_export_data = &builder.exports_file_buffer[tag_offset..tag_offset + tag_size];
            tag_data = parse_material_slot_tags_from_binary(
                tag_export_data,
                name_map,
                is_ue5,
                package_name,
            );

            if !tag_data.is_empty() {
                let total_tags: usize = tag_data.iter().map(|t| t.tag_names.len()).sum();
                emit_log(&format!(
                    "[MaterialTags] {} - Found {} tag(s) across {} slot(s)",
                    package_name,
                    total_tags,
                    tag_data.len()
                ));
            }
        }
    }

    if tag_data.is_empty() {
        emit_log(&format!(
            "[MaterialTags] {} - Will patch with null containers (no tags found)",
            package_name
        ));
    }

    let mut mesh_export_data_mut =
        builder.exports_file_buffer[mesh_offset..mesh_offset + mesh_size].to_vec();

    if patch_skeletal_mesh_materials(&mut mesh_export_data_mut, name_map, &tag_data, package_name) {
        let size_diff = mesh_export_data_mut.len() as isize - mesh_size as isize;
        builder
            .exports_file_buffer
            .splice(mesh_offset..mesh_offset + mesh_size, mesh_export_data_mut);

        let mesh_export_serial_offset =
            builder.legacy_package.exports[skeletal_mesh_idx].serial_offset;
        builder.legacy_package.exports[skeletal_mesh_idx].serial_size += size_diff as i64;

        for exp in builder.legacy_package.exports.iter_mut() {
            if exp.serial_offset > mesh_export_serial_offset {
                exp.serial_offset += size_diff as i64;
            }
        }

        emit_log(&format!(
            "[MaterialTags] {} - Patched, size change: +{} bytes",
            package_name, size_diff
        ));
        return true;
    }

    false
}

struct ZenPackageBuilder {
    legacy_package: FLegacyPackageHeader,
    package_id: FPackageId,
    zen_package: FZenPackageHeader,
    container_header_version: EIoContainerHeaderVersion,
    package_import_lookup: HashMap<FPackageId, u32>,
    import_to_package_id_lookup: HashMap<FPackageObjectIndex, FPackageId>,
    export_hash_lookup: HashMap<u64, u32>,
    // If this is a localized package, name of the culture for which this package is localized
    localized_package_culture: Option<String>,
    // If this package is a redirect target from another package (including by localization), this is the name of the original package that should get redirected to this package
    source_package_name: Option<String>,
    // True if we should write placeholder legacy external arc values for UE4 external arcs. We will then need a fix-up pass after the initial serialization to fix them up with real from bundle indices
    fixup_legacy_external_arcs: bool,
    // Information necessary for the fixup of the legacy external dependency arcs
    legacy_external_arc_fixup_data: Vec<ZenLegacyPackageExternalArcFixupData>,
    legacy_external_arc_counter: i32,
    legacy_export_bundle_mapping: Vec<ZenLegacyPackageExportBundleMapping>,
    // full names of package objects by their index, useful for debugging
    debug_full_package_object_names: HashMap<FPackageIndex, String>,
    // Export binary data for patching
    exports_file_buffer: Vec<u8>,
}

// Flow is create_asset_builder -> setup_zen_package_summary -> build_zen_import_map -> build_zen_export_map -> build_zen_preload_dependencies -> serialize_zen_asset
fn create_asset_builder(
    package: FLegacyPackageHeader,
    container_header_version: EIoContainerHeaderVersion,
    fixup_legacy_external_arcs: bool,
    exports_buffer: Vec<u8>,
) -> ZenPackageBuilder {
    ZenPackageBuilder {
        package_id: FPackageId::from_name(&package.summary.package_name),
        legacy_package: package,
        zen_package: FZenPackageHeader {
            container_header_version,
            ..FZenPackageHeader::default()
        },
        container_header_version,
        package_import_lookup: HashMap::new(),
        import_to_package_id_lookup: HashMap::new(),
        export_hash_lookup: HashMap::new(),
        localized_package_culture: None,
        source_package_name: None,
        fixup_legacy_external_arcs,
        legacy_external_arc_fixup_data: Vec::new(),
        legacy_external_arc_counter: 0,
        legacy_export_bundle_mapping: Vec::new(),
        debug_full_package_object_names: HashMap::new(),
        exports_file_buffer: exports_buffer,
    }
}

fn setup_zen_package_summary(
    builder: &mut ZenPackageBuilder,
    ubulk_size: Option<usize>,
) -> anyhow::Result<()> {
    let is_unversioned = builder
        .legacy_package
        .summary
        .versioning_info
        .is_unversioned;

    // Copy package flags
    builder.zen_package.summary.package_flags = builder.legacy_package.summary.package_flags;

    // Copy versioning info from the package, except the zen version, which is derived from package file version
    let zen_version: EZenPackageVersion = heuristic_zen_version_from_package_file_version(
        builder
            .legacy_package
            .summary
            .versioning_info
            .package_file_version,
        builder.container_header_version,
    );

    builder.zen_package.is_unversioned = is_unversioned;
    builder.zen_package.versioning_info = FZenPackageVersioningInfo {
        zen_version,
        package_file_version: builder
            .legacy_package
            .summary
            .versioning_info
            .package_file_version,
        licensee_version: builder
            .legacy_package
            .summary
            .versioning_info
            .licensee_version,
        custom_versions: builder
            .legacy_package
            .summary
            .versioning_info
            .custom_versions
            .clone(),
    };

    // Copy name map from the cooked package up to the number of names referenced by exports
    // We do not actually need the rest of the name map
    let name_map_size = builder
        .legacy_package
        .summary
        .names_referenced_from_export_data_count as usize;
    let name_map_slice =
        builder.legacy_package.name_map.copy_raw_names()[0..name_map_size].to_vec();
    builder.zen_package.name_map =
        FNameMap::create_from_names(EMappedNameType::Package, name_map_slice);

    // Make sure not to attempt to put uncooked packages into zen
    // PKG_Cooked is only present in UE5.0+ packages. For earlier versions, check for FilterEditorOnlyData instead
    if builder
        .legacy_package
        .summary
        .versioning_info
        .package_file_version
        .is_ue5()
    {
        if (builder.legacy_package.summary.package_flags & (EPackageFlags::Cooked as u32)) == 0 {
            bail!("Detected absent PKG_Cooked flag in legacy package summary. Uncooked assets cannot be converted to Zen. Are you sure the asset has been Cooked?");
        }
    } else if (builder.legacy_package.summary.package_flags
        & (EPackageFlags::FilterEditorOnly as u32))
        == 0
    {
        bail!("Detected absent PKG_FilterEditorOnly flag in legacy package summary. Assets with editor data cannot be converted to Zen. Are you sure the asset has been Cooked?");
    }

    // Make sure we do not have any soft object paths serialized in the header. These cannot be represented in zen packages and should never be written when cooking
    if builder.legacy_package.summary.soft_object_paths.count > 0 {
        bail!("Detected soft object paths serialized as a part of the package header. Such paths cannot be represented in Zen packages and should never be written for cooked packages. Are you sure the package is cooked?");
    }

    // Set package name on the zen package from the legacy package header
    builder.zen_package.summary.name = builder
        .zen_package
        .name_map
        .store(&builder.legacy_package.summary.package_name);
    // Copy size of the cooked header from the legacy package
    builder.zen_package.summary.cooked_header_size =
        builder.legacy_package.summary.total_header_size as u32;

    // Check if this is a localized package, and track the culture and source package name if it is
    if let Some((source_package_name, culture_name)) =
        convert_localized_package_name_to_source(&builder.legacy_package.summary.package_name)
    {
        // Store source package name and the culture name for which this package is localized
        builder.source_package_name = Some(source_package_name);
        builder.localized_package_culture = Some(culture_name);
    }

    // Setup source package name for the UE4 zen packages. UE5.0+ zen packages do not internally track source package name, it is a part of the container header only
    if builder.container_header_version <= EIoContainerHeaderVersion::Initial {
        // If this package is not a localized package, write None as the source package name. It has to always point to a valid name in the name map
        let source_package_name = builder.source_package_name.as_deref().unwrap_or("None");
        builder.zen_package.summary.source_name =
            builder.zen_package.name_map.store(source_package_name);
    }

    builder.zen_package.bulk_data = match ubulk_size {
        Some(ubulk_size) if !builder.legacy_package.data_resources.is_empty() => {
            // DataResource offsets are relative to the file that owns the payload. Only
            // resources actually stored in .ubulk can be validated against ubulk_size;
            // optional and inline resources point into .uptnl and .uexp respectively.
            const BULKDATA_FORCE_INLINE_PAYLOAD: u32 = 0x40;
            const BULKDATA_PAYLOAD_IN_SEPARATE_FILE: u32 = 0x100;
            const BULKDATA_OPTIONAL_PAYLOAD: u32 = 0x800;
            let entries_valid = builder
                .legacy_package
                .data_resources
                .iter()
                .filter(|r| {
                    r.legacy_bulk_data_flags & BULKDATA_PAYLOAD_IN_SEPARATE_FILE != 0
                        && r.legacy_bulk_data_flags & BULKDATA_OPTIONAL_PAYLOAD == 0
                        && r.legacy_bulk_data_flags & BULKDATA_FORCE_INLINE_PAYLOAD == 0
                })
                .all(|r| r.serial_offset + r.serial_size <= ubulk_size as i64);

            if entries_valid {
                builder
                    .legacy_package
                    .data_resources
                    .iter()
                    .map(|x| FBulkDataMapEntry {
                        serial_offset: x.serial_offset,
                        duplicate_serial_offset: x.duplicate_serial_offset,
                        serial_size: x.serial_size,
                        flags: x.legacy_bulk_data_flags,
                        pad: 0,
                    })
                    .collect()
            } else {
                vec![FBulkDataMapEntry {
                    serial_offset: 0,
                    duplicate_serial_offset: -1,
                    serial_size: ubulk_size as i64,
                    flags: 0x00010501,
                    pad: 0,
                }]
            }
        }
        _ => builder
            .legacy_package
            .data_resources
            .iter()
            .map(|x| FBulkDataMapEntry {
                serial_offset: x.serial_offset,
                duplicate_serial_offset: x.duplicate_serial_offset,
                serial_size: x.serial_size,
                flags: x.legacy_bulk_data_flags,
                pad: 0,
            })
            .collect(),
    };

    // // Copy bulk resources from the legacy package without modifications
    // builder.zen_package.bulk_data = builder
    //     .legacy_package
    //     .data_resources
    //     .iter()
    //     .map(|x| FBulkDataMapEntry {
    //         serial_offset: x.serial_offset,
    //         duplicate_serial_offset: x.duplicate_serial_offset,
    //         serial_size: x.serial_size,
    //         flags: x.legacy_bulk_data_flags,
    //         pad: 0,
    //     })
    //     .collect();
    Ok(())
}

fn resolve_zen_package_import(
    builder: &mut ZenPackageBuilder,
    package_id: FPackageId,
    package_name: &str,
    export_hash: u64,
) -> FPackageImportReference {
    // Resolve index of the imported package, if it's not found add it into the import list and into package names list
    let imported_package_index =
        if let Some(existing_index) = builder.package_import_lookup.get(&package_id) {
            *existing_index
        } else {
            let new_imported_package_index = builder.zen_package.imported_packages.len() as u32;
            builder.zen_package.imported_packages.push(package_id);
            builder
                .zen_package
                .imported_package_names
                .push(package_name.to_string());

            builder
                .package_import_lookup
                .insert(package_id, new_imported_package_index);
            new_imported_package_index
        };

    // Resolve index of the imported export hash
    let imported_public_export_hash_index =
        if let Some(existing_index) = builder.export_hash_lookup.get(&export_hash) {
            *existing_index
        } else {
            let new_imported_export_hash_index =
                builder.zen_package.imported_public_export_hashes.len() as u32;
            builder
                .zen_package
                .imported_public_export_hashes
                .push(export_hash);

            builder
                .export_hash_lookup
                .insert(export_hash, new_imported_export_hash_index);
            new_imported_export_hash_index
        };
    FPackageImportReference {
        imported_package_index,
        imported_public_export_hash_index,
    }
}

// Returns package name and package-relative export path. Package-relative export path is lowercased and is prefixed with /, and uses / as a separator
fn resolve_legacy_package_object(
    package: &ZenPackageBuilder,
    object_index: FPackageIndex,
) -> anyhow::Result<(String, String)> {
    // If this package is a redirect or a localized package, we want to use the name of the source package when resolving exports from it, not it's original name
    // This does not actually matter for UE5.0+ packages because their export hashes are package relative, but for UE4 this is important for being able to resolve references to localized package exports
    let package_name_override = package.source_package_name.as_deref();

    // Zen uses / as path separator, and always lowercases the package relative object path
    Ok(get_package_object_full_name(
        &package.legacy_package,
        object_index,
        '/',
        true,
        package_name_override,
    ))
}

fn convert_legacy_import_to_object_index(
    builder: &mut ZenPackageBuilder,
    import_index: usize,
) -> anyhow::Result<FPackageObjectIndex> {
    let (package_name, full_import_name) =
        resolve_legacy_package_object(builder, FPackageIndex::create_import(import_index as u32))?;

    // MaterialTags: replace /Script/MaterialTagPlugin imports with /Script/Engine equivalents.
    // The game doesn't know about MaterialTagPlugin — we can't strip the export (Extras raw binary
    // has FPackageIndex we can't remap), so instead we remap the class to the engine-native base class AssetUserData.
    let package_name_lower = package_name.to_ascii_lowercase();
    let full_import_name_lower = full_import_name.to_ascii_lowercase();
    if package_name_lower.contains("/materialtagplugin")
        || full_import_name_lower.contains("/materialtagplugin")
        || package_name_lower.contains("/rivalsmeshmaterialmanager")
        || full_import_name_lower.contains("/rivalsmeshmaterialmanager")
    {
        let replaced_path = if full_import_name_lower.contains("default__") {
            "/Script/Engine.Default__AssetUserData"
        } else if full_import_name_lower.contains("materialtagassetuserdata")
            || full_import_name_lower.contains("hiddenmaterialsassetuserdata")
        {
            "/Script/Engine.AssetUserData"
        } else {
            "/Script/Engine"
        };

        emit_log(&format!(
            "[MaterialTags] Remapped import[{}]: {} -> {}",
            import_index, full_import_name, replaced_path
        ));
        return Ok(FPackageObjectIndex::create_script_import(replaced_path));
    }

    // If this is a script import, just resolve it directly using the full import name as an index into script objects
    let is_script_import = package_name.starts_with("/Script/");
    if is_script_import {
        return Ok(FPackageObjectIndex::create_script_import(&full_import_name));
    }

    // If this is a package import (full import name length is the same as package name), emit Null
    // Zen does not preserve Package imports, and they cannot be represented at all in terms of FPackageObjectIndex
    let is_package_import = package_name.len() == full_import_name.len();
    if is_package_import {
        return Ok(FPackageObjectIndex::create_null());
    }
    let package_id = FPackageId::from_name(&package_name);

    // Store the debug mapping of the ID of this import to the full name of it
    builder.debug_full_package_object_names.insert(
        FPackageIndex::create_import(import_index as u32),
        full_import_name.clone(),
    );

    // New style imports with export hashes and package IDs
    let result_package_import =
        if builder.container_header_version > EIoContainerHeaderVersion::Initial {
            // This is a normal import of the export of another package otherwise. Create FPackageId from package ID and public export hash from package relative path
            let public_export_hash =
                get_public_export_hash(&full_import_name[package_name.len() + 1..]);

            // Resolve import reference now, and convert it to object index
            let import_reference =
                resolve_zen_package_import(builder, package_id, &package_name, public_export_hash);
            FPackageObjectIndex::create_package_import(import_reference)
        } else {
            // Old style (UE4.27) imports with full name of the export just converted into FPackageObjectIndex
            let global_import_index =
                FPackageObjectIndex::create_legacy_package_import_from_path(&full_import_name);

            // Note that we still have to track this package ID in our imported packages, even if we are not indexing into it
            if let std::collections::hash_map::Entry::Vacant(e) =
                builder.package_import_lookup.entry(package_id)
            {
                let package_import_index = builder.zen_package.imported_packages.len() as u32;
                builder.zen_package.imported_packages.push(package_id);
                e.insert(package_import_index);
            }
            global_import_index
        };

    // Map the resulting import to the original package ID it came from. This is necessary to resolve legacy UE4 imports into package ID
    builder
        .import_to_package_id_lookup
        .insert(result_package_import, package_id);

    Ok(result_package_import)
}

fn build_zen_import_map(builder: &mut ZenPackageBuilder) -> anyhow::Result<()> {
    builder
        .zen_package
        .import_map
        .reserve(builder.legacy_package.imports.len());

    for import_index in 0..builder.legacy_package.imports.len() {
        let import_object_index = convert_legacy_import_to_object_index(builder, import_index)?;
        builder.zen_package.import_map.push(import_object_index)
    }
    Ok(())
}

fn remap_package_index_reference(
    builder: &mut ZenPackageBuilder,
    package_index: FPackageIndex,
) -> FPackageObjectIndex {
    if package_index.is_export() {
        return FPackageObjectIndex::create_export(package_index.to_export_index());
    }
    if package_index.is_import() {
        return builder.zen_package.import_map[package_index.to_import_index() as usize];
    }
    FPackageObjectIndex::create_null()
}

fn build_zen_export_map(builder: &mut ZenPackageBuilder) -> anyhow::Result<()> {
    builder
        .zen_package
        .export_map
        .reserve(builder.legacy_package.exports.len());

    for export_index in 0..builder.legacy_package.exports.len() {
        let object_export = builder.legacy_package.exports[export_index].clone();
        let total_header_size = builder.legacy_package.summary.total_header_size as u64;
        let object_name = builder
            .legacy_package
            .name_map
            .get(object_export.object_name)
            .to_string();

        // Zen cooked serial offset does not include header size, but legacy asset one does
        let cooked_serial_offset = object_export.serial_offset as u64 - total_header_size;
        let mapped_object_name = builder.zen_package.name_map.store(&object_name);

        let outer_index = remap_package_index_reference(builder, object_export.outer_index);
        let class_index = remap_package_index_reference(builder, object_export.class_index);
        let super_index = remap_package_index_reference(builder, object_export.super_index);
        let template_index = remap_package_index_reference(builder, object_export.template_index);

        let should_have_public_export_hash =
            (object_export.object_flags & EObjectFlags::Public as u32) != 0
                || object_export.generate_public_hash;
        let (export_package_name, full_export_name) = resolve_legacy_package_object(
            builder,
            FPackageIndex::create_export(export_index as u32),
        )?;

        let public_export_hash: u64 = if should_have_public_export_hash {
            // Use global import index converted to the raw representation for legacy packages, and get_public_export_hash otherwise
            if builder.container_header_version > EIoContainerHeaderVersion::Initial {
                get_public_export_hash(&full_export_name[export_package_name.len() + 1..])
            } else {
                FPackageObjectIndex::create_legacy_package_import_from_path(&full_export_name)
                    .to_raw()
            }
        } else {
            0
        };

        let filter_flags: EExportFilterFlags = if object_export.is_not_for_server {
            EExportFilterFlags::NotForServer
        } else if object_export.is_not_for_client {
            EExportFilterFlags::NotForClient
        } else {
            EExportFilterFlags::None
        };

        // Store the debug mapping of the ID of this import to the full name of it
        builder.debug_full_package_object_names.insert(
            FPackageIndex::create_export(export_index as u32),
            full_export_name.clone(),
        );

        let zen_export = FExportMapEntry {
            cooked_serial_offset,
            cooked_serial_size: object_export.serial_size as u64,
            object_name: mapped_object_name,
            object_flags: object_export.object_flags,
            outer_index,
            class_index,
            super_index,
            template_index,
            public_export_hash,
            filter_flags,
            padding: [0; 3],
        };
        builder.zen_package.export_map.push(zen_export);
    }
    Ok(())
}

#[derive(Debug, Copy, Clone, PartialEq, Default, Eq, Hash)]
struct ZenDependencyGraphNode {
    package_index: FPackageIndex,
    command_type: EExportCommandType,
}

fn build_zen_dependency_bundles_legacy(
    builder: &mut ZenPackageBuilder,
    export_load_order: &[ZenExportGraphNode],
    export_dependencies: &HashMap<ZenDependencyGraphNode, Vec<ZenDependencyGraphNode>>,
) {
    let mut current_export_bundle_header_index: i64 = -1;
    let mut current_export_offset: u64 = 0;
    let mut export_to_bundle_map: HashMap<ZenDependencyGraphNode, usize> = HashMap::new();

    // Create export bundles from the export list sorted by the dependencies
    for graph_node in export_load_order {
        let dependency_graph_node = graph_node.node;

        // Skip non-export items in the dependency graph. Imports will occasionally appear in the graph when there is a requirement for both a creation and a serialization
        if !dependency_graph_node.package_index.is_export() {
            continue;
        }

        let export_index = dependency_graph_node.package_index.to_export_index() as usize;
        let export_command_type = dependency_graph_node.command_type;

        // Open a new export bundle if we do not have one running
        if current_export_bundle_header_index == -1 {
            current_export_bundle_header_index =
                builder.zen_package.export_bundle_headers.len() as i64;

            let first_entry_index = builder.zen_package.export_bundle_entries.len() as u32;
            let serial_offset = current_export_offset;

            builder
                .zen_package
                .export_bundle_headers
                .push(FExportBundleHeader {
                    serial_offset,
                    first_entry_index,
                    entry_count: 0,
                })
        }

        // Add current export as an entry into the currently open bundle
        builder
            .zen_package
            .export_bundle_entries
            .push(FExportBundleEntry {
                local_export_index: export_index as u32,
                command_type: export_command_type,
            });
        // Associate this export command with this bundle. This is needed to build internal and external dependency arcs
        export_to_bundle_map.insert(
            dependency_graph_node,
            current_export_bundle_header_index as usize,
        );

        // Increment the entry count for the bundle
        builder.zen_package.export_bundle_headers[current_export_bundle_header_index as usize]
            .entry_count += 1;

        // Account for this export in the current export offset if this export is Serialize command
        if export_command_type == EExportCommandType::Serialize {
            current_export_offset +=
                builder.zen_package.export_map[export_index].cooked_serial_size;
        }
        let is_public_export = builder.zen_package.export_map[export_index].is_public_export();

        // If we perform the fix-up on the serialized package data later, store the information necessary to perform the fixup of another package imports
        if is_public_export
            && builder.fixup_legacy_external_arcs
            && builder.container_header_version <= EIoContainerHeaderVersion::Initial
        {
            let export_global_index =
                builder.zen_package.export_map[export_index].legacy_global_import_index();
            let full_export_name = builder
                .debug_full_package_object_names
                .get(&FPackageIndex::create_export(export_index as u32))
                .cloned();

            builder
                .legacy_export_bundle_mapping
                .push(ZenLegacyPackageExportBundleMapping {
                    export_index: export_global_index,
                    export_command_type,
                    export_bundle_index: current_export_bundle_header_index as i32,
                    debug_full_export_name: full_export_name,
                });
        }

        // Export bundles end at a public export with an export hash. So if this is a public export, close the current bundle
        if is_public_export {
            current_export_bundle_header_index = -1;
        }
    }

    // Used to avoid adding duplicate dependencies between export bundles and other export bundles/imports
    let mut internal_dependency_arcs: HashSet<FInternalDependencyArc> = HashSet::new();
    let mut external_dependency_arcs: HashSet<FExternalDependencyArc> = HashSet::new();
    let mut legacy_dependency_arcs: HashSet<(FPackageId, FInternalDependencyArc)> = HashSet::new();

    // Function to create export dependency arcs to the export's export bundle from another export's export bundle, or from an entry in the import map
    let mut create_dependency_arc_from_node =
        |to_export_bundle_index: i32,
         dependency_node: &ZenDependencyGraphNode,
         mut_builder: &mut ZenPackageBuilder| {
            // This is an export-to-export dependency
            if dependency_node.package_index.is_export() {
                let from_export_bundle_index =
                    *export_to_bundle_map.get(dependency_node).unwrap() as i32;

                // Skip dependencies between exports that belong to the same bundle, they are already sorted
                if from_export_bundle_index != to_export_bundle_index {
                    // If we have not previously created a dependency from that export bundle to our export bundle, add one
                    let internal_dependency_arc = FInternalDependencyArc {
                        from_export_bundle_index,
                        to_export_bundle_index,
                    };
                    if !internal_dependency_arcs.contains(&internal_dependency_arc) {
                        internal_dependency_arcs.insert(internal_dependency_arc);
                        // Note that internal dependency arcs are discarded in UE4.27, and export bundle N always has an implicit internal dependency arc to bundle N-1
                        // Since we create bundles in the export load order, such an implicit ordering works well and does not need to be represented by the internal dependency arc
                        mut_builder
                            .zen_package
                            .internal_dependency_arcs
                            .push(internal_dependency_arc);
                    }
                }
            }
            // This is an import-to-export dependency. We need to add a dependency arc for it unless it's a script import or a removed package import
            else if dependency_node.package_index.is_import() {
                let from_import_index = dependency_node.package_index.to_import_index() as i32;
                let from_command_type = dependency_node.command_type;
                let package_object_import =
                    mut_builder.zen_package.import_map[from_import_index as usize];

                // Do not add external arcs for script imports and removed package imports (represented as Null in the zen import map)
                if package_object_import.kind() == FPackageObjectIndexType::PackageImport {
                    // New graph data will map a specific import to the specific export bundle for UE5.0+ zen assets
                    if mut_builder.container_header_version > EIoContainerHeaderVersion::Initial {
                        let imported_package_index = package_object_import
                            .package_import()
                            .unwrap()
                            .imported_package_index
                            as usize;
                        let external_dependency_arc = FExternalDependencyArc {
                            from_import_index,
                            from_command_type,
                            to_export_bundle_index,
                        };

                        // Only add the dependency arc if we have not previously created in
                        if !external_dependency_arcs.contains(&external_dependency_arc) {
                            external_dependency_arcs.insert(external_dependency_arc);
                            // We lay out external package dependencies to match imported package indices, so this is always safe
                            mut_builder.zen_package.external_package_dependencies
                                [imported_package_index]
                                .external_dependency_arcs
                                .push(external_dependency_arc);
                        }
                    } else {
                        let imported_package_id = *mut_builder
                            .import_to_package_id_lookup
                            .get(&package_object_import)
                            .unwrap();
                        let imported_package_index = *mut_builder
                            .package_import_lookup
                            .get(&imported_package_id)
                            .unwrap() as usize;

                        // Legacy UE4 graph data will only map the export bundle index in this package to export bundle index in the imported package
                        // This requires knowledge of the export bundle layout of another package, which we do not have if fix-up is not possible. So just use -1 as a placeholder
                        // If we are intending to fix up the serialized data later though, write a placeholder value and emit the information necessary for the fixup
                        let from_export_bundle_index: i32 =
                            if mut_builder.fixup_legacy_external_arcs {
                                let current_fixup_id = mut_builder.legacy_external_arc_counter;
                                let full_import_name = mut_builder
                                    .debug_full_package_object_names
                                    .get(&dependency_node.package_index)
                                    .cloned();

                                let fixup_data = ZenLegacyPackageExternalArcFixupData {
                                    fixup_from_bundle_id: current_fixup_id,
                                    from_package_id: imported_package_id,
                                    from_import_index: package_object_import,
                                    from_command_type,
                                    debug_full_import_name: full_import_name,
                                };
                                // Add the fixup data to the hash map and increment the counter, and write current fixup ID as the bundle index
                                mut_builder.legacy_external_arc_fixup_data.push(fixup_data);
                                mut_builder.legacy_external_arc_counter += 1;
                                current_fixup_id
                            } else {
                                -1
                            };

                        // Prevent adding duplicate dependencies on the packages
                        let legacy_dependency_arc = FInternalDependencyArc {
                            from_export_bundle_index,
                            to_export_bundle_index,
                        };
                        if !legacy_dependency_arcs
                            .contains(&(imported_package_id, legacy_dependency_arc))
                        {
                            legacy_dependency_arcs
                                .insert((imported_package_id, legacy_dependency_arc));
                            mut_builder.zen_package.external_package_dependencies
                                [imported_package_index]
                                .legacy_dependency_arcs
                                .push(legacy_dependency_arc);
                        }
                    }
                }
            }
        };

    // Pre-initialize external package dependencies with the number of imported package IDs
    builder
        .zen_package
        .external_package_dependencies
        .reserve(builder.zen_package.imported_packages.len());
    for imported_package_id in &builder.zen_package.imported_packages {
        builder
            .zen_package
            .external_package_dependencies
            .push(ExternalPackageDependency {
                from_package_id: *imported_package_id,
                external_dependency_arcs: Vec::new(),
                legacy_dependency_arcs: Vec::new(),
            });
    }

    // Build internal and external dependency arcs
    for export_index in 0..builder.zen_package.export_map.len() {
        let export_create_node = ZenDependencyGraphNode {
            package_index: FPackageIndex::create_export(export_index as u32),
            command_type: EExportCommandType::Create,
        };
        let export_serialize_node = ZenDependencyGraphNode {
            package_index: FPackageIndex::create_export(export_index as u32),
            command_type: EExportCommandType::Serialize,
        };

        let export_create_bundle_index = *export_to_bundle_map.get(&export_create_node).unwrap();
        let export_serialize_bundle_index =
            *export_to_bundle_map.get(&export_serialize_node).unwrap();

        for export_create_dependency in export_dependencies
            .get(&export_create_node)
            .unwrap_or(&Vec::new())
        {
            create_dependency_arc_from_node(
                export_create_bundle_index as i32,
                export_create_dependency,
                builder,
            );
        }
        for export_serialize_dependency in export_dependencies
            .get(&export_serialize_node)
            .unwrap_or(&Vec::new())
        {
            create_dependency_arc_from_node(
                export_serialize_bundle_index as i32,
                export_serialize_dependency,
                builder,
            );
        }
    }
}

fn build_zen_dependency_bundle_new(
    builder: &mut ZenPackageBuilder,
    export_load_order: &[ZenExportGraphNode],
    export_dependencies: &HashMap<ZenDependencyGraphNode, Vec<ZenDependencyGraphNode>>,
) {
    // Create a single dependency bundle with all exports
    for dependency_graph in export_load_order {
        let dependency_graph_node = dependency_graph.node;

        // Skip non-export items in the dependency graph. Imports will occasionally appear in the graph when there is a requirement for both a creation and a serialization
        if !dependency_graph_node.package_index.is_export() {
            continue;
        }

        let export_index = dependency_graph_node.package_index.to_export_index() as usize;
        let export_command_type = dependency_graph_node.command_type;

        // Add current export as an entry into the currently open bundle
        builder
            .zen_package
            .export_bundle_entries
            .push(FExportBundleEntry {
                local_export_index: export_index as u32,
                command_type: export_command_type,
            });
    }

    // Collects all dependencies of the given node with the given command type
    let collect_export_dependencies = |to_dependency_node: &ZenDependencyGraphNode,
                                       from_command_type: EExportCommandType,
                                       immut_builder: &ZenPackageBuilder|
     -> Vec<FDependencyBundleEntry> {
        let mut result_dependencies: Vec<FDependencyBundleEntry> = Vec::new();

        for from_dependency_node in export_dependencies
            .get(to_dependency_node)
            .unwrap_or(&Vec::new())
        {
            // Skip nodes that do not have the matching command type, and nodes to ourselves (e.g. serialize depends on create)
            if from_dependency_node.command_type == from_command_type
                && from_dependency_node.package_index != to_dependency_node.package_index
            {
                // If this is an export, add the dependency bundle entry at all times
                if from_dependency_node.package_index.is_export() {
                    result_dependencies.push(FDependencyBundleEntry {
                        local_import_or_export_index: from_dependency_node.package_index,
                    });
                }
                // Otherwise, if this is an import, we only add it if it's a package export import
                else if from_dependency_node.package_index.is_import() {
                    let zen_import_package_index = immut_builder.zen_package.import_map
                        [from_dependency_node.package_index.to_import_index() as usize];
                    if zen_import_package_index.kind() == FPackageObjectIndexType::PackageImport {
                        result_dependencies.push(FDependencyBundleEntry {
                            local_import_or_export_index: from_dependency_node.package_index,
                        });
                    }
                }
            }
        }
        result_dependencies
    };

    // Build dependency bundles
    for export_index in 0..builder.zen_package.export_map.len() {
        let export_create_node = ZenDependencyGraphNode {
            package_index: FPackageIndex::create_export(export_index as u32),
            command_type: EExportCommandType::Create,
        };
        let export_serialize_node = ZenDependencyGraphNode {
            package_index: FPackageIndex::create_export(export_index as u32),
            command_type: EExportCommandType::Serialize,
        };

        let mut create_before_create_deps: Vec<FDependencyBundleEntry> =
            collect_export_dependencies(&export_create_node, EExportCommandType::Create, builder);
        let mut serialize_before_create_deps: Vec<FDependencyBundleEntry> =
            collect_export_dependencies(
                &export_create_node,
                EExportCommandType::Serialize,
                builder,
            );
        let mut create_before_serialize_deps: Vec<FDependencyBundleEntry> =
            collect_export_dependencies(
                &export_serialize_node,
                EExportCommandType::Create,
                builder,
            );
        let mut serialize_before_serialize_deps: Vec<FDependencyBundleEntry> =
            collect_export_dependencies(
                &export_serialize_node,
                EExportCommandType::Serialize,
                builder,
            );

        // Create dependency header for this export
        let first_entry_index = builder.zen_package.dependency_bundle_entries.len() as i32;

        builder
            .zen_package
            .dependency_bundle_headers
            .push(FDependencyBundleHeader {
                first_entry_index,
                create_before_create_dependencies: create_before_create_deps.len() as u32,
                serialize_before_create_dependencies: serialize_before_create_deps.len() as u32,
                create_before_serialize_dependencies: create_before_serialize_deps.len() as u32,
                serialize_before_serialize_dependencies: serialize_before_serialize_deps.len()
                    as u32,
            });

        // Push dependency bundle entries into the zen asset following the first index
        builder
            .zen_package
            .dependency_bundle_entries
            .append(&mut create_before_create_deps);
        builder
            .zen_package
            .dependency_bundle_entries
            .append(&mut serialize_before_create_deps);
        builder
            .zen_package
            .dependency_bundle_entries
            .append(&mut create_before_serialize_deps);
        builder
            .zen_package
            .dependency_bundle_entries
            .append(&mut serialize_before_serialize_deps);
    }
}

#[derive(PartialEq, Eq, Copy, Clone, Hash)]
struct ZenExportGraphNode {
    node: ZenDependencyGraphNode,
    is_public_export: bool,
}
impl PartialOrd for ZenExportGraphNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ZenExportGraphNode {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .is_public_export
            .cmp(&self.is_public_export)
            .then(self.node.command_type.cmp(&other.node.command_type))
            .then(
                self.node
                    .package_index
                    .to_export_index()
                    .cmp(&other.node.package_index.to_export_index()),
            )
            .reverse()
    }
}

fn sort_dependencies_in_load_order(
    export_graph_nodes: &Vec<ZenExportGraphNode>,
    dependency_to_dependants: &HashMap<ZenExportGraphNode, Vec<ZenExportGraphNode>>,
) -> anyhow::Result<Vec<ZenExportGraphNode>> {
    let mut incoming_edge_count: HashMap<ZenExportGraphNode, usize> = HashMap::new();

    // Prime all nodes that have dependencies
    for to_nodes in dependency_to_dependants.values() {
        for to_node in to_nodes {
            *incoming_edge_count.entry(*to_node).or_default() += 1;
        }
    }

    // Prime list of nodes that have no dependencies on other nodes
    let mut nodes_with_no_incoming_edges: BinaryHeap<ZenExportGraphNode> =
        BinaryHeap::with_capacity(export_graph_nodes.len());
    for export_node in export_graph_nodes {
        if *incoming_edge_count.entry(*export_node).or_default() == 0 {
            nodes_with_no_incoming_edges.push(*export_node);
        }
    }

    // Take nodes with no dependencies until we run out of them
    let mut load_order: Vec<ZenExportGraphNode> = Vec::with_capacity(export_graph_nodes.len());
    while !nodes_with_no_incoming_edges.is_empty() {
        let removed_node = nodes_with_no_incoming_edges.pop().unwrap();
        load_order.push(removed_node);

        // Remove one edge from all the nodes that depend on this node
        if let Some(node_dependants) = dependency_to_dependants.get(&removed_node) {
            for to_node in node_dependants {
                // Make sure the to node has the edge for this node
                let incoming_edge_count = incoming_edge_count.entry(*to_node).or_default();
                *incoming_edge_count -= 1;

                // If to node no longer has any dependencies that are still unsatisfied, add it to the list of nodes with no incoming edges to be processed later
                if *incoming_edge_count == 0 {
                    nodes_with_no_incoming_edges.push(*to_node);
                }
            }
        }
    }

    // Make sure we actually sorted all the dependencies. If we did not we have a circular dependency on one of the nodes
    if load_order.len() != export_graph_nodes.len() {
        bail!("Failed to sort exports in load order because of circular dependencies");
    }
    Ok(load_order)
}

fn build_zen_preload_dependencies(builder: &mut ZenPackageBuilder) -> anyhow::Result<()> {
    // Build a dependency map with each export and it's preload dependencies
    let export_count = builder.legacy_package.exports.len();
    let mut export_dependencies: HashMap<ZenDependencyGraphNode, Vec<ZenDependencyGraphNode>> =
        HashMap::with_capacity(export_count);
    let mut export_graph_nodes: Vec<ZenExportGraphNode> = Vec::with_capacity(export_count);

    for export_index in 0..export_count {
        let export_package_index = FPackageIndex::create_export(export_index as u32);
        let object_export = builder.legacy_package.exports[export_index].clone();

        let create_graph_node = ZenDependencyGraphNode {
            package_index: export_package_index,
            command_type: EExportCommandType::Create,
        };
        let serialize_graph_node = ZenDependencyGraphNode {
            package_index: export_package_index,
            command_type: EExportCommandType::Serialize,
        };

        let mut create_dependencies: Vec<ZenDependencyGraphNode> = Vec::new();
        let mut serialize_dependencies: Vec<ZenDependencyGraphNode> = Vec::new();

        //This export's serialize has a dependency on this export's create. This dependency is added first because it is added before anything else by the Package Store Optimizer
        serialize_dependencies.push(create_graph_node);

        // Collect create and serialize dependencies for this export
        if object_export.first_export_dependency_index != -1 {
            // Create before create dependencies. They go first because Package Store Optimizer puts them first
            for i in 0..object_export.create_before_create_dependencies {
                let preload_dependency_index = object_export.first_export_dependency_index
                    + object_export.serialize_before_serialize_dependencies
                    + object_export.create_before_serialize_dependencies
                    + object_export.serialize_before_create_dependencies
                    + i;
                let preload_dependency =
                    builder.legacy_package.preload_dependencies[preload_dependency_index as usize];

                let dependency = ZenDependencyGraphNode {
                    package_index: preload_dependency,
                    command_type: EExportCommandType::Create,
                };
                create_dependencies.push(dependency);
            }

            // Serialize before create dependencies. They go second because Package Store Optimizer puts them second
            for i in 0..object_export.serialize_before_create_dependencies {
                let preload_dependency_index = object_export.first_export_dependency_index
                    + object_export.serialize_before_serialize_dependencies
                    + object_export.create_before_serialize_dependencies
                    + i;
                let preload_dependency =
                    builder.legacy_package.preload_dependencies[preload_dependency_index as usize];

                let dependency = ZenDependencyGraphNode {
                    package_index: preload_dependency,
                    command_type: EExportCommandType::Serialize,
                };
                create_dependencies.push(dependency);
            }

            // Create before serialize dependencies. They go third because Package Store Optimizer puts them third
            for i in 0..object_export.create_before_serialize_dependencies {
                let preload_dependency_index = object_export.first_export_dependency_index
                    + object_export.serialize_before_serialize_dependencies
                    + i;
                let preload_dependency =
                    builder.legacy_package.preload_dependencies[preload_dependency_index as usize];

                let dependency = ZenDependencyGraphNode {
                    package_index: preload_dependency,
                    command_type: EExportCommandType::Create,
                };
                serialize_dependencies.push(dependency);
            }

            // Serialize before serialize dependencies. They go last because Package Store Optimizer puts them last
            for i in 0..object_export.serialize_before_serialize_dependencies {
                let preload_dependency_index = object_export.first_export_dependency_index + i;
                let preload_dependency =
                    builder.legacy_package.preload_dependencies[preload_dependency_index as usize];

                let dependency = ZenDependencyGraphNode {
                    package_index: preload_dependency,
                    command_type: EExportCommandType::Serialize,
                };
                serialize_dependencies.push(dependency);
            }
        }

        // Add create and serialize graph nodes for this export
        // Nodes are added into the graph in export order, Create first, then Serialize. So Export0Create -> Export0Serialize -> Export1Create -> Export1Serialize -> etc
        let is_public_export = builder.zen_package.export_map[export_index].is_public_export();
        export_graph_nodes.push(ZenExportGraphNode {
            node: create_graph_node,
            is_public_export,
        });
        export_graph_nodes.push(ZenExportGraphNode {
            node: serialize_graph_node,
            is_public_export,
        });

        // Remember dependencies associated with each node. This is necessary for building dependency arcs later
        export_dependencies.insert(create_graph_node, create_dependencies);
        export_dependencies.insert(serialize_graph_node, serialize_dependencies);
    }

    // Build a reverse lookup from export to exports that depend on it
    let mut dependency_to_dependants: HashMap<ZenExportGraphNode, Vec<ZenExportGraphNode>> =
        HashMap::with_capacity(export_count);
    for dependant_node in &export_graph_nodes {
        if let Some(dependencies) = export_dependencies.get(&dependant_node.node) {
            for raw_dependency_node in dependencies {
                // Skip non-export dependencies from exports. They do not matter for graph building purposes
                if !raw_dependency_node.package_index.is_export() {
                    continue;
                }
                // Determine whenever this export is public or not to create a ZenExportGraphNode
                let is_public_export = builder.zen_package.export_map
                    [raw_dependency_node.package_index.to_export_index() as usize]
                    .is_public_export();

                // Create the dependency node and add the dependant node to it's dependants list
                let dependency_node = ZenExportGraphNode {
                    node: *raw_dependency_node,
                    is_public_export,
                };
                dependency_to_dependants
                    .entry(dependency_node)
                    .or_default()
                    .push(*dependant_node);
            }
        }
    }

    // Sort the export graph nodes in load order
    let sorted_node_list =
        sort_dependencies_in_load_order(&export_graph_nodes, &dependency_to_dependants)?;

    // Use legacy path for versions before NoExportInfo, and a new one for versions after
    if builder.container_header_version >= EIoContainerHeaderVersion::NoExportInfo {
        build_zen_dependency_bundle_new(builder, &sorted_node_list, &export_dependencies);
    } else {
        build_zen_dependency_bundles_legacy(builder, &sorted_node_list, &export_dependencies);
    }
    Ok(())
}

fn write_exports_in_bundle_order<S: Write>(
    writer: &mut S,
    builder: &ZenPackageBuilder,
    exports_buffer: &[u8],
) -> anyhow::Result<()> {
    let total_header_size = builder.legacy_package.summary.total_header_size as u64;
    let mut current_export_offset: u64 = 0;
    let mut largest_exports_buffer_export_end_offset: usize = 0;

    for export_bundle_header_index in 0..builder.zen_package.export_bundle_headers.len() {
        let export_bundle_header =
            builder.zen_package.export_bundle_headers[export_bundle_header_index];

        // Make sure bundle data is actually being placed at the correct offset
        if export_bundle_header.serial_offset != current_export_offset {
            bail!("Export bundle {} serial offset does not match it's actual placement. Expected bundle data to be placed at {}, but it's placed at {}",
            export_bundle_header_index, export_bundle_header.serial_offset, current_export_offset);
        }

        for i in 0..export_bundle_header.entry_count {
            let export_bundle_entry_index = export_bundle_header.first_entry_index + i;
            let export_bundle_entry =
                builder.zen_package.export_bundle_entries[export_bundle_entry_index as usize];

            // Only Serialize command actually means the export data placement
            if export_bundle_entry.command_type == EExportCommandType::Serialize {
                let export_index = export_bundle_entry.local_export_index as usize;

                // Export serial offset here is actually relative to the legacy package header size, so we need to subtract it to get the real position in the exports buffer
                let export_serial_offset = (builder.legacy_package.exports[export_index]
                    .serial_offset as u64
                    - total_header_size) as usize;
                let export_serial_size =
                    builder.legacy_package.exports[export_index].serial_size as usize;
                let export_end_serial_offset = export_serial_offset + export_serial_size;

                // Serialize the export at this position and increment the current position
                largest_exports_buffer_export_end_offset = max(
                    largest_exports_buffer_export_end_offset,
                    export_end_serial_offset,
                );
                writer
                    .write_all(&exports_buffer[export_serial_offset..export_end_serial_offset])?;
                current_export_offset += export_serial_size as u64;
            }
        }
    }

    // There can be extra data after the export blobs in the export buffer that we should try to preserve
    // Note that normally there is also a package end magic there, that we want explicitly NOT to preserve because zen assets before 5.2 do not include end magic
    let extra_data_start_offset = largest_exports_buffer_export_end_offset;
    let mut extra_data_length = exports_buffer.len() - largest_exports_buffer_export_end_offset;

    // Check if last 4 bytes are package file magic, and if they are, do not consider them as extra data
    let package_end_tag_start_offset = exports_buffer.len() - size_of::<u32>();
    if extra_data_length >= size_of::<u32>()
        && Cursor::new(&exports_buffer[package_end_tag_start_offset..]).read_u32::<LE>()?
            == FLegacyPackageFileSummary::PACKAGE_FILE_TAG
    {
        extra_data_length -= size_of::<u32>();
    }
    // If we have any actual extra data, write it to the zen asset
    if extra_data_length > 0 {
        let extra_data_end_offset = extra_data_start_offset + extra_data_length;
        writer.write_all(&exports_buffer[extra_data_start_offset..extra_data_end_offset])?;
    }
    Ok(())
}

fn serialize_zen_asset(
    builder: &ZenPackageBuilder,
    _legacy_asset_bundle: &FSerializedAssetBundle,
) -> anyhow::Result<(StoreEntry, Vec<u8>, Vec<u64>)> {
    let mut result_package_buffer: Vec<u8> = Vec::new();
    let mut result_package_writer = Cursor::new(&mut result_package_buffer);
    let mut result_store_entry: StoreEntry = StoreEntry::default();

    // Serialize package header
    let legacy_external_arcs_serialized_offsets = FZenPackageHeader::serialize(
        &builder.zen_package,
        &mut result_package_writer,
        &mut result_store_entry,
        builder.container_header_version,
    )?;

    // Use the (potentially patched) exports buffer from the builder
    if builder.container_header_version >= EIoContainerHeaderVersion::NoExportInfo {
        // Write export buffer without any changes if we are following cooked offsets
        result_package_writer.write_all(&builder.exports_file_buffer)?;
    } else {
        // Write export buffer in bundle order otherwise, moving exports around to follow bundle serialization order
        write_exports_in_bundle_order(
            &mut result_package_writer,
            builder,
            &builder.exports_file_buffer,
        )?;
    }
    Ok((
        result_store_entry,
        result_package_buffer,
        legacy_external_arcs_serialized_offsets,
    ))
}

fn build_converted_zen_asset(
    builder: &ZenPackageBuilder,
    legacy_asset_bundle: FSerializedAssetBundle,
    path: &UEPath,
    package_name_to_referenced_shader_maps: &HashMap<String, Vec<FSHAHash>>,
) -> anyhow::Result<ConvertedZenAssetBundle> {
    let (mut result_store_entry, result_package_buffer, legacy_external_arc_serialized_offsets) =
        serialize_zen_asset(builder, &legacy_asset_bundle)?;

    // Append shader map hashes to the store entry from the package name to shader maps lookup
    if let Some(referenced_shader_maps) =
        package_name_to_referenced_shader_maps.get(&builder.legacy_package.summary.package_name)
    {
        result_store_entry
            .shader_map_hashes
            .append(&mut referenced_shader_maps.clone());
    }

    Ok(ConvertedZenAssetBundle {
        package_id: builder.package_id,
        package_name: builder.legacy_package.summary.package_name.clone(),
        path: path.into(),
        store_entry: result_store_entry,
        package_buffer: result_package_buffer,
        bulk_data_buffer: legacy_asset_bundle.bulk_data_buffer,
        optional_bulk_data_buffer: legacy_asset_bundle.optional_bulk_data_buffer,
        memory_mapped_bulk_data_buffer: legacy_asset_bundle.memory_mapped_bulk_data_buffer,
        source_package_name: builder.source_package_name.clone(),
        localized_package_culture_name: builder.localized_package_culture.clone(),
        legacy_external_arc_serialized_offsets,
        legacy_external_arc_fixup_data: builder.legacy_external_arc_fixup_data.clone(),
        legacy_export_bundle_mapping_data: builder.legacy_export_bundle_mapping.clone(),
    })
}

pub(crate) struct ConvertedZenAssetBundle {
    pub(crate) package_id: FPackageId,
    pub(crate) package_name: String,
    path: UEPathBuf,
    store_entry: StoreEntry,
    package_buffer: Vec<u8>,
    bulk_data_buffer: Option<Vec<u8>>,
    optional_bulk_data_buffer: Option<Vec<u8>>,
    memory_mapped_bulk_data_buffer: Option<Vec<u8>>,
    source_package_name: Option<String>,
    localized_package_culture_name: Option<String>,
    // Offsets into the package buffer at which legacy external arcs have been serialized. Needed for UE4 external arc fixup that requires knowing the layout of imported assets
    legacy_external_arc_serialized_offsets: Vec<u64>,
    legacy_external_arc_fixup_data: Vec<ZenLegacyPackageExternalArcFixupData>,
    legacy_export_bundle_mapping_data: Vec<ZenLegacyPackageExportBundleMapping>,
}
impl ConvertedZenAssetBundle {
    pub(crate) fn package_data_size(&self) -> usize {
        self.package_buffer.len()
    }
    pub(crate) fn fixup_legacy_external_arcs(
        &mut self,
        global_package_lookup: &HashMap<FPackageId, Arc<RwLock<ConvertedZenAssetBundle>>>,
        log: &Log,
    ) -> anyhow::Result<()> {
        for legacy_serialized_offset in &self.legacy_external_arc_serialized_offsets {
            // Seek to the relevant position and read the ID of the placeholder from bundle index
            let placeholder_from_bundle_index: i32 = {
                let mut package_buffer_reader = Cursor::new(&self.package_buffer);
                package_buffer_reader.seek(SeekFrom::Start(*legacy_serialized_offset))?;
                package_buffer_reader.de()?
            };

            // Resolve the fixup data for this arc
            let fixup_data = self
                .legacy_external_arc_fixup_data
                .iter()
                .find(|x| x.fixup_from_bundle_id == placeholder_from_bundle_index)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "Failed to find fixup data for placeholder ID {}",
                        placeholder_from_bundle_index
                    )
                })?;

            // Attempt to find the package in the lookup to which this import maps
            let result_from_bundle_index: i32 = if let Some(referenced_asset_bundle_lock) =
                global_package_lookup.get(&fixup_data.from_package_id)
            {
                // Resolve the export this reference is mapping to
                let referenced_asset_bundle = referenced_asset_bundle_lock.read().unwrap();
                let export_bundle_mapping = referenced_asset_bundle.legacy_export_bundle_mapping_data.iter()
                    .find(|x| x.export_index == fixup_data.from_import_index && x.export_command_type == fixup_data.from_command_type)
                    .cloned().ok_or_else(|| {
                        dbg!(referenced_asset_bundle.legacy_export_bundle_mapping_data.clone());
                        anyhow!("Failed to find export in the package {} ({}) mapping to the import {} (full name: {}) dependency {:?} in package {} ({})",
                            referenced_asset_bundle.package_name.clone(), referenced_asset_bundle.package_id,
                            fixup_data.from_import_index, fixup_data.debug_full_import_name.clone().unwrap_or(String::from("unknown")), fixup_data.from_command_type,
                            self.package_name.clone(), self.package_id)
                    })?;

                if log.debug_enabled() {
                    log!(log, "Applying fixup to package {} for import of package {} export {} command {:?}. Resolved export bundle for the export: {}",
                        self.package_id.clone(), fixup_data.from_package_id.clone(), fixup_data.from_import_index, fixup_data.from_command_type, export_bundle_mapping.export_bundle_index);
                }

                // We found the export bundle this dependency maps to
                export_bundle_mapping.export_bundle_index
            } else {
                // This import is not found in the global package lookup, so assume it is external and use -1 as a value meaning "last export bundle in the package"
                -1
            };

            // Write the fixed-up from export bundle index to the correct position
            let mut package_buffer_writer = Cursor::new(&mut self.package_buffer);
            package_buffer_writer.seek(SeekFrom::Start(*legacy_serialized_offset))?;
            package_buffer_writer.ser(&result_from_bundle_index)?;
        }
        Ok(())
    }

    // Writes both the package data and the bulk data in one go
    pub(crate) fn write(&mut self, writer: &mut IoStoreWriter) -> anyhow::Result<()> {
        self.write_package_data(writer)?;
        self.write_and_release_bulk_data(writer)?;
        Ok(())
    }

    // Writes package data into the container, and releases the reference to it
    pub(crate) fn write_package_data(&mut self, writer: &mut IoStoreWriter) -> anyhow::Result<()> {
        let package_chunk_id =
            FIoChunkId::from_package_id(self.package_id, 0, EIoChunkType::ExportBundleData);
        writer.write_package_chunk(
            package_chunk_id,
            Some(&self.path),
            &self.package_buffer,
            &self.store_entry,
        )?;

        // Add the localized package entry if this is a localized package
        if let Some(package_culture_name) = &self.localized_package_culture_name {
            writer.add_localized_package(
                package_culture_name,
                self.source_package_name.as_ref().unwrap(),
                self.package_id,
            )?;
        }
        // If this is a redirected package, add the redirect to the redirect map
        else if let Some(source_package_name) = &self.source_package_name {
            writer.add_package_redirect(source_package_name, self.package_id)?;
        }

        self.package_buffer = Vec::new();
        Ok(())
    }

    // Writes bulk data into the container, and releases the reference to it so that it is no longer stored in memory. Needed for two-stage processing of legacy UE4.27 zen assets
    pub(crate) fn write_and_release_bulk_data(
        &mut self,
        writer: &mut IoStoreWriter,
    ) -> anyhow::Result<()> {
        // Write bulk data chunk if it is present
        if let Some(bulk_data_buffer) = &self.bulk_data_buffer {
            let bulk_data_chunk_id =
                FIoChunkId::from_package_id(self.package_id, 0, EIoChunkType::BulkData);
            writer.write_chunk(
                bulk_data_chunk_id,
                Some(&self.path.with_extension("ubulk")),
                bulk_data_buffer,
            )?;
        }
        // Write optional bulk data chunk if it is present
        if let Some(optional_bulk_data_buffer) = &self.optional_bulk_data_buffer {
            let optional_bulk_data_chunk_id =
                FIoChunkId::from_package_id(self.package_id, 0, EIoChunkType::OptionalBulkData);
            writer.write_chunk(
                optional_bulk_data_chunk_id,
                Some(&self.path.with_extension("uptnl")),
                optional_bulk_data_buffer,
            )?;
        }
        // Write memory mapped bulk data chunk if it is present
        if let Some(memory_mapped_bulk_data_buffer) = &self.memory_mapped_bulk_data_buffer {
            let memory_mapped_bulk_data_chunk_id =
                FIoChunkId::from_package_id(self.package_id, 0, EIoChunkType::MemoryMappedBulkData);
            writer.write_chunk(
                memory_mapped_bulk_data_chunk_id,
                Some(&self.path.with_extension("m.ubulk")),
                memory_mapped_bulk_data_buffer,
            )?;
        }

        // Release the buffers to free the memory taken by them
        self.bulk_data_buffer = None;
        self.optional_bulk_data_buffer = None;
        self.memory_mapped_bulk_data_buffer = None;
        Ok(())
    }
}

fn build_zen_asset_internal(
    legacy_asset: &FSerializedAssetBundle,
    container_header_version: EIoContainerHeaderVersion,
    package_version_fallback: Option<FPackageFileVersion>,
    fixup_legacy_external_arcs: bool,
) -> anyhow::Result<ZenPackageBuilder> {
    // Read legacy package header
    let mut asset_header_reader = Cursor::new(&legacy_asset.asset_file_buffer);
    let legacy_package_header =
        FLegacyPackageHeader::deserialize(&mut asset_header_reader, package_version_fallback)?;

    // Construct zen asset from the package header
    let mut builder = create_asset_builder(
        legacy_package_header,
        container_header_version,
        fixup_legacy_external_arcs,
        legacy_asset.exports_file_buffer.clone(),
    );

    let bulk_size = legacy_asset
        .bulk_data_buffer
        .clone()
        .unwrap_or(vec![])
        .len();

    let bulk_size = if bulk_size == 0 {
        None
    } else {
        Some(bulk_size)
    };

    // Build zen asset data
    setup_zen_package_summary(&mut builder, bulk_size)?;

    // MaterialTags: patch SkeletalMesh with MaterialTagAssetUserData
    patch_material_tags(&mut builder);

    build_zen_import_map(&mut builder)?;
    build_zen_export_map(&mut builder)?;
    build_zen_preload_dependencies(&mut builder)?;

    Ok(builder)
}

// Builds zen asset and returns the resulting package ID, chunk data buffer, and it's store entry. Zen package conversion does not modify bulk data in any way.
pub(crate) fn build_serialize_zen_asset(
    legacy_asset: &FSerializedAssetBundle,
    container_header_version: EIoContainerHeaderVersion,
    package_version_fallback: Option<FPackageFileVersion>,
) -> anyhow::Result<(FPackageId, StoreEntry, Vec<u8>)> {
    // Do not allow legacy external arc fixup, just emit the asset that does not require fixup immediately using only the information available from this asset
    let builder = build_zen_asset_internal(
        legacy_asset,
        container_header_version,
        package_version_fallback,
        false,
    )?;

    let (store_entry, package_data, _) = serialize_zen_asset(&builder, legacy_asset)?;
    Ok((builder.package_id, store_entry, package_data))
}

// Builds zen asset and writes it into the container using the provided serialized legacy asset and package version
pub(crate) fn build_zen_asset(
    legacy_asset: FSerializedAssetBundle,
    package_name_to_referenced_shader_maps: &HashMap<String, Vec<FSHAHash>>,
    path: &UEPath,
    package_version_fallback: Option<FPackageFileVersion>,
    container_header_version: EIoContainerHeaderVersion,
    allow_fixup: bool,
) -> anyhow::Result<ConvertedZenAssetBundle> {
    // We want to fixup this asset once we have converted all the packages
    let final_allow_fixup =
        container_header_version <= EIoContainerHeaderVersion::Initial && allow_fixup;
    let builder = build_zen_asset_internal(
        &legacy_asset,
        container_header_version,
        package_version_fallback,
        final_allow_fixup,
    )?;

    // Serialize the resulting asset into the container writer
    build_converted_zen_asset(
        &builder,
        legacy_asset,
        path,
        package_name_to_referenced_shader_maps,
    )
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::zen::EUnrealEngineObjectUE5Version;
    use crate::EIoStoreTocVersion;
    use fs_err as fs;

    fn padded_empty_material(package_index: i32, slot_name_index: i32) -> Vec<u8> {
        let mut material = Vec::new();
        material.extend_from_slice(&package_index.to_le_bytes());
        material.extend_from_slice(&slot_name_index.to_le_bytes());
        material.extend_from_slice(&0i32.to_le_bytes());
        material.extend_from_slice(&slot_name_index.to_le_bytes());
        material.extend_from_slice(&0i32.to_le_bytes());
        material.extend_from_slice(&[0u8; 20]);
        material.extend_from_slice(&0i32.to_le_bytes());
        material
    }

    #[test]
    fn injects_tags_into_existing_empty_material_containers() {
        let name_map = FPackageNameMap::create_from_names(vec![
            "None".to_string(),
            "SlotA".to_string(),
            "SlotB".to_string(),
            "MaterialTag.Glasses".to_string(),
        ]);
        let tag_data = vec![
            MaterialSlotTagData {
                slot_name: "SlotA".to_string(),
                tag_names: vec!["MaterialTag.Glasses".to_string()],
            },
            MaterialSlotTagData {
                slot_name: "SlotB".to_string(),
                tag_names: Vec::new(),
            },
        ];

        let mut export_data = vec![0u8; 4];
        export_data.extend_from_slice(&2i32.to_le_bytes());
        export_data.extend_from_slice(&padded_empty_material(-1, 1));
        export_data.extend_from_slice(&padded_empty_material(-2, 2));
        let original_len = export_data.len();

        assert!(patch_skeletal_mesh_materials(
            &mut export_data,
            &name_map,
            &tag_data,
            "/Game/TestMesh",
        ));
        assert_eq!(export_data.len(), original_len + 8);
        assert_eq!(read_i32_at(&export_data, 48), Some(1));
        assert_eq!(read_i32_at(&export_data, 52), Some(3));
        assert_eq!(read_i32_at(&export_data, 56), Some(0));
        assert_eq!(read_i32_at(&export_data, 100), Some(0));
    }

    #[test]
    fn test_zen_asset_identity_conversion() -> anyhow::Result<()> {
        run_test(
            "tests/UE5.4/BP_Table_Lamp.uasset",
            "tests/UE5.4/BP_Table_Lamp.uexp",
            "tests/UE5.4/BP_Table_Lamp.uzenasset",
        )?;

        run_test(
            "tests/UE5.4/Randy.uasset",
            "tests/UE5.4/Randy.uexp",
            "tests/UE5.4/Randy.uzenasset",
        )?;

        Ok(())
    }

    fn run_test(header: &str, exports: &str, original_zen: &str) -> anyhow::Result<()> {
        use pretty_assertions::assert_eq;

        let asset_header_buffer = fs::read(header)?;
        let asset_exports_buffer = fs::read(exports)?;

        let serialized_asset_bundle = FSerializedAssetBundle {
            asset_file_buffer: asset_header_buffer,
            exports_file_buffer: asset_exports_buffer.clone(),
            bulk_data_buffer: None,
            optional_bulk_data_buffer: None,
            memory_mapped_bulk_data_buffer: None,
        };

        // UE5.4, NoExportInfo zen header, OnDemandMetaData TOC version, and PropertyTagCompleteTypeName package file version
        let package_file_version = Some(FPackageFileVersion::create_ue5(
            EUnrealEngineObjectUE5Version::PropertyTagCompleteTypeName,
        ));
        let container_header_version = EIoContainerHeaderVersion::NoExportInfo;
        let container_toc_version = EIoStoreTocVersion::OnDemandMetaData;

        let original_zen_asset = fs::read(original_zen)?;
        let original_zen_asset_package = FZenPackageHeader::deserialize(
            &mut Cursor::new(&original_zen_asset),
            None,
            container_toc_version,
            container_header_version,
            package_file_version,
        )?;

        let (_, _, converted_zen_asset) = build_serialize_zen_asset(
            &serialized_asset_bundle,
            container_header_version,
            package_file_version,
        )?;
        let converted_zen_asset_package = FZenPackageHeader::deserialize(
            &mut Cursor::new(&converted_zen_asset),
            None,
            container_toc_version,
            container_header_version,
            package_file_version,
        )?;

        //dbg!(original_zen_asset_package.clone());
        //dbg!(converted_zen_asset_package.clone());

        // Make sure the header is equal between the original and the converted asset, minus the load order data
        assert_eq!(
            original_zen_asset_package.name_map.copy_raw_names(),
            converted_zen_asset_package.name_map.copy_raw_names()
        );
        assert_eq!(
            original_zen_asset_package.bulk_data.clone(),
            converted_zen_asset_package.bulk_data.clone()
        );
        assert_eq!(
            original_zen_asset_package.imported_package_names.clone(),
            converted_zen_asset_package.imported_package_names.clone()
        );
        assert_eq!(
            original_zen_asset_package.imported_packages.clone(),
            converted_zen_asset_package.imported_packages.clone()
        );
        assert_eq!(
            original_zen_asset_package
                .imported_public_export_hashes
                .clone(),
            converted_zen_asset_package
                .imported_public_export_hashes
                .clone()
        );
        assert_eq!(
            original_zen_asset_package.import_map.clone(),
            converted_zen_asset_package.import_map.clone()
        );
        assert_eq!(
            original_zen_asset_package.export_map.clone(),
            converted_zen_asset_package.export_map.clone()
        );
        assert_eq!(
            original_zen_asset_package.dependency_bundle_headers.clone(),
            converted_zen_asset_package
                .dependency_bundle_headers
                .clone()
        );
        assert_eq!(
            original_zen_asset_package.dependency_bundle_entries.clone(),
            converted_zen_asset_package
                .dependency_bundle_entries
                .clone()
        );
        assert_eq!(
            original_zen_asset_package.export_bundle_entries.clone(),
            converted_zen_asset_package.export_bundle_entries.clone()
        );
        assert_eq!(
            original_zen_asset_package.summary.clone(),
            converted_zen_asset_package.summary.clone()
        );

        // Make sure export blob is identical after the header size. Offsets in export map are relative to the end of the header so if they are correct and this data is correct exports are correct
        assert_eq!(
            original_zen_asset[(original_zen_asset_package.summary.header_size as usize)..]
                .to_vec(),
            asset_exports_buffer.clone(),
            "Uexp file and the original zen asset exports do not match"
        );
        assert_eq!(
            original_zen_asset[(original_zen_asset_package.summary.header_size as usize)..]
                .to_vec(),
            converted_zen_asset[(converted_zen_asset_package.summary.header_size as usize)..]
                .to_vec(),
            "Original zen asset and converted zen asset exports do not match"
        );

        assert_eq!(
            original_zen_asset, converted_zen_asset,
            "Original and converted asset binary equality check failed"
        );
        Ok(())
    }
}
