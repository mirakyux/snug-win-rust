use std::{array::TryFromSliceError, ops::Deref};

use bytemuck::{Pod, Zeroable};

const _EDID: [u8; 128] = [
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x0D, 0x19, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xFF, 0x21, 0x01, 0x03, 0x80, 0x32, 0x1F, 0x78, 0x07, 0xEE, 0x95, 0xA3, 0x54, 0x4C, 0x99, 0x26,
    0x0F, 0x50, 0x54, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x3A, 0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C,
    0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1E, 0x00, 0x00, 0x00, 0xFD, 0x00, 0x17, 0xF0, 0x0F,
    0xFF, 0x0F, 0x00, 0x0A, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0xFC, 0x00, 0x56,
    0x69, 0x72, 0x74, 0x75, 0x44, 0x69, 0x73, 0x70, 0x6C, 0x61, 0x79, 0x2B, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

const EDID_LEN: usize = _EDID.len();

static EDID: AlignedEdid<EDID_LEN> = AlignedEdid {
    data: _EDID,
    _align: [],
};

#[repr(C)]
struct AlignedEdid<const N: usize> {
    data: [u8; N],
    // required to make this type aligned to Edid
    _align: [Edid; 0],
}

impl<const N: usize> AlignedEdid<N> {
    fn new(data: &[u8]) -> Result<Self, TryFromSliceError> {
        let data: [u8; N] = data.try_into()?;
        Ok(Self { data, _align: [] })
    }
}

impl<const N: usize> Deref for AlignedEdid<N> {
    type Target = Edid;

    fn deref(&self) -> &Self::Target {
        let header = &self.data[..EDID_SIZE];
        bytemuck::from_bytes(header)
    }
}

const EDID_SIZE: usize = std::mem::size_of::<Edid>();

#[repr(C)]
#[derive(Debug, Copy, Clone, Pod, Zeroable)]
pub struct Edid {
    header: [u8; 8],
    manufacturer_id: [u8; 2],
    product_code: u16,
    serial_number: u32,
    manufacture_week: u8,
    manufacture_year: u8,
    version: u8,
    revision: u8,
}

impl Edid {
    pub fn generate_with(serial: u32) -> Vec<u8> {
        // change serial number in the header
        let mut header = *EDID;
        header.serial_number = serial;

        header.generate()
    }

    /// Advertise the first requested mode as preferred when EDID can represent it.
    /// The IPC/callback mode list remains authoritative, including on fallback.
    pub fn generate_for_mode(serial: u32, width: u32, height: u32, refresh: u32) -> Vec<u8> {
        let Some((dtd, max_clock)) = Self::preferred_dtd(width, height, refresh) else {
            return Self::generate_with(serial);
        };
        let mut edid = Self::generate_with(serial);
        edid[54..72].copy_from_slice(&dtd);
        // Keep the range descriptor consistent without narrowing its old limit.
        edid[81] = max_clock;
        Self::gen_checksum(&mut edid);
        edid
    }

    fn preferred_dtd(width: u32, height: u32, refresh: u32) -> Option<([u8; 18], u8)> {
        // DTD active/blanking fields are 12-bit, pixel clock is 16-bit in 10kHz.
        // Legal IPC modes outside these limits still use the original EDID.
        if !(1..=4095).contains(&width)
            || !(1..=4095).contains(&height)
            || !(1..=240).contains(&refresh)
        {
            return None;
        }
        let width = u16::try_from(width).ok()?;
        let height = u16::try_from(height).ok()?;
        let (hb, vb, hf, hs, vf, vs): (u16, u16, u16, u16, u16, u16) =
            match (width, height, refresh) {
                // Retain the original 1080p60 and hardware-verified 720p60/900p60 timings.
                (1920, 1080, 60) => (280, 45, 88, 44, 4, 5),
                (1280, 720, 60) => (370, 30, 110, 40, 5, 5),
                (1600, 900, 60) => (200, 100, 24, 80, 1, 3),
                // Synthetic virtual progressive timing, not a CVT/GTF claim.
                _ => (160, 45, 48, 32, 3, 5),
            };
        if hb > 4095
            || vb > 4095
            || hf == 0
            || hs == 0
            || vf == 0
            || vs == 0
            || hf > 1023
            || hs > 1023
            || vf > 63
            || vs > 63
            || hf.checked_add(hs)? >= hb
            || vf.checked_add(vs)? >= vb
        {
            return None;
        }
        let htotal = u64::from(width).checked_add(u64::from(hb))?;
        let vtotal = u64::from(height).checked_add(u64::from(vb))?;
        let pixels = htotal.checked_mul(vtotal)?;
        let target_clock = pixels.checked_mul(u64::from(refresh))?;
        let clock = u16::try_from(target_clock.checked_add(5_000)? / 10_000).ok()?;
        let encoded_clock = u64::from(clock).checked_mul(10_000)?;
        let error = encoded_clock.abs_diff(target_clock);
        // Bound both relative error (0.1%) and refresh error (0.1Hz).
        if clock == 0
            || error.checked_mul(1_000)? > target_clock
            || error.checked_mul(10)? > pixels
            // Respect the unchanged vertical and horizontal range limits.
            || encoded_clock < pixels.checked_mul(u64::from(_EDID[77]))?
            || encoded_clock > pixels.checked_mul(u64::from(_EDID[78]))?
            || encoded_clock < htotal.checked_mul(u64::from(_EDID[79]).checked_mul(1_000)?)?
            || encoded_clock > htotal.checked_mul(u64::from(_EDID[80]).checked_mul(1_000)?)?
        {
            return None;
        }
        let max_clock = u8::try_from(encoded_clock.checked_add(9_999_999)? / 10_000_000)
            .ok()?
            .max(_EDID[81]);
        let mut dtd = [0; 18];
        dtd[..2].copy_from_slice(&clock.to_le_bytes());
        dtd[2] = width.to_le_bytes()[0];
        dtd[3] = hb.to_le_bytes()[0];
        dtd[4] = u8::try_from(((width >> 8) << 4) | (hb >> 8)).ok()?;
        dtd[5] = height.to_le_bytes()[0];
        dtd[6] = vb.to_le_bytes()[0];
        dtd[7] = u8::try_from(((height >> 8) << 4) | (vb >> 8)).ok()?;
        dtd[8] = hf.to_le_bytes()[0];
        dtd[9] = hs.to_le_bytes()[0];
        dtd[10] = u8::try_from(((vf & 15) << 4) | (vs & 15)).ok()?;
        dtd[11] = u8::try_from(((hf >> 8) << 6) | ((hs >> 8) << 4) | ((vf >> 4) << 2) | (vs >> 4))
            .ok()?;
        // Digital separate positive sync, progressive; image size/borders stay zero.
        dtd[17] = 0x1e;
        Some((dtd, max_clock))
    }

    pub fn get_serial(edid: &[u8]) -> Result<u32, TryFromSliceError> {
        let edid = AlignedEdid::<EDID_LEN>::new(edid)?;
        Ok(edid.serial_number)
    }

    fn generate(&self) -> Vec<u8> {
        let header = bytemuck::bytes_of(self);

        // slice of monitor edid minus header
        let data = &EDID.data[EDID_SIZE..];

        // splice together header and the rest of the EDID
        let mut edid: Vec<u8> = header.iter().chain(data).copied().collect();
        // regenerate checksum
        Self::gen_checksum(&mut edid);

        edid
    }

    fn gen_checksum(data: &mut [u8]) {
        // important, this is the bare minimum length
        assert!(data.len() >= 128);

        // slice to the entire data minus the last checksum byte
        let edid_data = &data[..=126];

        // do checksum calculation
        let sum: u32 = edid_data.iter().copied().map(u32::from).sum();
        // this wont ever truncate
        #[allow(clippy::cast_possible_truncation)]
        let checksum = (256 - (sum % 256)) as u8;

        // update last byte with new checksum
        data[127] = checksum;
    }
}
