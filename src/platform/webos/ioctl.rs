//! Linux `_IOC` request numbers for the ioctls the `libc` crate doesn't carry (evdev, hidraw).
//! One encoder, so no subsystem hand-assembles the bit layout on its own.

/// `_IOC_WRITE`: the caller passes data in.
pub(crate) const WRITE: u32 = 1;
/// `_IOC_READ`: the kernel writes data back.
pub(crate) const READ: u32 = 2;

/// `_IOC(dir, ty, nr, len)`: direction in the top two bits, then the argument's size, the
/// subsystem's letter and the call number.
pub(crate) const fn ioc(dir: u32, ty: u8, nr: u32, len: u32) -> libc::c_ulong {
    ((dir << 30) | (len << 16) | ((ty as u32) << 8) | nr) as libc::c_ulong
}
