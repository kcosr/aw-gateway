const MAX_SECRET_COMPARE_BYTES: usize = 4096;

pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() > MAX_SECRET_COMPARE_BYTES || right.len() > MAX_SECRET_COMPARE_BYTES {
        return false;
    }
    let mut diff = left.len() ^ right.len();
    for index in 0..MAX_SECRET_COMPARE_BYTES {
        let left_byte = left.get(index).copied().unwrap_or_default();
        let right_byte = right.get(index).copied().unwrap_or_default();
        diff |= usize::from(left_byte ^ right_byte);
    }
    diff == 0
}
