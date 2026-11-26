use super::*;

#[test]
fn launch_requires_the_packed_group_and_wave64_lane_definitions() {
    let words: [u32; 10] = [
        0xd7650000, 0x000100c1, // v0 = mbcnt_lo(-1, 0)
        0x7e000505, // s0 = readfirstlane(v5)
        0x9381ff03, 0x00040018, // s1 = s3[27:24]
        0xd7660000, 0x000200c1, // v0 = mbcnt_hi(-1, v0)
        0x900a8800, // s10 = s0 >> 8
        0xd7460001, 0x04010c01, // v1 = (s1 << 6) + v0
    ];
    let check = |words: &[u32]| {
        let bytes = words
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>();
        validate_launch(&disassemble(&bytes), 8, Opnd::Vgpr(1))
    };
    assert!(check(&words).is_ok());
    let mut wrong_wave = words;
    wrong_wave[4] = 0x00040017;
    assert!(check(&wrong_wave).is_err());
    // An s_load_dwordx4 into s[0:3] clobbers both intermediate values.
    let mut clobbered = words.to_vec();
    clobbered.splice(7..7, [0xf408000d, 0xfa000000]);
    assert!(check(&clobbered).is_err());
    let mut wrong_group = words;
    wrong_group[7] = 0x900a8700;
    assert!(check(&wrong_group).is_err());
}

#[test]
fn only_uniform_values_can_bypass_a_wave_broadcast() {
    let validate = |words: &[u32]| {
        let bytes = words
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>();
        { let body = disassemble(&bytes); validate_lane_ops(&body, &waterfall_reads(&body), false) }
    };
    // Entry v5 holds the same packed group identifier in every lane.
    assert!(validate(&[0x7e000505]).is_ok());
    // Counting preceding lanes overwrites v5 with a lane-dependent value.
    assert!(validate(&[0xd7650005, 0x000100c1, 0x7e000505]).is_err());
    // A new constant in v5 is safe to broadcast.
    assert!(validate(&[0x7e0a0281, 0x7e000505]).is_ok());
    // Shared memory would require synchronization among logical Xbox waves.
    assert!(validate(&[0xd8380100, 0x00020100]).is_err());
}

#[test]
fn class5_mesh_pointer_is_eight_above_the_user_location() {
    assert_class5_mesh_pointer();
}

#[test]
fn class5_descriptor_load_uses_the_guest_heap_index() {
    assert_class5_descriptor_load();
}

#[test]
fn sampler_heap_binding_uses_the_reaching_pointer() {
    use tuxbox_dxbc::mesh_table::MeshTableBinding;

    let words = [
        0xbe99_0380u32, // s_mov_b32 s25, 0
        0xbf82_0001, // s_branch past the write to s24
        0xbe98_0380, // s_mov_b32 s24, 0
        0xf408_050c, 0xfa00_0410, // s_load_dwordx4 s[20:23], s[24:25], 0x410
        0xf40c_0808, 0xfa00_0000, // s_load_dwordx8 s[32:39], s[16:17], 0
        0xf09c_0108, 0x00a8_0401, // image_sample_lz v4, v[1:2], s[32:39], s[20:23]
        0xbf81_0000,
    ];
    let emit = |words: [u32; 10]| -> Result<String, String> {
        let isa: Vec<_> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        let ex = heap_extract(&[[6, 0, 1, 0, 16]], 0x800, &isa);
        let insns = disassemble(&isa);
        let mut em = Emitter::new(&ex, insns.clone());
        em.mesh_bindings = Some(super::mesh_bindings::MeshBindings {
            tables: [((16, 0), MeshTableBinding {
                parameter: 0, kind: 0, register: 1, space: 0, stride_words: None,
            })].into(),
            raw: Default::default(),
            constants: Default::default(),
            replacements: Default::default(),
        });
        em.resolve_resources()?;
        em.emit_insn(5, &insns[5], false, None)?;
        let mut hlsl = String::new();
        em.heap_sampler_locals(&mut hlsl);
        hlsl.push_str(&em.body);
        Ok(hlsl)
    };
    let hlsl = emit(words).unwrap();
    assert!(hlsl.contains("SamplerState samp65 = SamplerDescriptorHeap[65];"));
    assert!(hlsl.contains(".SampleLevel(samp65,"));

    let mut clobbered = words;
    clobbered[1] = 0xbf84_0001; // s_cbranch_scc0: the write is now reachable
    let reason = emit(clobbered).unwrap_err();
    assert!(reason.contains("base s24 is not a root table pointer"));
}
