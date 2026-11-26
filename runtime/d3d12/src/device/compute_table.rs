//! Desktop signatures for shaders whose physical table layout is encoded in FSSM.
use super::compute::RootSource;
use std::sync::Arc;
use tuxbox_dxbc::rootsig::{RootParamKind, parse_rts0};
use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{Interface, Result};

pub(crate) struct ComputeTable {
    pub(crate) native: ID3D12RootSignature,
    pub(crate) original: RootSource,
    pub(crate) layout: RootSource,
    pub(crate) lengths: Vec<u32>,
    pub(crate) accessed_views: Vec<(usize, std::ops::Range<u32>)>,
    pub(crate) index_map: Option<IndexMapBinding>,
    /// LOCAL: appended host parameters that bind the same guest table
    /// (host index, guest slot), e.g. a pixel-only copy of a mesh-rewritten table.
    pub(crate) mirrors: Vec<(u32, u32)>,
    /// The root a mesh pipeline's pixel program is translated against when
    /// that program reads a table at other slots than the title declares.
    pub(crate) pixel_root: Option<RootSource>,
    /// The appended root SRV that receives the resource heap's buffer stride
    /// table (t1, space32766; see `descriptor_paging::strides`).
    pub(crate) heap_strides: Option<u32>,
}

#[derive(Clone, Copy)]
pub(crate) struct IndexMapBinding {
    pub(crate) slot: u32,
    pub(crate) scalar_word: u32,
    pub(crate) scalar_mask: u32,
}

pub(super) enum Program<'a> {
    Compute(&'a [u8]),
    Raster { vertex: &'a [u8], pixel: &'a [u8] },
}

pub(super) enum TableLayout {
    Unsupported,
    Original,
    Reflected(Arc<ComputeTable>),
}

/// The program with an embedded root signature hidden from the binding reader.
/// FSSM records describe the table layout a program was compiled for,
/// whatever root it embeds, and a pipeline root overrides that root when the
/// stage is translated (see `shader_cache::with_root`). Renaming the part keeps
/// every other offset valid.
pub(super) fn layout_view(shader: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let tag = tuxbox_dxbc::Dxbc::parse(shader).ok()
        .and_then(|dxbc| dxbc.chunk(b"RTS0").map(|root| root.offset))
        .and_then(|offset| offset.checked_sub(8))
        .filter(|&tag| shader.get(tag..tag + 4) == Some(b"RTS0"));
    match tag {
        Some(tag) => {
            let mut copy = shader.to_vec();
            copy[tag..tag + 4].copy_from_slice(b"RTSX");
            copy.into()
        }
        None => shader.into(),
    }
}

/// Private static samplers (space 32765) for a program whose literal sampler
/// states are not all among the guest root's static samplers. The console builds
/// a sampler from constant words, so a title can pair a program with a root that
/// lacks its state. The translator binds any static sampler whose state equals
/// the words (`inline_sampler::matches_static`), the guest's own first.
fn missing_literal_samplers(shader: &[u8], stage: u32, existing: &[[u32; 13]]) -> Vec<[u32; 13]> {
    let Some(private) = tuxbox_dxbc::inline_sampler::private(shader, stage) else { return Vec::new() };
    let covered = |p: &[u32; 13]| existing.iter().any(|s| s[..10] == p[..10] && (s[12] == 0 || s[12] == p[12]));
    if private.iter().all(covered) { Vec::new() } else { private }
}

pub(super) fn static_sampler(s: [u32; 13]) -> D3D12_STATIC_SAMPLER_DESC {
    D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER(s[0] as i32),
        AddressU: D3D12_TEXTURE_ADDRESS_MODE(s[1] as i32),
        AddressV: D3D12_TEXTURE_ADDRESS_MODE(s[2] as i32),
        AddressW: D3D12_TEXTURE_ADDRESS_MODE(s[3] as i32),
        MipLODBias: f32::from_bits(s[4]), MaxAnisotropy: s[5],
        ComparisonFunc: D3D12_COMPARISON_FUNC(s[6] as i32), BorderColor: D3D12_STATIC_BORDER_COLOR(s[7] as i32),
        MinLOD: f32::from_bits(s[8]), MaxLOD: f32::from_bits(s[9]), ShaderRegister: s[10], RegisterSpace: s[11],
        ShaderVisibility: D3D12_SHADER_VISIBILITY(s[12] as i32),
    }
}

/// The guest root with the private literal samplers a compute program builds
/// from constant words and the root leaves out. Its parameters are the guest's,
/// so the title's arguments bind unchanged; only the static samplers grow.
/// For roots a reflected table cannot describe (more than two tables).
pub(super) fn sampler_root(
    device: &ID3D12Device,
    original: &RootSource,
    shader: &[u8],
    node: u32,
) -> Result<Option<(ID3D12RootSignature, RootSource)>> {
    let Ok(signature) = parse_rts0(original.payload()) else { return Ok(None) };
    if signature.flags & 0x800 != 0 {
        return Ok(None);
    }
    let recovered = missing_literal_samplers(shader, 5, &signature.static_samplers);
    if recovered.is_empty() {
        return Ok(None);
    }
    let bytes = original.serialized();
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
        let mut samplers = if source.NumStaticSamplers == 0 {
            Vec::new()
        } else {
            if source.pStaticSamplers.is_null() { return Err(E_INVALIDARG.into()); }
            std::slice::from_raw_parts(source.pStaticSamplers, source.NumStaticSamplers as usize).to_vec()
        };
        samplers.extend(recovered.into_iter().map(static_sampler));
        desc.Anonymous.Desc_1_1.pStaticSamplers = samplers.as_ptr();
        desc.Anonymous.Desc_1_1.NumStaticSamplers = samplers.len() as u32;
        let mut blob = None;
        D3D12SerializeVersionedRootSignature(&desc, &mut blob, None)?;
        let blob = blob.ok_or(E_POINTER)?;
        let bytes = std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize());
        Ok(Some((device.CreateRootSignature(node, bytes)?, RootSource::new(bytes))))
    }
}

pub(super) fn reflected(
    device: &ID3D12Device,
    original: &RootSource,
    program: Program<'_>,
    node: u32,
) -> Result<TableLayout> {
    reflected_root(device, original, program, node, false)
}

/// The reflected root with the resource heap's buffer stride table appended,
/// for a program the translator could only express with each slot's stride.
pub(super) fn reflected_with_strides(
    device: &ID3D12Device,
    original: &RootSource,
    program: Program<'_>,
    node: u32,
) -> Result<TableLayout> {
    reflected_root(device, original, program, node, true)
}

fn reflected_root(
    device: &ID3D12Device,
    original: &RootSource,
    program: Program<'_>,
    node: u32,
    strides: bool,
) -> Result<TableLayout> {
    let Ok(signature) = parse_rts0(original.payload()) else {
        return Ok(TableLayout::Unsupported);
    };
    let mut scalar_index = None;
    let mut resolve_constants = None;
    let mut corroborated_raster = false;
    // The user-data layout confirmed every compute binding against this root.
    let mut verified_compute = false;
    let mut recovered_samplers = Vec::new();
    let bindings = match (&program, signature.params.as_slice()) {
        (Program::Compute(shader), params) => {
            let tables = match params {
                [tables @ .., last] if matches!(last.kind, RootParamKind::Constants { num32: 1..=4, .. }) => tables,
                _ => params,
            };
            // Leading tables, then root descriptors and constants that pass
            // through unchanged. Beyond one or two tables with trailing
            // constants, every descriptor must be confirmed by the user data.
            let leading = params.iter().take_while(|p| matches!(p.kind, RootParamKind::Table(_))).count();
            let simple = (1..=2).contains(&tables.len()) && tables.iter().all(|p| matches!(p.kind, RootParamKind::Table(_)));
            let extended = !simple && leading >= 1 && params[leading..].iter()
                .all(|p| matches!(p.kind, RootParamKind::Constants { .. } | RootParamKind::Descriptor { .. }));
            if !simple && !extended {
                return Ok(TableLayout::Unsupported);
            }
            if signature.static_samplers.is_empty() && signature.flags & 0x800 == 0 {
                recovered_samplers = tuxbox_dxbc::inline_sampler::private(shader, 5).unwrap_or_default();
            } else if signature.flags & 0x800 == 0 {
                recovered_samplers = missing_literal_samplers(shader, 5, &signature.static_samplers);
            }
            let checked = tuxbox_dxbc::compute_table::compute_root_bindings(&layout_view(shader), &signature).map(
                |(bindings, constants)| {
                    resolve_constants = constants;
                    verified_compute = true;
                    bindings
                },
            );
            if tables.len() == 2 || extended {
                checked
            } else {
                // One table: bindings the user-data layout cannot confirm keep
                // the program's own reflection, as before.
                checked.or_else(|| tuxbox_dxbc::compute_table::bindings(&layout_view(shader), 5))
            }
        }
        // Several leading tables (each read from s8 + N), then any root descriptors and
        // constants, which pass through unchanged. Every descriptor must be confirmed
        // by the user-data layout of both stages.
        (Program::Raster { vertex, pixel }, params)
            if tuxbox_dxbc::compute_table::leading_tables(&signature) > 1
                && params[tuxbox_dxbc::compute_table::leading_tables(&signature)..].iter().all(|p| {
                    matches!(p.kind, RootParamKind::Constants { .. } | RootParamKind::Descriptor { .. })
                }) =>
        {
            let get = |shader: &[u8], stage| tuxbox_dxbc::compute_table::root_bindings(&layout_view(shader), stage, &signature);
            // These roots took the guest root before. Anything the stages disagree
            // on keeps that.
            let Some(bindings) = get(vertex, 1).zip(get(pixel, 0))
                .and_then(|(v, p)| tuxbox_dxbc::compute_table::merge_raster_bindings(v, p))
            else {
                return Ok(TableLayout::Unsupported);
            };
            corroborated_raster = true;
            // A static sampler the title compiled into the pixel shader and left out
            // of this root. The translation reads the root built here, so it finds it.
            recovered_samplers = tuxbox_dxbc::inline_sampler::omitted(pixel, &signature).unwrap_or_default();
            Some(bindings)
        }
        (Program::Raster { vertex, pixel }, [table, rest @ ..])
            if matches!(table.kind, RootParamKind::Table(_)) && (rest.is_empty()
                || matches!(rest, [param] if signature.flags & 0xc00 != 0 &&
                matches!(param.kind, RootParamKind::Constants { num32: 1..=4, .. }))) =>
        {
            if let [constants] = rest
                && let RootParamKind::Constants { num32, .. } = constants.kind
                && let Some(index) = tuxbox_dxbc::compute_table::scalar_heap_index(pixel, num32)
            {
                capture_stage_root(pixel, original);
                if signature.flags & 0x400 != 0 { scalar_index = Some(index); }
            }
            if !rest.is_empty() && signature.flags & 0x800 == 0 && scalar_index.is_none() {
                return Ok(TableLayout::Unsupported);
            }
            let ordinary = rest.is_empty() && signature.flags & 0xc00 == 0;
            let both_stages = |check_user_layout| {
                let get = |shader: &[u8], stage| {
                    if check_user_layout {
                        tuxbox_dxbc::compute_table::root_bindings(&layout_view(shader), stage, &signature)
                    } else {
                        tuxbox_dxbc::compute_table::bindings(&layout_view(shader), stage)
                    }
                };
                Some((get(vertex, 1)?, get(pixel, 0)?))
            };
            let checked = ordinary.then(|| both_stages(true)).flatten();
            corroborated_raster = checked.is_some();
            let Some((mut bindings, pixel_bindings)) = checked.or_else(|| both_stages(false)) else {
                return Ok(TableLayout::Unsupported);
            };
            if signature.static_samplers.is_empty() && ordinary {
                recovered_samplers = tuxbox_dxbc::inline_sampler::pixel(pixel)
                    .map(|s| vec![s])
                    .or_else(|| tuxbox_dxbc::inline_sampler::private(pixel, 0))
                    .unwrap_or_default();
            }
            else if !signature.static_samplers.is_empty() && signature.flags & 0x800 == 0 {
                recovered_samplers = missing_literal_samplers(pixel, 0, &signature.static_samplers);
            }
            for b in pixel_bindings {
                if let Some(existing) = bindings.iter().find(|r| r.slot == b.slot) {
                    if *existing != b {
                        return Err(E_INVALIDARG.into());
                    }
                } else {
                    bindings.push(b);
                }
            }
            bindings.sort_by_key(|r| r.slot);
            Some(bindings)
        }
        _ => return Ok(TableLayout::Unsupported),
    };
    let Some(bindings) = bindings else {
        if let Program::Compute(shader) = program {
            return append_corroborated_ranges(device, original, shader, &signature, node);
        }
        return Ok(TableLayout::Unsupported);
    };
    // Root SRVs read through their address keep their parameter; only the
    // register the shader names for them can change.
    let (root_srvs, bindings): (Vec<_>, Vec<_>) =
        bindings.into_iter().partition(|b| b.kind == tuxbox_dxbc::compute_table::ROOT_SRV);
    let mut srv_registers = Vec::new();
    for b in &root_srvs {
        let Some(RootParamKind::Descriptor { kind: 3, register, space }) =
            signature.params.get(b.table as usize).map(|p| &p.kind) else {
            return Ok(TableLayout::Unsupported);
        };
        if (*register, *space) != (b.register, b.space) {
            srv_registers.push((b.table as usize, b.register, b.space));
        }
    }
    // A reflected table binds through the resource heap at draw time, so a root
    // SRV rename alone (no table binding) keeps the guest root.
    if signature.version != 2 || (bindings.is_empty() && scalar_index.is_none()) {
        return Ok(TableLayout::Unsupported);
    }
    let tables = signature.params.len().max(2);
    let mut lengths = vec![0; tables];
    let mut rewrite = vec![false; tables];
    let mut accessed_views = Vec::new();
    for (index, param) in signature.params.iter().enumerate() {
        if let RootParamKind::Table(ranges) = &param.kind
            && (index >= lengths.len() || param.visibility != 0 || ranges.is_empty()
                || ranges.iter().any(|r| r.range_type > 2 || r.num == u32::MAX))
        {
            return Ok(TableLayout::Unsupported);
        }
    }
    for b in &bindings {
        let index = b.table as usize;
        let Some(RootParamKind::Table(ranges)) = signature.params.get(index).map(|p| &p.kind) else {
            return Ok(TableLayout::Unsupported);
        };
        let end = b.slot.checked_add(1).ok_or(E_INVALIDARG)?;
        lengths[index] = lengths[index].max(end);
        if b.kind < 2 { accessed_views.push((index, b.slot..end)); }
        let mut next = 0;
        let covered = ranges.iter().any(|r| {
            let start = if r.offset_in_table == u32::MAX { next } else { r.offset_in_table };
            next = start.saturating_add(r.num);
            (start..next).contains(&b.slot)
                && r.range_type == b.kind && r.space == b.space
                && r.base_register.checked_add(b.slot - start) == Some(b.register)
        });
        rewrite[index] |= !covered;
    }
    let rewrite_constants = resolve_constants.filter(|c| {
        !matches!(signature.params.get(c.param as usize).map(|p| &p.kind),
            Some(RootParamKind::Constants { register, space, .. }) if (*register, *space) == (c.register, c.space))
    });
    if !rewrite.contains(&true) && srv_registers.is_empty() && scalar_index.is_none()
        && rewrite_constants.is_none() && recovered_samplers.is_empty() && !strides {
        return Ok(TableLayout::Original);
    }

    let bytes = original.serialized();
    // Keep the original static samplers, descriptor volatility and root flags.
    // The input description stays live through D3D12SerializeVersionedRootSignature.
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
        if source.NumParameters as usize != signature.params.len() || source.pParameters.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let mut parameters =
            std::slice::from_raw_parts(source.pParameters, signature.params.len()).to_vec();
        let mut range_storage = vec![Vec::new(); tables];
        for (index, ranges) in range_storage.iter_mut().enumerate().filter(|(i, _)| rewrite[*i]) {
            let parameter = &mut parameters[index];
            let table = parameter.Anonymous.DescriptorTable;
            let guest_ranges = std::slice::from_raw_parts(table.pDescriptorRanges, table.NumDescriptorRanges as usize);
            for b in bindings.iter().filter(|b| b.table as usize == index) {
                let ty = D3D12_DESCRIPTOR_RANGE_TYPE(b.kind as i32);
                let mut matched = guest_ranges.iter().filter(|r| r.RangeType == ty);
                let flags = if let Some(first) = matched.next() {
                    if matched.any(|r| r.Flags != first.Flags) { return Ok(TableLayout::Unsupported); }
                    first.Flags
                } else if corroborated_raster || verified_compute || resolve_constants.is_some() || !recovered_samplers.is_empty() {
                    // This table ABI omits some native resource classes in the stream root.
                    D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE
                } else { return Ok(TableLayout::Unsupported); };
                ranges.push(D3D12_DESCRIPTOR_RANGE1 {
                    RangeType: ty,
                    NumDescriptors: 1,
                    BaseShaderRegister: b.register,
                    RegisterSpace: b.space,
                    Flags: flags,
                    OffsetInDescriptorsFromTableStart: b.slot,
                });
            }
            parameter.Anonymous.DescriptorTable = D3D12_ROOT_DESCRIPTOR_TABLE1 {
                NumDescriptorRanges: ranges.len() as u32, pDescriptorRanges: ranges.as_ptr(),
            };
        }
        for &(index, register, space) in &srv_registers {
            let parameter = &mut parameters[index];
            if parameter.ParameterType != D3D12_ROOT_PARAMETER_TYPE_SRV {
                return Ok(TableLayout::Unsupported);
            }
            parameter.Anonymous.Descriptor.ShaderRegister = register;
            parameter.Anonymous.Descriptor.RegisterSpace = space;
        }
        if let Some(constants) = rewrite_constants {
            // The user-data record matched this parameter's size; only its
            // register and space move.
            let Some(parameter) = parameters.get_mut(constants.param as usize)
                .filter(|p| p.ParameterType == D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS) else {
                return Ok(TableLayout::Unsupported);
            };
            parameter.Anonymous.Constants.ShaderRegister = constants.register;
            parameter.Anonymous.Constants.RegisterSpace = constants.space;
        }
        let index_map = scalar_index.map(|(scalar_word, scalar_mask)| IndexMapBinding {
            slot: source.NumParameters, scalar_word, scalar_mask,
        });
        if index_map.is_some() {
            // Read through a root SRV so the map itself never occupies a
            // guest-addressable descriptor. The CPU can publish new indices
            // after this draw has been recorded.
            parameters.push(D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
                ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                        ShaderRegister: 0,
                        RegisterSpace: 32766,
                        Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                    },
                },
            });
            desc.Anonymous.Desc_1_1.NumParameters = parameters.len() as u32;
        }
        let heap_strides = strides.then_some(parameters.len() as u32);
        if strides {
            // Like the index map: a root SRV, never a guest-addressable descriptor.
            parameters.push(D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                        ShaderRegister: 1,
                        RegisterSpace: 32766,
                        Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                    },
                },
            });
            desc.Anonymous.Desc_1_1.NumParameters = parameters.len() as u32;
        }
        let samplers: Vec<_> = recovered_samplers.into_iter().map(static_sampler).collect();
        // Recovered samplers join the root's own: `omitted` gives them registers the
        // root does not use, and the other recoveries apply only to a root with none.
        let samplers: Vec<_> = if samplers.is_empty() || source.NumStaticSamplers == 0 {
            samplers
        } else {
            if source.pStaticSamplers.is_null() { return Err(E_INVALIDARG.into()); }
            let mut all = std::slice::from_raw_parts(source.pStaticSamplers, source.NumStaticSamplers as usize).to_vec();
            all.extend(samplers);
            all
        };
        if !samplers.is_empty() {
            desc.Anonymous.Desc_1_1.pStaticSamplers = samplers.as_ptr();
            desc.Anonymous.Desc_1_1.NumStaticSamplers = samplers.len() as u32;
        }
        desc.Anonymous.Desc_1_1.pParameters = parameters.as_ptr();
        let mut blob = None;
        D3D12SerializeVersionedRootSignature(&desc, &mut blob, None)?;
        let blob = blob.ok_or(E_POINTER)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize());
        let native = device.CreateRootSignature(node, bytes)?;
        Ok(TableLayout::Reflected(Arc::new(ComputeTable {
            native,
            original: original.clone(),
            layout: RootSource::new(bytes),
            lengths,
            accessed_views,
            index_map,
            mirrors: Vec::new(),
            pixel_root: None,
            heap_strides,
        })))
    }
}

/// Keep every embedded range and append FSSM slots the root left out.
/// Shaders that already translate keep their parameter indexes. The game's
/// descriptor-table handle still addresses the slot FSSM recorded.
fn append_corroborated_ranges(
    device: &ID3D12Device,
    original: &RootSource,
    shader: &[u8],
    signature: &tuxbox_dxbc::rootsig::RootSig,
    node: u32,
) -> Result<TableLayout> {
    if signature.version != 2 {
        return Ok(TableLayout::Unsupported);
    }
    let extra = tuxbox_dxbc::compute_table::missing_root_descriptors(shader, signature);
    if extra.is_empty() {
        return Ok(TableLayout::Unsupported);
    }
    let bytes = original.serialized();
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
        if source.NumParameters as usize != signature.params.len() || source.pParameters.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let mut parameters =
            std::slice::from_raw_parts(source.pParameters, signature.params.len()).to_vec();
        let mut stored = vec![Vec::new(); signature.params.len()];
        let mut lengths = vec![0u32; signature.params.len()];
        let mut accessed_views = Vec::new();
        for (index, param) in signature.params.iter().enumerate() {
            let RootParamKind::Table(declared) = &param.kind else { continue };
            let additions: Vec<_> = extra.iter().filter(|binding| binding.table as usize == index).collect();
            if additions.is_empty() {
                continue;
            }
            let table = parameters[index].Anonymous.DescriptorTable;
            let guest = std::slice::from_raw_parts(
                table.pDescriptorRanges,
                table.NumDescriptorRanges as usize,
            );
            let mut ranges = guest.to_vec();
            for binding in &additions {
                let ty = D3D12_DESCRIPTOR_RANGE_TYPE(binding.kind as i32);
                let flags = guest
                    .iter()
                    .find(|range| range.RangeType == ty)
                    .map(|range| range.Flags)
                    .unwrap_or(
                        D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                            | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE,
                    );
                ranges.push(D3D12_DESCRIPTOR_RANGE1 {
                    RangeType: ty,
                    NumDescriptors: 1,
                    BaseShaderRegister: binding.register,
                    RegisterSpace: binding.space,
                    Flags: flags,
                    OffsetInDescriptorsFromTableStart: binding.slot,
                });
            }
            stored[index] = ranges;
            parameters[index].Anonymous.DescriptorTable = D3D12_ROOT_DESCRIPTOR_TABLE1 {
                NumDescriptorRanges: stored[index].len() as u32,
                pDescriptorRanges: stored[index].as_ptr(),
            };
            let Some((mut extent, mut views)) = finite_table(declared) else { continue };
            for binding in &additions {
                extent = extent.max(binding.slot.saturating_add(1));
                if binding.kind < 2 {
                    views.push(binding.slot..binding.slot + 1);
                }
            }
            lengths[index] = extent;
            accessed_views.extend(views.into_iter().map(|range| (index, range)));
        }
        desc.Anonymous.Desc_1_1.pParameters = parameters.as_ptr();
        let mut blob = None;
        D3D12SerializeVersionedRootSignature(&desc, &mut blob, None)?;
        let blob = blob.ok_or(E_POINTER)?;
        let bytes = std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize());
        let native = device.CreateRootSignature(node, bytes)?;
        Ok(TableLayout::Reflected(Arc::new(ComputeTable {
            native,
            original: original.clone(),
            layout: RootSource::new(bytes),
            lengths,
            accessed_views,
            index_map: None,
            mirrors: Vec::new(),
            pixel_root: None,
            heap_strides: None,
        })))
    }
}

/// Exclusive end of a finite descriptor table, plus the SRV and UAV spans in it.
fn finite_table(
    ranges: &[tuxbox_dxbc::rootsig::RootRange],
) -> Option<(u32, Vec<std::ops::Range<u32>>)> {
    let mut end = 0u32;
    let mut extent = 0u32;
    let mut views = Vec::new();
    for range in ranges {
        if range.num == u32::MAX {
            return None;
        }
        let start = if range.offset_in_table == u32::MAX { end } else { range.offset_in_table };
        let next = start.checked_add(range.num)?;
        end = next;
        extent = extent.max(next);
        if range.range_type < 2 {
            views.push(start..next);
        }
    }
    Some((extent, views))
}

pub(super) fn capture_stage_root(stage: &[u8], root: &RootSource) {
    use std::{io::Write, path::PathBuf, sync::LazyLock};
    static ROOTS: LazyLock<Option<PathBuf>> =
        LazyLock::new(|| std::env::var_os("TUXBOX_CAPTURE_RASTER_ROOTS").map(Into::into));
    static INPUTS: LazyLock<Option<PathBuf>> =
        LazyLock::new(|| std::env::var_os("TUXBOX_CAPTURE_SHADER_INPUTS").map(Into::into));
    if ROOTS.is_none() && INPUTS.is_none() { return; }
    let Some(shader) = super::shader_cache::source_digest(stage) else { return; };
    let write = |path: PathBuf, data: &[u8]| {
        if let Ok(mut file) = std::fs::File::create_new(&path)
            && file.write_all(data).is_err()
        {
            drop(file);
            let _ = std::fs::remove_file(path);
        }
    };
    if let Some(directory) = ROOTS.as_ref()
        && let Some(signature) = super::shader_cache::source_digest(root.payload())
    {
        write(directory.join(format!("{shader}-{signature}.rootsig")), root.serialized());
    }
    if let Some(directory) = INPUTS.as_ref() {
        write(directory.join(format!("{shader}.dxbc")), stage);
    }
}
