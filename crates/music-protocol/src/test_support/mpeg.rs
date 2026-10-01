//! Synthetic MPEG frames shared by offline media and application tests.
pub fn audio() -> Vec<u8> {
    let mut frame = vec![0; 417];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
    [frame.clone(), frame].concat()
}
