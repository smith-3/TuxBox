use super::*;

#[test]
fn wave_range_compatibility_checks_interval_overlap() {
    let wave64_refusal = check_wave_range((64, 64), (32, 32)).unwrap();
    assert_eq!(wave64_refusal.tag, "host-lacks-wave64");
    assert!(wave64_refusal.detail.contains("host lacks wave64"));
    assert_eq!(check_wave_range((64, 64), (32, 64)), None);
    assert_eq!(check_wave_range((64, 64), (64, 64)), None);
    assert_eq!(check_wave_range((32, 32), (32, 32)), None);
    assert_eq!(check_wave_range((32, 32), (32, 64)), None);
    assert_eq!(check_wave_range((32, 64), (32, 32)), None);
    let wave32_refusal = check_wave_range((32, 32), (64, 64)).unwrap();
    assert_eq!(wave32_refusal.tag, "host-lacks-wave32");
    let range_refusal = check_wave_range((64, 128), (32, 32)).unwrap();
    assert_eq!(range_refusal.tag, "host-wave-range-32-32-excludes-64-128");
    assert_eq!(check_wave_range((64, 64), (0, 0)), None);
    assert_eq!(check_wave_range((64, 64), (64, 32)), None);
}

#[test]
fn only_a_64_lane_program_on_a_32_lane_host_is_translated_again() {
    assert!(needs_narrow_waves(Some((64, 64)), (32, 32)));
    assert!(!needs_narrow_waves(Some((64, 64)), (32, 64)));
    assert!(!needs_narrow_waves(Some((64, 64)), (64, 64)));
    assert!(!needs_narrow_waves(Some((32, 32)), (32, 32)));
    assert!(!needs_narrow_waves(None, (32, 32)));
    // The translation runs waves of 32, so a host limited below that cannot use it.
    assert!(!needs_narrow_waves(Some((64, 64)), (16, 16)));
}

#[test]
fn a_failed_compile_is_reported_late_only_once_creation_returned() {
    const FAILED: i32 = 0x8007_000e_u32 as i32;
    // The creating thread waited and saw the failure itself.
    let waited = LateFailure::default();
    assert_eq!(waited.settle_failure(FAILED), None);
    // The compile failed after the stand-in was returned.
    let after = LateFailure::default();
    assert_eq!(after.settle_deferred(), None);
    assert_eq!(after.settle_failure(FAILED), Some(FAILED));
    // The compile failed while creation was deciding to return a stand-in.
    let between = LateFailure::default();
    assert_eq!(between.settle_failure(FAILED), None);
    assert_eq!(between.settle_deferred(), Some(FAILED));
}
