use mistlib::ffi::{get_build_info, get_version};

unsafe fn read_ffi_string(function: unsafe extern "C" fn(*mut u8, usize) -> u32) -> String {
    let required = unsafe { function(std::ptr::null_mut(), 0) } as usize;
    assert!(required > 0);

    let mut buffer = vec![0_u8; required];
    let written = unsafe { function(buffer.as_mut_ptr(), buffer.len()) } as usize;
    assert_eq!(written, required);
    String::from_utf8(buffer).unwrap()
}

#[test]
fn native_version_exports_match_the_loaded_library() {
    let version = unsafe { read_ffi_string(get_version) };
    assert_eq!(version, env!("CARGO_PKG_VERSION"));

    let build_info = unsafe { read_ffi_string(get_build_info) };
    let value: serde_json::Value = serde_json::from_str(&build_info).unwrap();
    assert_eq!(value["version"], version);
    assert_eq!(value["target"], env!("MISTLIB_BUILD_TARGET"));
    assert!(value["commit"].is_string());
    assert!(value["dirty"].is_boolean());
}
