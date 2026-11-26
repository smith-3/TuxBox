//! Use the FSSM layout at the same root parameter indices the title binds.
use super::{compute::RootSource, compute_table::ComputeTable};
use std::{ops::Range, sync::Arc};
use tuxbox_dxbc::{
    mesh_table::{MeshLayout, shader_resources},
    rootsig::{RootParamKind, RootRange, RootSig, parse_rts0},
};
use windows::Win32::{
    Foundation::{E_INVALIDARG, E_NOTIMPL, E_POINTER},
    Graphics::Direct3D12::*,
};
use windows::core::{Interface, Result};

pub(super) fn reflected(
    device: &ID3D12Device,
    original: &RootSource,
    mesh: &[u8],
    pixel: Option<&[u8]>,
    node: u32,
) -> Result<Option<Arc<ComputeTable>>> {
    let Ok(root) = parse_rts0(original.payload()) else {
        return Ok(None);
    };
    let Some(layout) = MeshLayout::from_shader(mesh, &root) else {
        return Ok(None);
    };
    // The pixel program carries its own table layout too. Where it differs
    // from what the title's root declares, the declaration is wrong for that
    // program: the console never reads it, the desktop does. Only tables
    // visible to every stage are taken: ProjectScarlett shows no other case.
    let pixel_tables: Vec<(usize, Vec<RootRange>)> = pixel
        .and_then(|pixel| MeshLayout::from_pixel_shader(pixel, &root))
        .into_iter()
        .flat_map(|layout| layout.replacements)
        .filter_map(|(index, change)| match change {
            RootParamKind::Table(ranges) if root.params[index].visibility == 0 => Some((index, ranges)),
            _ => None,
        })
        .collect();
    if layout.replacements.is_empty() && pixel_tables.is_empty() {
        return Ok(None);
    }
    let recovered_samplers = pixel.map_or_else(Vec::new, |code| recover_pixel_samplers(code, &root));
    let mesh_rewritten: Vec<usize> = layout.replacements.keys().copied().collect();
    let mut lengths = vec![0; root.params.len()];
    let mut accessed_views = Vec::new();
    for ((_, offset), binding) in &layout.tables {
        let slot = offset / 32;
        let end = slot + 1;
        lengths[binding.parameter] = lengths[binding.parameter].max(end);
        if binding.kind != 2 {
            accessed_views.push((binding.parameter, slot..end));
        }
    }
    // Guest tables the pixel stage reads but the mesh layout rewrote. Each gets
    // an appended pixel-only copy of its original ranges (see `mirrors`).
    let mut mirrored = Vec::new();
    if let Some(pixel) = pixel {
        collect_pixel_accesses(&root, &layout, pixel, &mut lengths, &mut accessed_views, &mut mirrored)?;
    }
    // The pixel stage is translated against the guest root, so it can read any
    // register of a pixel-visible table, not only the ones its guest resource
    // list names. Mirror every pixel-visible table the mesh layout rewrote.
    if pixel.is_some() {
        for (index, param) in root.params.iter().enumerate() {
            let RootParamKind::Table(ranges) = &param.kind else { continue };
            if !layout.replacements.contains_key(&index)
                || !matches!(param.visibility, 0 | 5)
                || mirrored.contains(&index)
            {
                continue;
            }
            mirrored.push(index);
            let target = root.params.len() + mirrored.len() - 1;
            if lengths.len() <= target {
                lengths.resize(target + 1, 0);
            }
            let mut next = 0u64;
            for range in ranges {
                let start = if range.offset_in_table == u32::MAX { next } else { u64::from(range.offset_in_table) };
                next = start + u64::from(range.num);
                let (Ok(start), Ok(end)) = (u32::try_from(start), u32::try_from(next)) else { continue };
                lengths[target] = lengths[target].max(end);
                if matches!(range.range_type, 0 | 1) {
                    accessed_views.push((target, start..end));
                }
            }
        }
    }
    // Such a table gets a pixel-only copy laid out as the pixel program reads it.
    for (guest, ranges) in &pixel_tables {
        if !mirrored.contains(guest) {
            mirrored.push(*guest);
        }
        let position = mirrored.iter().position(|index| index == guest).unwrap_or_default();
        let target = root.params.len() + position;
        if lengths.len() <= target {
            lengths.resize(target + 1, 0);
        }
        for range in ranges {
            let end = range.offset_in_table + 1;
            lengths[target] = lengths[target].max(end);
            if range.range_type != 2 {
                accessed_views.push((target, range.offset_in_table..end));
            }
        }
    }
    let bytes = original.serialized();
    // SAFETY: the deserializer owns the source pointers throughout serialization.
    // Replacement tables stay live until the serialized native root is built.
    unsafe {
        let mut reader = None::<ID3D12VersionedRootSignatureDeserializer>;
        D3D12CreateVersionedRootSignatureDeserializer(
            bytes.as_ptr().cast(),
            bytes.len(),
            &ID3D12VersionedRootSignatureDeserializer::IID,
            (&raw mut reader).cast(),
        )?;
        let reader = reader.ok_or(E_POINTER)?;
        let mut desc = *reader.GetRootSignatureDescAtVersion(D3D_ROOT_SIGNATURE_VERSION_1_1)?;
        let source = desc.Anonymous.Desc_1_1;
        if source.NumParameters as usize != root.params.len() || source.pParameters.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let mut params = std::slice::from_raw_parts(source.pParameters, root.params.len()).to_vec();
        let originals = params.clone();
        let mut range_storage = vec![Vec::new(); params.len()];
        for (index, change) in layout.replacements {
            let parameter = &mut params[index];
            parameter.ShaderVisibility = D3D12_SHADER_VISIBILITY_MESH;
            match change {
                RootParamKind::Table(ranges) => {
                    let table = parameter.Anonymous.DescriptorTable;
                    if table.NumDescriptorRanges == 0 || table.pDescriptorRanges.is_null() {
                        return Err(E_INVALIDARG.into());
                    }
                    let old = std::slice::from_raw_parts(
                        table.pDescriptorRanges,
                        table.NumDescriptorRanges as usize,
                    );
                    let storage = &mut range_storage[index];
                    for range in ranges {
                        let kind = D3D12_DESCRIPTOR_RANGE_TYPE(range.range_type as i32);
                        let mut equivalents = old.iter().filter(|r| r.RangeType == kind);
                        let flags = equivalents.next().map(|r| r.Flags).unwrap_or(
                            D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                                | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE,
                        );
                        if equivalents.any(|r| r.Flags != flags) {
                            return Err(E_NOTIMPL.into());
                        }
                        storage.push(D3D12_DESCRIPTOR_RANGE1 {
                            RangeType: kind,
                            NumDescriptors: 1,
                            BaseShaderRegister: range.base_register,
                            RegisterSpace: range.space,
                            Flags: flags,
                            OffsetInDescriptorsFromTableStart: range.offset_in_table,
                        });
                    }
                    parameter.Anonymous.DescriptorTable = D3D12_ROOT_DESCRIPTOR_TABLE1 {
                        NumDescriptorRanges: storage.len() as u32,
                        pDescriptorRanges: storage.as_ptr(),
                    };
                }
                RootParamKind::Descriptor {
                    register, space, ..
                } => {
                    parameter.Anonymous.Descriptor.ShaderRegister = register;
                    parameter.Anonymous.Descriptor.RegisterSpace = space;
                }
                RootParamKind::Constants {
                    register, space, ..
                } => {
                    parameter.Anonymous.Constants.ShaderRegister = register;
                    parameter.Anonymous.Constants.RegisterSpace = space;
                }
            }
        }
        // The pixel program's layout of each such table. Filled before any
        // pointer into it is taken.
        let mut pixel_storage: Vec<(usize, Vec<D3D12_DESCRIPTOR_RANGE1>)> = Vec::new();
        for (guest, ranges) in &pixel_tables {
            let source = originals[*guest];
            if source.ParameterType != D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE {
                return Err(E_INVALIDARG.into());
            }
            let table = source.Anonymous.DescriptorTable;
            let old = if table.NumDescriptorRanges == 0 || table.pDescriptorRanges.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(table.pDescriptorRanges, table.NumDescriptorRanges as usize)
            };
            let storage = ranges
                .iter()
                .map(|range| {
                    let kind = D3D12_DESCRIPTOR_RANGE_TYPE(range.range_type as i32);
                    D3D12_DESCRIPTOR_RANGE1 {
                        RangeType: kind,
                        NumDescriptors: 1,
                        BaseShaderRegister: range.base_register,
                        RegisterSpace: range.space,
                        Flags: old.iter().find(|r| r.RangeType == kind).map(|r| r.Flags).unwrap_or(
                            D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                                | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE,
                        ),
                        OffsetInDescriptorsFromTableStart: range.offset_in_table,
                    }
                })
                .collect();
            pixel_storage.push((*guest, storage));
        }
        let pixel_table = |guest: usize| {
            pixel_storage.iter().find(|(index, _)| *index == guest).map(|(_, ranges)| D3D12_ROOT_DESCRIPTOR_TABLE1 {
                NumDescriptorRanges: ranges.len() as u32,
                pDescriptorRanges: ranges.as_ptr(),
            })
        };
        let mut samplers = Vec::new();
        if !recovered_samplers.is_empty() {
            if source.NumStaticSamplers as usize != root.static_samplers.len() {
                return Err(E_INVALIDARG.into());
            }
            if source.NumStaticSamplers != 0 {
                if source.pStaticSamplers.is_null() { return Err(E_INVALIDARG.into()); }
                samplers.extend_from_slice(std::slice::from_raw_parts(source.pStaticSamplers, source.NumStaticSamplers as usize));
            }
            samplers.extend(recovered_samplers.into_iter().map(super::compute_table::static_sampler));
            desc.Anonymous.Desc_1_1.NumStaticSamplers = samplers.len() as u32;
            desc.Anonymous.Desc_1_1.pStaticSamplers = samplers.as_ptr();
        }
        // The root the pixel program is translated against: the title's, with
        // those tables as the program reads them.
        // A formerly empty root keeps the translation's existing private slots.
        // Appending omitted states to a populated root also changes the compiler input.
        let complete_pixel_root = !root.static_samplers.is_empty() && !samplers.is_empty();
        let pixel_root = if pixel_storage.is_empty() && !complete_pixel_root {
            None
        } else {
            let mut for_pixel = originals.clone();
            for (guest, _) in &pixel_storage {
                if let Some(table) = pixel_table(*guest) {
                    for_pixel[*guest].Anonymous.DescriptorTable = table;
                }
            }
            let mut pixel_desc = desc;
            if !complete_pixel_root {
                pixel_desc.Anonymous.Desc_1_1.NumStaticSamplers = source.NumStaticSamplers;
                pixel_desc.Anonymous.Desc_1_1.pStaticSamplers = source.pStaticSamplers;
            }
            pixel_desc.Anonymous.Desc_1_1.NumParameters = for_pixel.len() as u32;
            pixel_desc.Anonymous.Desc_1_1.pParameters = for_pixel.as_ptr();
            let mut blob = None;
            D3D12SerializeVersionedRootSignature(&pixel_desc, &mut blob, None)?;
            let blob = blob.ok_or(E_POINTER)?;
            Some(RootSource::new(std::slice::from_raw_parts(
                blob.GetBufferPointer().cast(),
                blob.GetBufferSize(),
            )))
        };
        // A table only the pixel program reads differently stays as the title
        // declares it, for the mesh stage: the pixel stage has its own copy.
        for (guest, _) in &pixel_storage {
            if !mesh_rewritten.contains(guest) {
                params[*guest].ShaderVisibility = D3D12_SHADER_VISIBILITY_MESH;
            }
        }
        let mut mirrors = Vec::new();
        for &guest in &mirrored {
            let mut copy = originals[guest];
            if copy.ParameterType != D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE {
                return Err(E_NOTIMPL.into());
            }
            copy.ShaderVisibility = D3D12_SHADER_VISIBILITY_PIXEL;
            if let Some(table) = pixel_table(guest) {
                copy.Anonymous.DescriptorTable = table;
            }
            mirrors.push((params.len() as u32, guest as u32));
            params.push(copy);
        }
        if lengths.len() < params.len() {
            lengths.resize(params.len(), 0);
        }
        desc.Anonymous.Desc_1_1.NumParameters = params.len() as u32;
        desc.Anonymous.Desc_1_1.pParameters = params.as_ptr();
        let mut blob = None;
        D3D12SerializeVersionedRootSignature(&desc, &mut blob, None)?;
        let blob = blob.ok_or(E_POINTER)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize());
        Ok(Some(Arc::new(ComputeTable {
            native: device.CreateRootSignature(node, bytes)?,
            original: original.clone(),
            layout: RootSource::new(bytes),
            lengths,
            accessed_views,
            index_map: None,
            mirrors,
            heap_strides: None,
            pixel_root,
        })))
    }
}

fn recover_pixel_samplers(code: &[u8], root: &RootSig) -> Vec<[u32; 13]> {
    if !root.static_samplers.is_empty() {
        return tuxbox_dxbc::inline_sampler::mesh_pixel_omissions(code, root);
    }
    let unsupported_flags = D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED.0
        | D3D12_ROOT_SIGNATURE_FLAG_DENY_PIXEL_SHADER_ROOT_ACCESS.0;
    let private_table = root.params.iter().any(|p| {
        matches!(p.visibility, 0 | 5) && matches!(&p.kind, RootParamKind::Table(ranges)
            if ranges.iter().any(|r| r.range_type == 3 && r.space == 32765))
    });
    if root.flags & unsupported_flags as u32 != 0 || private_table {
        return Vec::new();
    }
    tuxbox_dxbc::inline_sampler::private(code, 0).unwrap_or_default()
}

fn collect_pixel_accesses(
    root: &RootSig,
    mesh: &MeshLayout,
    pixel: &[u8],
    lengths: &mut Vec<u32>,
    views: &mut Vec<(usize, Range<u32>)>,
    mirrored: &mut Vec<usize>,
) -> Result<()> {
    for [ty, space, low, high] in shader_resources(pixel).ok_or(E_NOTIMPL)? {
        let kind = match ty {
            1 => continue, // sampler tables and static samplers are unchanged
            2 => 2,
            3..=5 | 9 => 0,
            6..=8 => 1,
            _ => return Err(E_NOTIMPL.into()),
        };
        let last = u64::from(high) + 1;
        for (index, param) in root
            .params
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(p.visibility, 0 | 5))
        {
            let rewritten = mesh.replacements.contains_key(&index);
            match &param.kind {
                RootParamKind::Table(ranges) => {
                    let mut next = 0;
                    for range in ranges {
                        let start = if range.offset_in_table == u32::MAX {
                            next
                        } else {
                            u64::from(range.offset_in_table)
                        };
                        next = start.checked_add(u64::from(range.num)).ok_or(E_NOTIMPL)?;
                        if range.range_type != kind || range.space != space {
                            continue;
                        }
                        let base = u64::from(range.base_register);
                        let from = base.max(u64::from(low));
                        let to = (base + u64::from(range.num)).min(last);
                        if from >= to {
                            continue;
                        }
                        // A rewritten table is mesh-only; the pixel stage reads
                        // its appended original copy at `target`.
                        let target = if rewritten {
                            let position = match mirrored.iter().position(|&m| m == index) {
                                Some(position) => position,
                                None => {
                                    mirrored.push(index);
                                    mirrored.len() - 1
                                }
                            };
                            root.params.len() + position
                        } else {
                            index
                        };
                        if lengths.len() <= target {
                            lengths.resize(target + 1, 0);
                        }
                        let start = u32::try_from(start + (from - base)).map_err(|_| E_NOTIMPL)?;
                        let end = start.checked_add((to - from) as u32).ok_or(E_NOTIMPL)?;
                        lengths[target] = lengths[target].max(end);
                        if kind != 2 {
                            views.push((target, start..end));
                        }
                    }
                }
                RootParamKind::Descriptor {
                    kind: descriptor,
                    register,
                    space: owner,
                } if rewritten
                    && *descriptor == 3
                    && kind == 0
                    && *owner == space
                    && (low..=high).contains(register) =>
                {
                    return Err(E_NOTIMPL.into());
                }
                RootParamKind::Constants {
                    register,
                    space: owner,
                    ..
                } if rewritten
                    && kind == 2
                    && *owner == space
                    && (low..=high).contains(register) =>
                {
                    return Err(E_NOTIMPL.into());
                }
                _ => {}
            }
        }
    }
    Ok(())
}
