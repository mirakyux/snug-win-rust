//! 截图，移植自 C# `SnugWin.Windows.Windows.ScreenshotService`（`ScreenshotService.cs`）。
//!
//! 流程与 C# 一一对应：
//! 1. 校验窗口还在、pid 没被系统复用；
//! 2. 取 `GetWindowRect` 与 DWM 可见边框；
//! 3. `PrintWindow(PW_RENDERFULLCONTENT)`，失败再试 `PrintWindow(0)`；
//! 4. 成功就按可见边框裁掉阴影，失败就 `BitBlt` 抓屏幕对应区域；
//! 5. 编码成 PNG（C# 用 WPF 的 PNG 编码器）。
//!
//! 两处实现选择：
//! - **PNG 自己编解码**（stored-block deflate），不引 `png`/`flate2`，保持零依赖。
//!   代价是 IDAT 不压缩，截图 PNG 体积约等于像素数据（1920×1080 约 8 MB）；
//!   `crop` 也只认这种 stored 流（截图都是自己产的）。
//! - **alpha 一律写 255**。C# 里 GDI+ 把 GDI 画进位图的 alpha 补成 255（实测
//!   `CopyFromScreen` 后取像素 A=255），这里照抄"不透明截图"的可见结果。
//!
//! 所有有副作用的调用都在 [`ScreenshotSystem`] 后面，单测只走假实现。

use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::Duration;

use snug_core::geometry::Rect;
use snug_core::screenshots::{CaptureError, CaptureFailure, CapturedImage, ScreenshotControl};
use snug_core::WindowKey;
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HALFTONE, HDC, HGDIOBJ,
    ReleaseDC, SRCCOPY, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::UI::WindowsAndMessaging::PW_RENDERFULLCONTENT;

// `PrintWindow` 被 windows crate 放在 `Win32::Storage::Xps` 下，为一处截图多开一个
// feature 不划算，这里直接声明（签名与 user32 导出一致）。
#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(hwnd: HWND, hdc: HDC, flags: u32) -> windows::core::BOOL;
}

// ---------------------------------------------------------------------------
// 纯逻辑：像素、PNG、裁剪
// ---------------------------------------------------------------------------

/// 一张 8 位 RGBA 位图（行优先、自上而下）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelImage {
    pub width: i32,
    pub height: i32,
    pub rgba: Vec<u8>,
}

impl PixelImage {
    pub fn new(width: i32, height: i32, rgba: Vec<u8>) -> Option<Self> {
        (width > 0 && height > 0 && rgba.len() == width as usize * height as usize * 4)
            .then_some(Self { width, height, rgba })
    }

    /// 取一个像素（越界返回 `None`）。
    pub fn pixel(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        let index = (y as usize * self.width as usize + x as usize) * 4;
        Some([self.rgba[index], self.rgba[index + 1], self.rgba[index + 2], self.rgba[index + 3]])
    }
}

/// C# `Math.Round` 的"四舍六入五成双"。
///
/// 实现只留一份（[`crate::display::round_ties_even`]）：这两处当初是各抄一遍，
/// 改一处忘另一处就是两种舍入混用。这里 re-export，调用路径与测试都不用改。
pub use crate::display::round_ties_even;

/// C# `Math.Clamp(int, int, int)`。
fn clamp_int(value: i32, min: i32, max: i32) -> i32 {
    value.max(min).min(max.max(min))
}

/// 把 `PrintWindow` 得到的整窗位图按可见边框裁剪（C# `CaptureAsync` 的 clamp 段）。
///
/// 返回 `(x, y, width, height)`，保证落在位图内且至少 1×1。
pub fn visible_crop(visible: Rect, raw: Rect, width: i32, height: i32) -> (i32, i32, i32, i32) {
    let x = clamp_int(
        round_ties_even(visible.x - raw.x) as i32,
        0,
        (width - 1).max(0),
    );
    let y = clamp_int(
        round_ties_even(visible.y - raw.y) as i32,
        0,
        (height - 1).max(0),
    );
    let crop_width = clamp_int(round_ties_even(visible.width) as i32, 1, width - x);
    let crop_height = clamp_int(round_ties_even(visible.height) as i32, 1, height - y);
    (x, y, crop_width, crop_height)
}

/// 裁一块像素（越界部分直接失败，调用方应先 clamp）。
pub fn crop_pixels(
    image: &PixelImage,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Option<PixelImage> {
    if width < 1 || height < 1 || x < 0 || y < 0 {
        return None;
    }
    if x + width > image.width || y + height > image.height {
        return None;
    }
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in y..y + height {
        let start = (row as usize * image.width as usize + x as usize) * 4;
        rgba.extend_from_slice(&image.rgba[start..start + width as usize * 4]);
    }
    Some(PixelImage { width, height, rgba })
}

/// 32 位 BGRA（自上而下）→ RGBA，并把 alpha 写死 255（截屏是不透明的）。
pub fn bgra_to_opaque_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for pixel in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
    }
    rgba
}

/// 由 BGRA 位图直接得到一张截图（C# `EncodePng(finalBmp)` 的语义）。
pub fn captured_from_bgra(width: i32, height: i32, bgra: &[u8]) -> Option<CapturedImage> {
    let image = PixelImage::new(width, height, bgra_to_opaque_rgba(bgra))?;
    let png = encode_png(&image);
    Some(CapturedImage::new(png, width, height))
}

/// 由 RGBA 位图得到一张截图。
pub fn captured_from_rgba(width: i32, height: i32, rgba: &[u8]) -> Option<CapturedImage> {
    let image = PixelImage::new(width, height, rgba.to_vec())?;
    let png = encode_png(&image);
    Some(CapturedImage::new(png, width, height))
}

/// 把截图写成剪贴板用的 `CF_DIB`：`BITMAPINFOHEADER` + 自下而上的 32 位 BGRA。
pub fn build_dib(image: &PixelImage) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(40 + image.rgba.len());
    let mut header = [0u8; 40];
    header[0..4].copy_from_slice(&40u32.to_le_bytes()); // biSize
    header[4..8].copy_from_slice(&image.width.to_le_bytes()); // biWidth
    // 正的 biHeight = 自下而上。
    header[8..12].copy_from_slice(&image.height.to_le_bytes());
    header[12..14].copy_from_slice(&1u16.to_le_bytes()); // biPlanes
    header[14..16].copy_from_slice(&32u16.to_le_bytes()); // biBitCount
    header[20..24].copy_from_slice(&(image.width * image.height * 4).to_le_bytes()); // biSizeImage
    bytes.extend_from_slice(&header);
    for row in (0..image.height).rev() {
        let start = row as usize * image.width as usize * 4;
        for pixel in image.rgba[start..start + image.width as usize * 4].chunks_exact(4) {
            bytes.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
    }
    bytes
}

// ---------------------------------------------------------------------------
// PNG（stored-block deflate；不引第三方 crate）
// ---------------------------------------------------------------------------

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// RGBA8 图像 → PNG（颜色类型 6、无滤波、stored deflate）。
pub fn encode_png(image: &PixelImage) -> Vec<u8> {
    let mut raw = Vec::with_capacity(image.rgba.len() + image.height as usize);
    for row in 0..image.height {
        raw.push(0); // filter: None
        let start = row as usize * image.width as usize * 4;
        raw.extend_from_slice(&image.rgba[start..start + image.width as usize * 4]);
    }

    let mut png = Vec::with_capacity(raw.len() + 64);
    png.extend_from_slice(&PNG_SIGNATURE);
    write_chunk(&mut png, b"IHDR", &ihdr(image.width, image.height));
    write_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    write_chunk(&mut png, b"IEND", &[]);
    png
}

fn ihdr(width: i32, height: i32) -> Vec<u8> {
    let mut data = Vec::with_capacity(13);
    data.extend_from_slice(&(width as u32).to_be_bytes());
    data.extend_from_slice(&(height as u32).to_be_bytes());
    data.push(8); // bit depth
    data.push(6); // color type: RGBA
    data.push(0); // compression
    data.push(0); // filter
    data.push(0); // interlace
    data
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// zlib 包装的 stored deflate（每个块最多 65535 字节）。
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 65535 * 5 + 16);
    out.extend_from_slice(&[0x78, 0x01]); // CMF/FLG：deflate、无字典、最快
    if raw.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    } else {
        let mut offset = 0;
        while offset < raw.len() {
            let take = (raw.len() - offset).min(65535);
            let last = offset + take == raw.len();
            out.push(if last { 0x01 } else { 0x00 });
            out.extend_from_slice(&(take as u16).to_le_bytes());
            out.extend_from_slice(&(!(take as u16)).to_le_bytes());
            out.extend_from_slice(&raw[offset..offset + take]);
            offset += take;
        }
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

fn crc_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 != 0 { 0xEDB8_8320 ^ (value >> 1) } else { value >> 1 };
            }
            *slot = value;
        }
        table
    })
}

fn crc32(data: &[u8]) -> u32 {
    let table = crc_table();
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = table[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// PNG → RGBA8（只支持 8 位、非隔行的灰度/RGB/RGBA，stored deflate）。
pub fn decode_png(bytes: &[u8]) -> Option<PixelImage> {
    if !bytes.starts_with(&PNG_SIGNATURE) {
        return None;
    }
    let mut offset = PNG_SIGNATURE.len();
    let mut width = 0i32;
    let mut height = 0i32;
    let mut color_type = 0u8;
    let mut idat = Vec::new();
    let mut saw_header = false;

    while offset + 12 <= bytes.len() {
        let length = read_u32(bytes, offset)? as usize;
        let kind = bytes.get(offset + 4..offset + 8)?;
        let data = bytes.get(offset + 8..offset + 8 + length)?;
        let crc = read_u32(bytes, offset + 8 + length)?;
        let mut crc_input = Vec::with_capacity(4 + length);
        crc_input.extend_from_slice(kind);
        crc_input.extend_from_slice(data);
        if crc32(&crc_input) != crc {
            return None;
        }
        match kind {
            b"IHDR" => {
                if length != 13 {
                    return None;
                }
                width = read_u32(data, 0)? as i32;
                height = read_u32(data, 4)? as i32;
                if data[8] != 8 || data[12] != 0 {
                    return None; // 只认 8 位、非隔行
                }
                color_type = data[9];
                if !matches!(color_type, 0 | 2 | 4 | 6) {
                    return None;
                }
                saw_header = true;
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        offset += 12 + length;
    }
    if !saw_header || idat.is_empty() || width <= 0 || height <= 0 {
        return None;
    }

    let channels = match color_type {
        0 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => return None,
    };
    let stride = width as usize * channels;
    let raw = inflate_stored(&idat)?;
    if raw.len() < (stride + 1) * height as usize {
        return None;
    }

    let mut pixels = vec![0u8; stride * height as usize];
    for row in 0..height as usize {
        let filter = raw[row * (stride + 1)];
        let source = &raw[row * (stride + 1) + 1..(row + 1) * (stride + 1)];
        let start = row * stride;
        if row == 0 {
            unfilter_row(filter, source, None, &mut pixels[start..start + stride], channels)?;
        } else {
            let (before, after) = pixels.split_at_mut(start);
            let previous = &before[start - stride..start];
            unfilter_row(
                filter,
                source,
                Some(previous),
                &mut after[..stride],
                channels,
            )?;
        }
    }

    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for pixel in pixels.chunks_exact(channels) {
        match channels {
            1 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]),
            2 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]),
            3 => rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
            _ => rgba.extend_from_slice(pixel),
        }
    }
    PixelImage::new(width, height, rgba)
}

/// 还原一行 PNG 滤波（`bpp` 是每像素字节数）。
fn unfilter_row(
    filter: u8,
    source: &[u8],
    previous: Option<&[u8]>,
    out: &mut [u8],
    bpp: usize,
) -> Option<()> {
    for index in 0..out.len() {
        let raw = source[index];
        let left = if index >= bpp { out[index - bpp] } else { 0 };
        let up = previous.map_or(0, |row| row[index]);
        let up_left = if index >= bpp {
            previous.map_or(0, |row| row[index - bpp])
        } else {
            0
        };
        out[index] = match filter {
            0 => raw,
            1 => raw.wrapping_add(left),
            2 => raw.wrapping_add(up),
            3 => raw.wrapping_add(((left as u16 + up as u16) / 2) as u8),
            4 => raw.wrapping_add(paeth(left, up, up_left)),
            _ => return None,
        };
    }
    Some(())
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let pa = (p - a as i16).abs();
    let pb = (p - b as i16).abs();
    let pc = (p - c as i16).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// zlib stored 流解压（只认 BTYPE=0；截图都是自己编的）。
fn inflate_stored(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 6 {
        return None;
    }
    if data[0] & 0x0F != 8 {
        return None;
    }
    let mut offset = 2usize;
    let mut out = Vec::new();
    loop {
        let header = *data.get(offset)?;
        offset += 1;
        if header & 0b110 != 0 {
            return None; // 不支持动态/固定 Huffman
        }
        let length = u16::from_le_bytes([*data.get(offset)?, *data.get(offset + 1)?]) as usize;
        let check = u16::from_le_bytes([*data.get(offset + 2)?, *data.get(offset + 3)?]);
        if (length as u16) != !check {
            return None;
        }
        offset += 4;
        out.extend_from_slice(data.get(offset..offset + length)?);
        offset += length;
        if header & 0b1 == 1 {
            break;
        }
    }
    let expected = read_be32(data, data.len() - 4)?;
    if adler32(&out) != expected {
        return None;
    }
    Some(out)
}

fn read_be32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

// ---------------------------------------------------------------------------
// 平台接口
// ---------------------------------------------------------------------------

/// 截图需要的系统能力；真机实现是 [`WinScreens`]。
pub trait ScreenshotSystem: Send + Sync {
    /// 窗口还在时返回它的 pid；不在了返回 `None`。
    fn window_pid(&self, handle: u64) -> Option<i32>;
    /// `GetWindowRect`。
    fn window_rect(&self, handle: u64) -> Option<Rect>;
    /// DWM 可见边框（取不到返回 `None`，调用方退回 `window_rect`）。
    fn visible_bounds(&self, handle: u64) -> Option<Rect>;
    /// `PrintWindow`：返回自上而下的 BGRA，`width * height * 4` 字节。
    fn print_window(&self, handle: u64, width: i32, height: i32) -> Option<Vec<u8>>;
    /// 屏幕区域 BitBlt：同样返回 BGRA。
    fn copy_from_screen(&self, left: i32, top: i32, width: i32, height: i32) -> Option<Vec<u8>>;
    /// 把 `CF_DIB` 与 `PNG` 两个格式放进剪贴板。
    fn write_clipboard(&self, dib: &[u8], png: &[u8]) -> bool;
}

// ---------------------------------------------------------------------------
// 截图服务
// ---------------------------------------------------------------------------

/// 截图服务（C# `ScreenshotService`）。界面层拿它实现 core 的 `ScreenshotControl`。
pub struct WinScreenshot {
    system: Box<dyn ScreenshotSystem>,
}

impl WinScreenshot {
    pub fn new() -> Self {
        Self::with_system(Box::new(WinScreens))
    }

    /// 换一套系统实现（测试用）。
    pub fn with_system(system: Box<dyn ScreenshotSystem>) -> Self {
        Self { system }
    }

    /// 抓某个窗口（C# `CaptureAsync`）：先 `PrintWindow`，失败退屏幕 BitBlt。
    pub fn capture_window(&self, window: WindowKey) -> Result<CapturedImage, CaptureError> {
        let pid = self
            .system
            .window_pid(window.handle)
            .ok_or(CaptureError::new(CaptureFailure::WindowGone))?;
        if pid != window.pid {
            return Err(CaptureError::new(CaptureFailure::WindowGone));
        }
        let raw = self
            .system
            .window_rect(window.handle)
            .ok_or(CaptureError::new(CaptureFailure::Unavailable))?;
        if raw.width <= 0.0 || raw.height <= 0.0 {
            return Err(CaptureError::new(CaptureFailure::Unavailable));
        }
        let visible = self.system.visible_bounds(window.handle).unwrap_or(raw);
        let width = raw.width.round() as i32;
        let height = raw.height.round() as i32;

        if let Some(bgra) = self.system.print_window(window.handle, width, height) {
            if bgra.len() == width as usize * height as usize * 4 {
                let (x, y, crop_width, crop_height) = visible_crop(visible, raw, width, height);
                let image = PixelImage::new(width, height, bgra_to_opaque_rgba(&bgra))
                    .ok_or(CaptureError::new(CaptureFailure::Unavailable))?;
                let cropped = crop_pixels(&image, x, y, crop_width, crop_height)
                    .ok_or(CaptureError::new(CaptureFailure::Unavailable))?;
                return Ok(CapturedImage::new(encode_png(&cropped), crop_width, crop_height));
            }
        }

        // PrintWindow 失败（C# 的 `CopyFromScreen` 分支）。
        let width = 1.max(round_ties_even(visible.width) as i32);
        let height = 1.max(round_ties_even(visible.height) as i32);
        let left = round_ties_even(visible.x) as i32;
        let top = round_ties_even(visible.y) as i32;
        let bgra = self
            .system
            .copy_from_screen(left, top, width, height)
            .ok_or(CaptureError::new(CaptureFailure::Unavailable))?;
        captured_from_bgra(width, height, &bgra).ok_or(CaptureError::new(CaptureFailure::Unavailable))
    }

    /// 抓屏幕上一块区域（截图键的"整屏"路径）。
    pub fn capture_rect(&self, rect: Rect) -> Option<CapturedImage> {
        if rect.width < 1.0 || rect.height < 1.0 {
            return None;
        }
        let width = round_ties_even(rect.width) as i32;
        let height = round_ties_even(rect.height) as i32;
        let left = round_ties_even(rect.x) as i32;
        let top = round_ties_even(rect.y) as i32;
        let bgra = self.system.copy_from_screen(left, top, width, height)?;
        captured_from_bgra(width, height, &bgra)
    }
}

impl Default for WinScreenshot {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenshotControl for WinScreenshot {
    /// Windows 上截图不需要额外授权（C# `Trusted => true`）。
    fn trusted(&self) -> bool {
        true
    }

    fn capture(&self, window: WindowKey) -> Result<CapturedImage, CaptureError> {
        self.capture_window(window)
    }

    /// C# `Crop`：从 PNG 解码 → clamp → 裁 → 重新编码。
    fn crop(&self, image: &CapturedImage, rect: Rect) -> Option<CapturedImage> {
        if rect.width < 1.0 || rect.height < 1.0 {
            return None;
        }
        let source = decode_png(&image.png)?;
        let x = clamp_int(round_ties_even(rect.x) as i32, 0, source.width - 1);
        let y = clamp_int(round_ties_even(rect.y) as i32, 0, source.height - 1);
        let width = clamp_int(round_ties_even(rect.width) as i32, 1, source.width - x);
        let height = clamp_int(round_ties_even(rect.height) as i32, 1, source.height - y);
        let cropped = crop_pixels(&source, x, y, width, height)?;
        Some(CapturedImage::new(encode_png(&cropped), width, height))
    }

    /// C# `Copy`：解码 PNG → 写 `CF_DIB` + `PNG`，失败重试 3 次（每次 40ms）。
    fn copy(&self, image: &CapturedImage) -> bool {
        let Some(decoded) = decode_png(&image.png) else {
            crate::warn("写入剪贴板失败：截图 PNG 解不开");
            return false;
        };
        let dib = build_dib(&decoded);
        for attempt in 0..3 {
            if self.system.write_clipboard(&dib, &image.png) {
                return true;
            }
            if attempt < 2 {
                std::thread::sleep(Duration::from_millis(40));
            }
        }
        crate::warn("写入剪贴板失败：剪贴板被占用");
        false
    }
}

// ---------------------------------------------------------------------------
// 真机实现
// ---------------------------------------------------------------------------

/// 抓取虚拟桌面坐标内的一帧画面。GDI 先缩放，再读出不透明 RGBA 像素。
pub fn capture_projection(rect: Rect) -> Option<PixelImage> {
    let [left, top, source_width, source_height] = [rect.x, rect.y, rect.width, rect.height]
        .map(f64::round);
    let coord_range = f64::from(i32::MIN)..=f64::from(i32::MAX);
    if ![left, top, source_width, source_height].iter().all(|v| coord_range.contains(v))
        || source_width < 1.0 || source_height < 1.0
    {
        return None;
    }
    let left = left as i32;
    let top = top as i32;
    let source_width = source_width as i32;
    let source_height = source_height as i32;
    left.checked_add(source_width)?;
    top.checked_add(source_height)?;
    let (width, height) = projection_size(source_width, source_height);
    let rgba = WinScreens::capture_bitmap(
        width,
        height,
        |memory, screen| unsafe {
            let layered = SRCCOPY | CAPTUREBLT;
            if width == source_width && height == source_height {
                return BitBlt(memory, 0, 0, width, height, Some(screen), left, top, layered).is_ok();
            }
            if SetStretchBltMode(memory, HALFTONE) == 0
                || !SetBrushOrgEx(memory, 0, 0, None).as_bool()
            {
                return false;
            }
            StretchBlt(
                memory, 0, 0, width, height, Some(screen), left, top, source_width, source_height,
                layered,
            ).as_bool()
        },
        bgra_to_opaque_rgba,
    )?;
    PixelImage::new(width, height, rgba)
}

fn projection_size(width: i32, height: i32) -> (i32, i32) {
    const MAX_EDGE: f64 = 1280.0;
    let scale = (MAX_EDGE / f64::from(width.max(height))).min(1.0);
    let scaled = |edge: i32| (f64::from(edge) * scale).round().max(1.0) as i32;
    (scaled(width), scaled(height))
}

/// 真机截图后端。
pub struct WinScreens;

impl WinScreens {
    /// 创建自上而下的 32 位 DIB，将设备上下文交给 `draw`，再读取同步后的 BGRA。
    fn capture_bitmap<T>(
        width: i32,
        height: i32,
        draw: impl FnOnce(HDC, HDC) -> bool,
        read: impl FnOnce(&[u8]) -> T,
    ) -> Option<T> {
        if width <= 0 || height <= 0 {
            return None;
        }
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let memory = CreateCompatibleDC(Some(screen));
            let mut result = None;
            if !memory.is_invalid() {
                let mut info = BITMAPINFO::default();
                info.bmiHeader = BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height, // 自上而下
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0,
                    ..Default::default()
                };
                let mut bits: *mut c_void = std::ptr::null_mut();
                if let Ok(bitmap) =
                    CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                {
                    let old = SelectObject(memory, HGDIOBJ(bitmap.0));
                    if !old.is_invalid() && draw(memory, screen) && GdiFlush().as_bool() && !bits.is_null() {
                        let length = width as usize * height as usize * 4;
                        result = Some(read(std::slice::from_raw_parts(bits as *const u8, length)));
                    }
                    if !old.is_invalid() {
                        SelectObject(memory, old);
                    }
                    let _ = DeleteObject(HGDIOBJ(bitmap.0));
                }
                let _ = DeleteDC(memory);
            }
            ReleaseDC(None, screen);
            result
        }
    }

    fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> bool {
        unsafe {
            let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, bytes.len()) else {
                return false;
            };
            let pointer = GlobalLock(memory);
            if pointer.is_null() {
                let _ = GlobalFree(Some(memory));
                return false;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
            let _ = GlobalUnlock(memory);
            if SetClipboardData(format, Some(HANDLE(memory.0))).is_ok() {
                // 所有权已经交给剪贴板，不能再 GlobalFree。
                true
            } else {
                let _ = GlobalFree(Some(memory));
                false
            }
        }
    }
}

impl ScreenshotSystem for WinScreens {
    fn window_pid(&self, handle: u64) -> Option<i32> {
        use crate::window::WindowApi;
        WindowApi::probe(&crate::window::WinDesktop, handle).map(|probe| probe.pid)
    }

    fn window_rect(&self, handle: u64) -> Option<Rect> {
        use crate::window::WindowApi;
        WindowApi::probe(&crate::window::WinDesktop, handle).map(|probe| probe.rect)
    }

    fn visible_bounds(&self, handle: u64) -> Option<Rect> {
        use crate::window::WindowApi;
        WindowApi::visible_bounds(&crate::window::WinDesktop, handle)
    }

    fn print_window(&self, handle: u64, width: i32, height: i32) -> Option<Vec<u8>> {
        let hwnd = HWND(handle as usize as *mut c_void);
        Self::capture_bitmap(
            width,
            height,
            |memory, _| unsafe {
                PrintWindow(hwnd, memory, PW_RENDERFULLCONTENT).as_bool()
                    || PrintWindow(hwnd, memory, 0).as_bool()
            },
            <[u8]>::to_vec,
        )
    }

    fn copy_from_screen(&self, left: i32, top: i32, width: i32, height: i32) -> Option<Vec<u8>> {
        Self::capture_bitmap(
            width,
            height,
            |memory, screen| unsafe {
                BitBlt(memory, 0, 0, width, height, Some(screen), left, top, SRCCOPY).is_ok()
            },
            <[u8]>::to_vec,
        )
    }

    fn write_clipboard(&self, dib: &[u8], png: &[u8]) -> bool {
        unsafe {
            if OpenClipboard(None).is_err() {
                return false;
            }
            let mut ok = EmptyClipboard().is_ok();
            if ok {
                ok = Self::set_clipboard_bytes(CF_DIB.0 as u32, dib);
            }
            if ok {
                let png_format = RegisterClipboardFormatW(w!("PNG"));
                if png_format != 0 {
                    ok = Self::set_clipboard_bytes(png_format, png);
                }
            }
            let _ = CloseClipboard();
            ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn projection_caps_either_orientation_without_upscaling() {
        assert_eq!(projection_size(1080, 1920), (720, 1280));
        assert_eq!(projection_size(3840, 2160), (1280, 720));
        assert_eq!(projection_size(360, 640), (360, 640));
        assert_eq!(projection_size(1, 4096), (1, 1280));
        assert!(capture_projection(Rect::new(0.0, 0.0, 12.0, f64::NAN)).is_none());
        assert!(capture_projection(Rect::new(f64::from(i32::MAX), 0.0, 12.0, 12.0)).is_none());
    }

    fn window(pid: i32, handle: u64) -> WindowKey {
        WindowKey::new(pid, handle)
    }

    fn solid(width: i32, height: i32, bgra: [u8; 4]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..width * height {
            bytes.extend_from_slice(&bgra);
        }
        bytes
    }

    // ---- PNG ----

    #[test]
    fn png_roundtrip_keeps_pixels() {
        let mut rgba = Vec::new();
        for pixel in 0..4u8 {
            rgba.extend_from_slice(&[pixel * 10, pixel * 20, pixel * 30, 255]);
        }
        let image = PixelImage::new(2, 2, rgba).unwrap();
        let png = encode_png(&image);
        assert!(png.starts_with(&PNG_SIGNATURE));
        let back = decode_png(&png).expect("decode");
        assert_eq!(back, image);
    }

    #[test]
    fn png_uses_rgba_and_valid_chunk_crcs() {
        let image = PixelImage::new(1, 1, vec![1, 2, 3, 4]).unwrap();
        let png = encode_png(&image);
        // IHDR：宽高、8 位、颜色类型 6。
        assert_eq!(png[12..16], *b"IHDR");
        assert_eq!(&png[16..20], &1u32.to_be_bytes());
        assert_eq!(&png[20..24], &1u32.to_be_bytes());
        assert_eq!(png[24], 8);
        assert_eq!(png[25], 6);
        // 每个块的 CRC 都对得上。
        let mut offset = 8;
        let mut chunks = 0;
        while offset + 12 <= png.len() {
            let length = u32::from_be_bytes(png[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = &png[offset + 4..offset + 8];
            let mut input = kind.to_vec();
            input.extend_from_slice(&png[offset + 8..offset + 8 + length]);
            let expected = u32::from_be_bytes(
                png[offset + 8 + length..offset + 12 + length].try_into().unwrap(),
            );
            assert_eq!(crc32(&input), expected, "{:?}", String::from_utf8_lossy(kind));
            chunks += 1;
            if kind == b"IEND" {
                break;
            }
            offset += 12 + length;
        }
        assert_eq!(chunks, 3, "IHDR + IDAT + IEND");
    }

    #[test]
    fn decode_rejects_broken_input() {
        assert_eq!(decode_png(&[]), None);
        assert_eq!(decode_png(b"not a png at all"), None);
        let image = PixelImage::new(2, 2, vec![0; 16]).unwrap();
        let mut png = encode_png(&image);
        png.truncate(20);
        assert_eq!(decode_png(&png), None);
        // 改坏一个 IDAT 字节 → CRC 不匹配。
        let mut png = encode_png(&image);
        let index = png.len() - 20;
        png[index] ^= 0xFF;
        assert_eq!(decode_png(&png), None);
    }

    // ---- 裁剪数学 ----

    #[test]
    fn rounding_matches_dotnet_math_round() {
        assert_eq!(round_ties_even(0.5), 0.0);
        assert_eq!(round_ties_even(1.5), 2.0);
        assert_eq!(round_ties_even(2.5), 2.0);
        assert_eq!(round_ties_even(-0.5), 0.0);
        assert_eq!(round_ties_even(-1.5), -2.0);
        assert_eq!(round_ties_even(2.4), 2.0);
        assert_eq!(round_ties_even(2.6), 3.0);
    }

    #[test]
    fn visible_crop_clamps_like_csharp() {
        let raw = Rect::new(100.0, 100.0, 10.0, 10.0);
        // 正常阴影：右/下各缩 1。
        assert_eq!(
            visible_crop(Rect::new(101.0, 101.0, 8.0, 8.0), raw, 10, 10),
            (1, 1, 8, 8)
        );
        // 可见边框跑到窗口外：clamp 回位图内。
        assert_eq!(
            visible_crop(Rect::new(90.0, 90.0, 40.0, 40.0), raw, 10, 10),
            (0, 0, 10, 10)
        );
        // 尺寸为 0 也至少留 1×1。
        assert_eq!(visible_crop(Rect::new(100.0, 100.0, 0.0, 0.0), raw, 10, 10), (0, 0, 1, 1));
        // 小数按 Math.Round。
        assert_eq!(
            visible_crop(Rect::new(100.5, 100.5, 9.5, 9.5), raw, 10, 10),
            (0, 0, 10, 10)
        );
    }

    // ---- 假系统 ----

    #[derive(Default)]
    struct FakeScreens {
        pid: Option<i32>,
        rect: Option<Rect>,
        visible: Option<Rect>,
        printed: Option<Vec<u8>>,
        screen: Option<Vec<u8>>,
        clipboard_failures: Arc<Mutex<usize>>,
        clipboard_calls: Arc<Mutex<Vec<(usize, usize)>>>,
        screen_calls: Arc<Mutex<Vec<(i32, i32, i32, i32)>>>,
    }

    impl ScreenshotSystem for FakeScreens {
        fn window_pid(&self, _handle: u64) -> Option<i32> {
            self.pid
        }

        fn window_rect(&self, _handle: u64) -> Option<Rect> {
            self.rect
        }

        fn visible_bounds(&self, _handle: u64) -> Option<Rect> {
            self.visible
        }

        fn print_window(&self, _handle: u64, _width: i32, _height: i32) -> Option<Vec<u8>> {
            self.printed.clone()
        }

        fn copy_from_screen(&self, left: i32, top: i32, width: i32, height: i32) -> Option<Vec<u8>> {
            self.screen_calls.lock().unwrap().push((left, top, width, height));
            self.screen.clone()
        }

        fn write_clipboard(&self, dib: &[u8], png: &[u8]) -> bool {
            self.clipboard_calls.lock().unwrap().push((dib.len(), png.len()));
            let mut failures = self.clipboard_failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return false;
            }
            true
        }
    }

    #[test]
    fn capture_requires_live_window_with_matching_pid() {
        let mut fake = FakeScreens::default();
        fake.pid = Some(7);
        let service = WinScreenshot::with_system(Box::new(fake));
        assert_eq!(service.capture_window(window(8, 1)).unwrap_err().failure, CaptureFailure::WindowGone);

        let service = WinScreenshot::with_system(Box::new(FakeScreens::default()));
        assert_eq!(service.capture_window(window(1, 1)).unwrap_err().failure, CaptureFailure::WindowGone);

        let mut fake = FakeScreens::default();
        fake.pid = Some(7);
        fake.rect = Some(Rect::new(0.0, 0.0, 0.0, 10.0));
        let service = WinScreenshot::with_system(Box::new(fake));
        assert_eq!(
            service.capture_window(window(7, 1)).unwrap_err().failure,
            CaptureFailure::Unavailable
        );
    }

    #[test]
    fn capture_crops_print_window_to_visible_bounds() {
        // 10×10 红窗，可见区域从 (1,2) 起 6×5。
        let mut fake = FakeScreens::default();
        fake.pid = Some(7);
        fake.rect = Some(Rect::new(100.0, 100.0, 10.0, 10.0));
        fake.visible = Some(Rect::new(101.0, 102.0, 6.0, 5.0));
        fake.printed = Some(solid(10, 10, [0, 0, 255, 0]));
        let service = WinScreenshot::with_system(Box::new(fake));

        let image = service.capture_window(window(7, 1)).expect("capture");
        assert_eq!((image.pixel_width, image.pixel_height), (6, 5));
        let decoded = decode_png(&image.png).expect("decode");
        assert_eq!((decoded.width, decoded.height), (6, 5));
        assert_eq!(decoded.pixel(0, 0), Some([255, 0, 0, 255]), "BGRA 转 RGBA，alpha 补齐");
    }

    #[test]
    fn capture_falls_back_to_screen_when_print_window_fails() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeScreens {
            pid: Some(7),
            rect: Some(Rect::new(100.0, 100.0, 10.0, 10.0)),
            // 小数按 Math.Round：10.5 → 10，3.5 → 4，2.5 → 2。
            visible: Some(Rect::new(10.5, 20.5, 3.5, 2.5)),
            screen: Some(solid(4, 2, [1, 2, 3, 0])),
            screen_calls: calls.clone(),
            ..Default::default()
        };
        let service = WinScreenshot::with_system(Box::new(fake));

        let image = service.capture_window(window(7, 1)).expect("capture");
        assert_eq!((image.pixel_width, image.pixel_height), (4, 2));
        assert_eq!(*calls.lock().unwrap(), vec![(10, 20, 4, 2)]);
        let decoded = decode_png(&image.png).unwrap();
        assert_eq!(decoded.pixel(0, 0), Some([3, 2, 1, 255]));
    }

    #[test]
    fn capture_rect_uses_rounded_screen_region() {
        let mut fake = FakeScreens::default();
        fake.screen = Some(solid(2, 3, [9, 8, 7, 0]));
        let service = WinScreenshot::with_system(Box::new(fake));
        let image = service.capture_rect(Rect::new(5.5, 6.5, 2.4, 2.6)).expect("capture");
        assert_eq!((image.pixel_width, image.pixel_height), (2, 3));
        assert!(service.capture_rect(Rect::new(0.0, 0.0, 0.4, 2.0)).is_none());
    }

    #[test]
    fn crop_clamps_and_rejects_bad_input() {
        let rgba: Vec<u8> = (0..16).flat_map(|pixel| [pixel as u8, 0, 0, 255]).collect();
        let image = CapturedImage::new(encode_png(&PixelImage::new(4, 4, rgba).unwrap()), 4, 4);
        let service = WinScreenshot::with_system(Box::new(FakeScreens::default()));

        let cropped = service.crop(&image, Rect::new(1.0, 1.0, 2.0, 2.0)).expect("crop");
        let decoded = decode_png(&cropped.png).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.pixel(0, 0).unwrap()[0], 5);

        // 超出右下角：clamp 到边界。
        let cropped = service.crop(&image, Rect::new(3.5, 3.5, 10.0, 10.0)).expect("crop");
        assert_eq!((cropped.pixel_width, cropped.pixel_height), (1, 1));

        // 宽高小于 1 直接拒绝。
        assert!(service.crop(&image, Rect::new(0.0, 0.0, 0.5, 4.0)).is_none());

        // 坏 PNG：不 panic。
        let broken = CapturedImage::new(vec![0, 1, 2], 4, 4);
        assert!(service.crop(&broken, Rect::new(0.0, 0.0, 2.0, 2.0)).is_none());
    }

    #[test]
    fn copy_writes_dib_and_png_and_retries() {
        let rgba = vec![10u8, 20, 30, 255, 40, 50, 60, 255];
        let image = CapturedImage::new(encode_png(&PixelImage::new(2, 1, rgba).unwrap()), 2, 1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeScreens {
            clipboard_failures: Arc::new(Mutex::new(2)),
            clipboard_calls: calls.clone(),
            ..Default::default()
        };
        let service = WinScreenshot::with_system(Box::new(fake));

        let dib = {
            let decoded = decode_png(&image.png).unwrap();
            build_dib(&decoded)
        };
        assert_eq!(dib.len(), 40 + 2 * 1 * 4);
        assert_eq!(u32::from_le_bytes(dib[0..4].try_into().unwrap()), 40);
        assert_eq!(i32::from_le_bytes(dib[4..8].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()), 1, "正的 biHeight = 自下而上");
        assert_eq!(u16::from_le_bytes(dib[14..16].try_into().unwrap()), 32);
        assert_eq!(dib[40..44], [30, 20, 10, 255], "RGBA → BGRA、alpha 255");

        assert!(service.copy(&image));
        let expected = vec![(dib.len(), image.png.len()); 3];
        assert_eq!(*calls.lock().unwrap(), expected);
    }

    #[test]
    fn copy_gives_up_after_three_attempts_and_never_panics_on_bad_png() {
        let image = CapturedImage::new(encode_png(&PixelImage::new(1, 1, vec![1, 2, 3, 4]).unwrap()), 1, 1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeScreens {
            clipboard_failures: Arc::new(Mutex::new(99)),
            clipboard_calls: calls.clone(),
            ..Default::default()
        };
        let service = WinScreenshot::with_system(Box::new(fake));
        assert!(!service.copy(&image));
        assert_eq!(calls.lock().unwrap().len(), 3);

        let service = WinScreenshot::with_system(Box::new(FakeScreens::default()));
        assert!(!service.copy(&CapturedImage::new(vec![9, 9], 1, 1)));
    }
}
