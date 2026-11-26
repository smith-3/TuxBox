use super::*;

const KEY: GUID = GUID::from_u128(0x6f2a_51c0_9d3e_4b7a_8c11_2e4f_5a6b_7c8d);

#[test]
fn private_bytes_follow_the_native_size_protocol() {
    let store = PrivateData::default();
    let mut size = 4u32;
    let mut out = [0u8; 8];
    assert_eq!(unsafe { store.get(&KEY, &mut size, out.as_mut_ptr().cast()) }, DXGI_ERROR_NOT_FOUND);
    assert_eq!(size, 0);
    let value = [1u8, 2, 3, 4, 5, 6];
    assert_eq!(unsafe { store.set(&KEY, value.len() as u32, value.as_ptr().cast()) }, S_OK);
    // A size query, a buffer that is too small, then a copy.
    size = 0;
    assert_eq!(unsafe { store.get(&KEY, &mut size, core::ptr::null_mut()) }, S_OK);
    assert_eq!(size, 6);
    size = 4;
    assert_eq!(unsafe { store.get(&KEY, &mut size, out.as_mut_ptr().cast()) }, DXGI_ERROR_MORE_DATA);
    assert_eq!(size, 6);
    size = 8;
    assert_eq!(unsafe { store.get(&KEY, &mut size, out.as_mut_ptr().cast()) }, S_OK);
    assert_eq!((size, &out[..6]), (6, &value[..]));
    assert_eq!(unsafe { store.get(&KEY, core::ptr::null_mut(), out.as_mut_ptr().cast()) }, E_INVALIDARG);
    // Setting null data removes the entry.
    assert_eq!(unsafe { store.set(&KEY, 0, core::ptr::null()) }, S_OK);
    assert_eq!(unsafe { store.get(&KEY, &mut size, out.as_mut_ptr().cast()) }, DXGI_ERROR_NOT_FOUND);
}
