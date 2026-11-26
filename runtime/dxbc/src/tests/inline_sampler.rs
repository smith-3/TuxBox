#[test]
fn comparison_sampler_recovers_opaque_white_border_and_less_equal() {
    // State seen in ProjectScarlett issue #39. No game shader bytes needed.
    let words = [0x30b6, 0xc00000, 0x18500000, 0x80000000];
    assert_eq!(
        super::descriptor(words, 7, 32765, 5, true),
        Some([0x95, 4, 4, 3, 0, 1, 4, 2, 0, 12.0_f32.to_bits(), 7, 32765, 5])
    );
    assert!(super::descriptor(words, 7, 32765, 5, false).is_none());
    for (word, mask) in [(0, 0x8000), (1, 1), (2, 0x10000), (3, 1), (3, 0x40000000)] {
        let mut bad = words;
        bad[word] ^= mask;
        assert!(super::descriptor(bad, 7, 32765, 5, true).is_none());
    }
}

#[test]
fn comparison_sampler_state_is_recovered_at_the_image_site() {
    let mut words = [
        0xb008_30b6u32,
        0xbe89_03ff,
        0x00c0_0000,
        0xbe8a_03ff,
        0x1850_0000,
        0xbe8b_03ff,
        0x8000_0000,
        0xf0a0_0f08,
        0x0040_0a0a, // image_sample_c
        0xbf81_0000,
    ];
    let classify = |code: &[u32]| {
        let isa: Vec<_> = code.iter().flat_map(|x| x.to_le_bytes()).collect();
        super::sites(&isa, 5)
    };
    let sample = classify(&words).unwrap();
    assert_eq!(sample.descriptors.len(), 1);
    assert_eq!(
        sample.descriptors[0],
        [0x95, 4, 4, 3, 0, 1, 4, 2, 0, 12.0_f32.to_bits(), 0, 32765, 5]
    );
    assert_eq!(sample.sites.get(&28), Some(&0));
    words[7] = 0xf080_0f08; // ordinary image_sample must not receive a comparison sampler
    assert!(classify(&words).is_none());
}

#[test]
fn identical_literal_has_distinct_normal_and_comparison_slots() {
    let words = [
        0xb008_0092u32,
        0xbe89_03ff,
        0x00c0_0000,
        0xbe8a_03ff,
        0x1450_0000,
        0xbe8b_0380,
        0xf080_0f08,
        0x0040_0a0a, // image_sample
        0xf0a0_0f08,
        0x0040_0a0a, // image_sample_c
        0xbf81_0000,
    ];
    let isa: Vec<_> = words.iter().flat_map(|x| x.to_le_bytes()).collect();
    let sample = super::sites(&isa, 5).unwrap();
    assert_eq!(sample.descriptors.len(), 2);
    assert_eq!(sample.descriptors[0][0], 0x14);
    assert_eq!(sample.descriptors[1][0], 0x94);
    assert_eq!(sample.sites.get(&24), Some(&0));
    assert_eq!(sample.sites.get(&32), Some(&1));
}

#[test]
fn all_six_logged_literal_states_match_the_same_static_sampler_decoder() {
    // Six distinct states observed in the ProjectScarlett #39 log.
    for (words, compare, filter, aniso) in [
        ([0x30b6, 0xc00000, 0x18500000, 0x80000000], true, 0x95, 1),
        ([0x92, 0xc00000, 0x14500000, 0], false, 0x14, 1),
        ([0, 0xc00000, 0x14000000, 0], false, 0, 1),
        ([0x92, 0xc00000, 0x14000000, 0], false, 0, 1),
        ([0x400, 0xc00000, 0x18f00000, 0], false, 0x55, 4),
        ([0x92, 0xc00000, 0x18500000, 0], false, 0x15, 1),
    ] {
        let sampler = super::descriptor(words, 3, 7, 5, compare).unwrap();
        assert_eq!((sampler[0], sampler[5]), (filter, aniso));
        assert!(super::matches_static(words, &sampler, compare));
        // All state fields (including LODs, compare function, and
        // anisotropy) must match. Register/space are binding identity.
        for index in 0..10 {
            let mut wrong = sampler;
            wrong[index] ^= 1;
            assert!(!super::matches_static(words, &wrong, compare), "field {index}");
        }
        assert!(!super::matches_static(words, &sampler, !compare));
    }
    for border in 0..=2 {
        let words = [0x1b6, 0xc00000, 0x14500000, border << 30];
        let sampler = super::descriptor(words, 0, 0, 0, false).unwrap();
        assert_eq!(sampler[7], border);
        assert!(super::matches_static(words, &sampler, false));
    }
}

#[test]
fn pair_moves_recover_both_words_without_guessing() {
    let mut words = [
        0xb00c_0092u32, // s_movk_i32 s12, 0x92
        0xbe8d_03ff, 0x00c0_0000, // s_mov_b32 s13, max LOD
        0xbe8e_03ff, 0x1450_0000, // s_mov_b32 s14, linear
        0xbe8f_0380, // s_mov_b32 s15, zero
        0xbe88_040c, // s_mov_b64 s[8:9], s[12:13]
        0xbe8a_040e, // s_mov_b64 s[10:11], s[14:15]
        0xf080_0f08, 0x0040_0a0a,
        0xbf81_0000,
    ];
    let classify = |code: &[u32]| {
        let isa: Vec<_> = code.iter().flat_map(|x| x.to_le_bytes()).collect();
        super::sites(&isa, 5)
    };
    let result = classify(&words).unwrap();
    assert_eq!(result.descriptors[0][0], 0x14);
    assert_eq!(result.sites.get(&32), Some(&0));
    words[6] = 0xbe88_040a; // reads uninitialized s[10:11]
    assert!(classify(&words).is_none());
}

#[test]
fn a_loop_backedge_cannot_initialize_a_sampler_at_entry() {
    let words = [
        0xf080_0f08u32, 0x0040_0a0a, // sample before any initialization
        0xb008_0092,
        0xbe89_03ff, 0x00c0_0000,
        0xbe8a_03ff, 0x1450_0000,
        0xbe8b_0380,
        0xbf82_fff7, // branch back to entry
    ];
    let isa: Vec<_> = words.iter().flat_map(|x| x.to_le_bytes()).collect();
    assert!(super::sites(&isa, 5).is_none());
}

#[test]
fn sampler_words_must_agree_on_every_reachable_loop_path() {
    // s[8:11] describe linear clamped sampling. The preheader skips a
    // conflicting definition of s10 immediately before the loop header.
    let mut words = [
        0xb008_0092u32, // s_movk_i32 s8, 0x92
        0xbe89_03ff,
        0x00c0_0000, // s_mov_b32 s9, max LOD 12
        0xbe8a_03ff,
        0x1450_0000, // s_mov_b32 s10, linear filtering
        0xbe8b_0380, // s_mov_b32 s11, 0
        0xbf82_0002, // s_branch sample
        0xbe8a_03ff,
        0x1400_0000, // unreachable point filtering
        0xf080_0f08,
        0x0040_0a0a, // sample: image_sample ... s[8:11]
        0xbf85_fffd, // s_cbranch_scc1 sample
        0xbf81_0000, // s_endpgm
    ];
    let classify = |code: &[u32]| {
        let isa: Vec<_> = code.iter().flat_map(|x| x.to_le_bytes()).collect();
        super::sites(&isa, 5)
    };
    let sample = classify(&words).unwrap();
    assert_eq!(sample.descriptors.len(), 1);
    assert_eq!(sample.descriptors[0][0], 0x14);
    assert_eq!(sample.descriptors[0][12], 5);
    assert_eq!(sample.sites.get(&36), Some(&0));
    words[6] = 0xbf84_0002; // s_cbranch_scc0 also allows the point filter.
    assert!(classify(&words).is_none());
}

#[test]
fn a_static_sampler_the_root_leaves_out_takes_its_declared_register() {
    // image_sample_c with the comparison state, then image_sample with a
    // linear clamped state the root does carry.
    let words = [
        0xb008_30b6u32, // s_movk_i32 s8
        0xbe89_03ff, 0x00c0_0000,
        0xbe8a_03ff, 0x1850_0000,
        0xbe8b_03ff, 0x8000_0000,
        0xf0a0_0f08, 0x0040_0a0a, // image_sample_c
        0xb008_0092, // s_movk_i32 s8, clamp
        0xbe8b_0380, // s_mov_b32 s11, 0
        0xf080_0f08, 0x0040_0a0a, // image_sample
        0xbf81_0000,
    ];
    let isa: Vec<_> = words.iter().flat_map(|x| x.to_le_bytes()).collect();
    let carried = super::descriptor([0x92, 0xc00000, 0x18500000, 0], 1, 1, 0, false).unwrap();
    let shadow = [0x95, 4, 4, 3, 0, 1, 4, 2, 0, 12.0_f32.to_bits(), 17, 1, 5];
    let omitted = |declared: &[(u32, u32)], recorded: &[u32], statics: &[[u32; 13]]| {
        super::omitted_states(&isa, declared, recorded, statics)
    };
    assert_eq!(omitted(&[(1, 1), (17, 1)], &[17, 1], &[carried]), Some(vec![shadow]));
    // The root already has that state, or covers every declared register.
    assert_eq!(omitted(&[(1, 1), (17, 1)], &[17, 1], &[carried, shadow]), None);
    let mut other = carried;
    other[10] = 17;
    assert_eq!(omitted(&[(1, 1), (17, 1)], &[17, 1], &[carried, other]), None);
    // A sampler visible only to another stage does not cover the register.
    other[12] = 1;
    assert_eq!(omitted(&[(1, 1), (17, 1)], &[17, 1], &[carried, other]), Some(vec![shadow]));
    // PSV and FSSM disagree, or the root has no static samplers of its own.
    assert_eq!(omitted(&[(1, 1), (17, 1)], &[1], &[carried]), None);
    assert_eq!(omitted(&[(1, 1), (17, 1)], &[1, 18], &[carried]), None);
    assert_eq!(omitted(&[(1, 1), (17, 1)], &[17, 1], &[]), None);
    // With neither state in the root both are missing. They take the free
    // registers in order, and need one each.
    let mut elsewhere = carried;
    (elsewhere[0], elsewhere[10]) = (0, 9);
    let both = omitted(&[(17, 1), (1, 1)], &[17, 1], &[elsewhere]).unwrap();
    assert_eq!(both.iter().map(|s| (s[10], s[11], s[12])).collect::<Vec<_>>(), [(1, 1, 5), (17, 1, 5)]);
    assert!(both.iter().any(|s| s[..10] == shadow[..10]) && both.iter().any(|s| s[..10] == carried[..10]));
    elsewhere[10] = 1;
    assert_eq!(omitted(&[(17, 1), (1, 1)], &[17, 1], &[elsewhere]), None);
}

fn pixel_with_sampler(words: &[u32], features: u64, fssm_register: u32) -> Vec<u8> {
    use crate::{container::tests::container_with, xdat::tests::{container, xbch}};
    let dwords = |values: &[u32]| values.iter().flat_map(|n| n.to_le_bytes()).collect::<Vec<_>>();
    let mut image = vec![0u8; 0x170];
    let write_u64 = |bytes: &mut [u8], at: usize, value: u64| {
        bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    };
    write_u64(&mut image, 0, 6);
    for slot in 0..12 {
        write_u64(&mut image, 8 + 8 * slot, if slot == 7 { 0x80 } else { 0xdead_beef_dead_beef });
    }
    write_u64(&mut image, 0x80, 0x170);
    write_u64(&mut image, 0x88, (words.len() * 4) as u64);
    image.extend(dwords(words));
    let records_offset = image.len() as u64;
    write_u64(&mut image, 0xe0, records_offset);
    image[0xec..0xf0].copy_from_slice(&1u32.to_le_bytes());
    image.extend(dwords(&[0x401, 0, fssm_register, 0, 0]));
    let mut fssm = Vec::from(*b"FSSM\x01\x01\0\0");
    fssm.extend((image.len() as u64).to_le_bytes());
    fssm.extend(image);
    let xdat = container(&[(13, xbch(&fssm)), (7, b"NOBC".to_vec())]);
    let mut dxil = dwords(&[
        0x66, ((24 + xdat.len()) / 4) as u32, u32::from_le_bytes(*b"DXIL"),
        0x100, (16 + xdat.len() - 4) as u32, 4,
    ]);
    dxil.extend(xdat);
    let psv = dwords(&[0, 1, 16, 1, 1, 0, 0]);
    container_with(&[(b"DXIL", &dxil), (b"PSV0", &psv), (b"SFI0", &features.to_le_bytes())])
}

#[test]
fn a_mesh_pixel_root_can_add_a_proven_omitted_sampler() {
    use crate::rootsig::{RootParam, RootParamKind, RootRange, RootSig};
    let mut words = [
        0xb008_0400,              // s8, 4:1 anisotropy
        0xbe89_03ff, 0x00c0_0000, // s9, maximum LOD 12
        0xbe8a_03ff, 0x18f0_0000, // s10, anisotropic min/mag and linear mip
        0xbe8b_0380,              // s11, zero
        0xf080_0f08, 0x0040_0a0a, // image_sample with s[8:11]
        0xbf81_0000,
    ];
    let point = super::descriptor([0, 0xc00000, 0x14000000, 0], 0, 1, 0, false).unwrap();
    let mut root = RootSig {
        version: 2, flags: 0x400, params: vec![],
        num_static_samplers: 1, static_samplers: vec![point],
    };
    let shader = pixel_with_sampler(&words, 1 << 25, 0);
    let recovered = super::mesh_pixel_omissions(&shader, &root);
    assert_eq!(recovered, [[0x55, 1, 1, 1, 0, 4, 1, 0, 0, 12.0_f32.to_bits(), 1, 32765, 5]]);
    assert!(super::mesh_pixel_omissions(&pixel_with_sampler(&words, 3 << 25, 0), &root).is_empty());
    assert!(super::mesh_pixel_omissions(&pixel_with_sampler(&words, 1 << 25, 1), &root).is_empty());
    for disabled in [0x20, 0x800] {
        root.flags |= disabled;
        assert!(super::mesh_pixel_omissions(&shader, &root).is_empty());
        root.flags &= !disabled;
    }
    root.params.push(RootParam {
        visibility: 0,
        kind: RootParamKind::Table(vec![RootRange {
            range_type: 3, num: 1, base_register: 1, space: 32765, offset_in_table: 0,
        }]),
    });
    assert!(super::mesh_pixel_omissions(&shader, &root).is_empty());
    root.params.clear();
    root.static_samplers = vec![super::descriptor([0x400, 0xc00000, 0x18f00000, 0], 0, 1, 0, false).unwrap()];
    assert!(super::mesh_pixel_omissions(&shader, &root).is_empty());
    root.static_samplers = vec![point];
    words[0] = 0xbe88_0302; // s_mov_b32 s8, s2, whose state is unknown.
    assert!(super::mesh_pixel_omissions(&pixel_with_sampler(&words, 1 << 25, 0), &root).is_empty());
}

#[test]
fn an_ordinary_mesh_pixel_root_recovers_the_declared_sampler_it_leaves_out() {
    use crate::rootsig::{RootParam, RootParamKind, RootRange, RootSig};
    let words = [
        0xb008_0400u32,           // s8, 4:1 anisotropy
        0xbe89_03ff, 0x00c0_0000, // s9, maximum LOD 12
        0xbe8a_03ff, 0x18f0_0000, // s10, anisotropic min/mag and linear mip
        0xbe8b_0380,              // s11, zero
        0xf080_0f08, 0x0040_0a0a, // image_sample with s[8:11], declared as s0, space 1
        0xbf81_0000,
    ];
    let shader = pixel_with_sampler(&words, 0, 0);
    let mut elsewhere = super::descriptor([0, 0xc00000, 0x14000000, 0], 0, 1, 0, false).unwrap();
    elsewhere[10] = 5;
    let root = |flags, params| RootSig {
        version: 2, flags, params, num_static_samplers: 1, static_samplers: vec![elsewhere],
    };
    let table = |visibility, base_register, num, space| RootParam {
        visibility,
        kind: RootParamKind::Table(vec![RootRange { range_type: 3, num, base_register, space, offset_in_table: 0 }]),
    };
    let recovered = vec![[0x55, 1, 1, 1, 0, 4, 1, 0, 0, 12.0_f32.to_bits(), 0, 1, 5]];
    // The failing MatrixVI pixel stages: static samplers, no heap indexing.
    assert_eq!(super::mesh_pixel_samplers(&shader, &root(0, vec![])), recovered);
    // The guest's own samplers are needed.
    for flags in [0x20, 0x800] {
        assert!(super::mesh_pixel_samplers(&shader, &root(flags, vec![])).is_empty());
    }
    // A visible sampler table already binds s0 in space 1.
    assert!(super::mesh_pixel_samplers(&shader, &root(0, vec![table(5, 0, 1, 1)])).is_empty());
    assert!(super::mesh_pixel_samplers(&shader, &root(0, vec![table(0, 0, u32::MAX, 1)])).is_empty());
    // Tables in another space, after the register, or for another stage do not.
    for other in [table(5, 0, 1, 2), table(5, 1, 4, 1), table(1, 0, 1, 1)] {
        assert_eq!(super::mesh_pixel_samplers(&shader, &root(0, vec![other])), recovered);
    }
    // Direct resource heap indexing takes the proven-literal path.
    assert_eq!(
        super::mesh_pixel_samplers(&shader, &root(0x400, vec![])),
        super::mesh_pixel_omissions(&shader, &root(0x400, vec![])),
    );
    // An empty root takes the private samplers, unless a table claims their space.
    let empty = |params| RootSig { version: 2, flags: 0, params, num_static_samplers: 0, static_samplers: vec![] };
    assert_eq!(super::mesh_pixel_samplers(&shader, &empty(vec![])), super::private(&shader, 0).unwrap_or_default());
    assert!(super::mesh_pixel_samplers(&shader, &empty(vec![table(0, 0, 1, 32765)])).is_empty());
}
