use std::mem::{align_of, offset_of, size_of};
use wdf_umdf_sys::{IDARG_IN_SWAPCHAINSETDEVICE, IDARG_OUT_RELEASEANDACQUIREBUFFER, IDDCX_METADATA};

macro_rules! layout {
    ($ty:ident) => {
        println!("{}.size={}", stringify!($ty), size_of::<$ty>());
        println!("{}.align={}", stringify!($ty), align_of::<$ty>());
    };
}

macro_rules! field {
    ($ty:ident, $field:ident) => {
        println!(
            "{}.{}={}",
            stringify!($ty),
            stringify!($field),
            offset_of!($ty, $field)
        );
    };
}

fn main() {
    layout!(IDARG_IN_SWAPCHAINSETDEVICE);
    field!(IDARG_IN_SWAPCHAINSETDEVICE, pDevice);
    layout!(IDARG_OUT_RELEASEANDACQUIREBUFFER);
    field!(IDARG_OUT_RELEASEANDACQUIREBUFFER, MetaData);
    layout!(IDDCX_METADATA);
    field!(IDDCX_METADATA, Size);
    field!(IDDCX_METADATA, PresentationFrameNumber);
    field!(IDDCX_METADATA, DirtyRectCount);
    field!(IDDCX_METADATA, MoveRegionCount);
    field!(IDDCX_METADATA, HwProtectedSurface);
    field!(IDDCX_METADATA, pSurface);
}
