/// Sums `len` bytes starting at `ptr` in the guest's linear memory and returns
/// the wrapping sum. Stand-in for "actor receives a payload, does some work."
///
/// # Safety
/// `ptr` and `len` must describe a valid byte range inside the guest's linear
/// memory. The host arranges this by writing into the memory before calling.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sum_bytes(ptr: u32, len: u32) -> u32 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let mut sum: u32 = 0;
    for &b in bytes {
        sum = sum.wrapping_add(u32::from(b));
    }
    sum
}
