//! 复制通知气泡：桌面右下角圆角卡片，不抢焦点、鼠标穿透，显示约 1.5 秒后平滑淡出。
//! 外观参考 QcClip 的通知窗口（去掉进度条、关闭按钮和图标），宽度按文字内容自适应。
//!
//! 动画由常驻的独立线程以 60fps 恒定步进驱动：窗口常驻复用，绘制表面在尺寸不变时复用，
//! 文本只在通知生成时排版/绘制一次；动画期间只更新分层窗口的位置与整体 alpha，
//! 不做整窗重绘，也不碰 UI 消息线程，所以不会因为剪贴板事件/写日志而掉帧。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use windows_sys::core::w;
use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, POINT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetDeviceCaps,
    GetMonitorInfoW, MonitorFromPoint, SelectObject, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, BI_RGB, DIB_RGB_COLORS, HDC, HGDIOBJ, LOGPIXELSX, MONITORINFO,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows_sys::Win32::Graphics::GdiPlus::{
    GdipCreateBitmapFromScan0, GdipCreateFont, GdipCreateFontFamilyFromName, GdipCreateSolidFill,
    GdipCreateStringFormat, GdipDeleteBrush, GdipDeleteFont, GdipDeleteFontFamily,
    GdipDeleteGraphics, GdipDeleteStringFormat, GdipDisposeImage, GdipDrawString,
    GdipGetImageGraphicsContext, GdipMeasureString, GdipSetSmoothingMode,
    GdipSetStringFormatFlags, GdipSetStringFormatTrimming, GdipSetTextRenderingHint,
    GdiplusStartup, GdiplusStartupInput, GdiplusStartupOutput, GpBitmap, GpBrush, GpFont,
    GpFontFamily, GpGraphics, GpImage, GpSolidFill, GpStringFormat, RectF, SmoothingModeAntiAlias,
    StringTrimmingEllipsisCharacter, TextRenderingHintAntiAliasGridFit, UnitPixel,
};
use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, PostMessageW, RegisterClassW, ShowWindow, UpdateLayeredWindow,
    SW_HIDE, SW_SHOWNOACTIVATE, WM_APP, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// 复制提示音：由 QcClip 的 BrightPing 波形（复制默认音效）生成，
/// 内嵌 WAV 以 50% 音量（WAV_BASE_VOLUME）为基准；实际音量按配置 volume/50 缩放，
/// 默认配置 70。波形以二进制内嵌进 exe，放在任意目录双击运行都能播放。
static COPY_SOUND_WAV: &[u8] = include_bytes!("copy.wav");
/// 内嵌波形本身的基准音量；配置音量按 volume / WAV_BASE_VOLUME 调整增益。
const WAV_BASE_VOLUME: u32 = 50;
/// RIFF/WAVE 头长度，块遍历从这里开始。
const WAV_HEADER_LEN: usize = 12;
/// `dib_has_alpha` 最多抽查的像素数，避免大图逐像素扫描。
const ALPHA_SAMPLE_BUDGET: usize = 200_000;

/// 由主线程消息循环分发的"有新通知"消息。
pub const WM_APP_TOAST: u32 = WM_APP + 1;

// ---- 卡片排版（逻辑像素，随 DPI 缩放）----
/// 窄内容的最小宽度，保证短内容是一条紧凑的小条而不是半块卡片。
const CARD_MIN_W: f32 = 100.0;
const PAD_X: f32 = 10.0;
const PAD_Y: f32 = 9.0;
const FONT_SIZE: f32 = 14.0;
const LINE_H: f32 = 20.0;
const RADIUS: f32 = 10.0;
/// 单行自然宽度之外额外的宽度余量，避免取整后触发省略号。
const WIDTH_SLACK: f32 = 2.0;
/// 墨迹扫描时判定“有像素”的 alpha 阈值（128 = 50% 覆盖率）。
const INK_ALPHA_THRESHOLD: u8 = 128;
/// 阴影只留贴边一圈淡影，窗口四周的透明留白也随之收窄。
const SHADOW: f32 = 8.0;
const SCREEN_MARGIN: f32 = 16.0;

/// 图片缩略图的最大显示框（逻辑像素，随 DPI 缩放）。小图按原始像素展示，不放大。
const THUMB_MAX_W: f32 = 240.0;
const THUMB_MAX_H: f32 = 160.0;
/// 缩略图圆角比卡片略小，配合 alpha 裁切让图片边缘干净、不硬切。
const THUMB_RADIUS_INSET: f32 = 2.0;
/// 文件图片的安全上限：超过任一限制就不整图解码，回退「已复制文件」，避免内存爆掉。
/// 像素上限 8000 万（与旧版一致）：8000 万像素 RGBA 解码缓冲约 320MB，文件名下不落盘。
const IMAGE_FILE_MAX_BYTES: u64 = 50 * 1024 * 1024;
const IMAGE_FILE_MAX_PIXELS: u64 = 80_000_000;

// ---- 动画参数 ----
const FADE_IN_MS: u32 = 180;
const FADE_OUT_MS: u32 = 200;
const SLIDE: f32 = 36.0;
/// 60fps 恒定步进（16.67ms）。
const FRAME_NS: u64 = 16_666_667;

const AC_SRC_OVER: u8 = 0;
const AC_SRC_ALPHA: u8 = 1;
const ULW_ALPHA: u32 = 2;

/// PixelFormat32bppPARGB：GDI+ 直接按预乘 alpha 写入，正好是分层窗口需要的格式。
const PIXEL_FORMAT_PARGB: i32 = 0x000E_200B;
/// StringFormatFlagsNoWrap：单行时不允许折行，超出就省略。
const STRING_FORMAT_NO_WRAP: i32 = 0x1000;

const DEDUP_MS: u128 = 1200;
const SOUND_THROTTLE_MS: u128 = 350;

static TOAST_HWND: AtomicIsize = AtomicIsize::new(0);
static GDIP_TOKEN: AtomicUsize = AtomicUsize::new(0);
/// 屏幕 DC 只取一次，避免每帧 GetDC/ReleaseDC。
static SCREEN_DC: AtomicIsize = AtomicIsize::new(0);
/// 测量 Graphics 所引用的 1×1 位图句柄；只保存不读取，用于保活，不可删除。
static MEASURE_BMP: AtomicIsize = AtomicIsize::new(0);
static MEASURE_GFX: AtomicIsize = AtomicIsize::new(0);
static SOUND_VOLUME: AtomicU32 = AtomicU32::new(crate::settings::DEFAULT_VOLUME);
static SOUND_BUF: Mutex<Option<Vec<u8>>> = Mutex::new(None);
static CARD_TX: Mutex<Option<Sender<Payload>>> = Mutex::new(None);
static QUEUE: Mutex<VecDeque<Payload>> = Mutex::new(VecDeque::new());
/// 同一内容短时间内只提示一次（小米同步一次复制会连写两次剪贴板）。
static LAST_SHOWN: Mutex<Option<(Instant, String)>> = Mutex::new(None);
/// 音效节流：连续快速复制时只响一次，避免叠成爆音。
static LAST_SOUND: Mutex<Option<Instant>> = Mutex::new(None);

/// 一条通知的内容：文字摘要，或已经缩放好的图片缩略图。
/// `since` 是剪贴板事件发生的时刻，用于记录"复制 → 首帧"耗时。
enum Payload {
    Text { text: String, since: Instant },
    Image { thumb: ImageThumb, since: Instant },
}

impl Payload {
    fn since(&self) -> Instant {
        match self {
            Payload::Text { since, .. } | Payload::Image { since, .. } => *since,
        }
    }
}

/// 剪贴板里读到的原始图片字节（未解码），便于把大图尽快缩小。
pub struct RawImage {
    pub bytes: Vec<u8>,
    /// true = PNG 数据，false = CF_DIB / CF_DIBV5。
    pub png: bool,
}

/// 已缩放到最终显示尺寸的缩略图：预乘 BGRA、自上而下、行距 w*4。
struct ImageThumb {
    w: i32,
    h: i32,
    px: Vec<u8>,
}

/// 绘制时叠加的一层文字。
struct TextLayer<'a> {
    text: &'a str,
    style: &'a TextStyle,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    two_lines: bool,
}

/// 绘制时叠加的一层图片（尺寸已是最终像素尺寸，只做预乘合成 + 圆角 alpha 裁切）。
struct ImageLayer<'a> {
    px: &'a [u8],
    w: i32,
    h: i32,
    x: f32,
    y: f32,
    radius: f32,
}

/// 一块 32 位 DIB，作为分层窗口的绘制表面。尺寸不变时反复复用。
struct Surface {
    dc: HDC,
    bitmap: HGDIOBJ,
    old: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

struct Palette {
    bg: [f32; 3],
    body: [f32; 3],
    shadow_alpha: f32,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            bg: [0.122, 0.122, 0.133],
            body: [0.706, 0.706, 0.741],
            shadow_alpha: 0.38,
        }
    } else {
        Palette {
            bg: [1.0, 1.0, 1.0],
            // 正文近纯黑 #0A0A0A：比 #000000 少一分锐利，但和纯黑肉眼无差，
            // 浅色卡片上不再有“发灰”的观感。
            body: [0.039, 0.039, 0.039],
            shadow_alpha: 0.12,
        }
    }
}

/// 字体、字号、排版格式：一条通知只创建一次，测量和绘制共用。
struct TextStyle {
    family: *mut GpFontFamily,
    font: *mut GpFont,
    /// 单行（不折行、超长省略）
    single: *mut GpStringFormat,
    /// 两行（允许折行、末行省略）
    multi: *mut GpStringFormat,
}

impl Drop for TextStyle {
    fn drop(&mut self) {
        unsafe {
            if !self.single.is_null() {
                GdipDeleteStringFormat(self.single);
            }
            if !self.multi.is_null() {
                GdipDeleteStringFormat(self.multi);
            }
            if !self.font.is_null() {
                GdipDeleteFont(self.font);
            }
            if !self.family.is_null() {
                GdipDeleteFontFamily(self.family);
            }
        }
    }
}

/// 启动 GDI+，读配置生成提示音，启动常驻动画线程（在 create 里）。
pub fn init() {
    unsafe {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            DebugEventCallback: 0,
            SuppressBackgroundThread: 0,
            SuppressExternalCodecs: 0,
        };
        let mut token: usize = 0;
        let mut output: GdiplusStartupOutput = std::mem::zeroed();
        if GdiplusStartup(&mut token, &input, &mut output) == 0 {
            GDIP_TOKEN.store(token, Ordering::SeqCst);
        }
    }
    reload_sound();
}

/// 按当前设置重建提示音波形（volume 改变后由 settingsChanged 立即调用）。
pub fn reload_sound() {
    let volume = crate::settings::volume();
    SOUND_VOLUME.store(volume, Ordering::SeqCst);
    *SOUND_BUF.lock().unwrap_or_else(|e| e.into_inner()) = Some(scale_wav(COPY_SOUND_WAV, volume));
}

/// 创建通知窗口（默认隐藏，鼠标完全穿透），并启动常驻动画线程。
pub unsafe fn create(hinstance: HINSTANCE) -> HWND {
    let class_name = w!("MIConnectdToastWindow");
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(wnd_proc);
    wc.hInstance = hinstance;
    wc.lpszClassName = class_name;
    RegisterClassW(&wc);

    // 分层 + 不激活 + 工具窗口 + 置顶 + 鼠标穿透
    let ex_style = WS_EX_LAYERED
        | WS_EX_NOACTIVATE
        | WS_EX_TOOLWINDOW
        | WS_EX_TOPMOST
        | WS_EX_TRANSPARENT;
    let hwnd = CreateWindowExW(
        ex_style,
        class_name,
        w!("MIConnectd"),
        WS_POPUP,
        0,
        0,
        1,
        1,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        hinstance,
        std::ptr::null(),
    );
    TOAST_HWND.store(hwnd as isize, Ordering::SeqCst);

    let (tx, rx) = mpsc::channel::<Payload>();
    *CARD_TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    let raw = hwnd as isize;
    let _ = std::thread::Builder::new()
        .name("toast-anim".to_string())
        .spawn(move || animation_loop(raw, rx));
    hwnd
}

/// 播放复制提示音（按设置音量，350ms 内连续触发只响一次；sound 关闭时静音）。
fn play_copy_sound() {
    if !crate::settings::sound() {
        return;
    }
    {
        let mut last = LAST_SOUND.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t: Instant| t.elapsed().as_millis() < SOUND_THROTTLE_MS) {
            return;
        }
        *last = Some(Instant::now());
    }
    if SOUND_VOLUME.load(Ordering::SeqCst) == 0 {
        return;
    }
    let guard = SOUND_BUF.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(buf) = guard.as_ref() {
        unsafe {
            PlaySoundW(
                buf.as_ptr() as *const u16,
                std::ptr::null_mut(),
                SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
            );
        }
    }
}

/// 按百分比缩放 16 位 PCM 采样（以 50% 的波形为基准）。
fn scale_wav(wav: &[u8], volume: u32) -> Vec<u8> {
    let mut out = wav.to_vec();
    let gain = volume as f32 / WAV_BASE_VOLUME as f32;
    if (gain - 1.0).abs() < 0.001 {
        return out;
    }
    let mut pos = WAV_HEADER_LEN;
    while pos + 8 <= out.len() {
        let size = u32::from_le_bytes([out[pos + 4], out[pos + 5], out[pos + 6], out[pos + 7]]) as usize;
        if &out[pos..pos + 4] == b"data" {
            let start = pos + 8;
            let end = (start + size).min(out.len());
            let mut i = start;
            while i + 1 < end {
                let sample = i16::from_le_bytes([out[i], out[i + 1]]) as f32 * gain;
                let value = sample.clamp(-32768.0, 32767.0) as i16;
                out[i..i + 2].copy_from_slice(&value.to_le_bytes());
                i += 2;
            }
            break;
        }
        pos += 8 + size + (size & 1);
    }
    out
}

/// 请求显示一条文字通知（可在任意线程调用）。`since` 是剪贴板事件时刻。
pub fn request_text(text: String, since: Instant) {
    if text.trim().is_empty() {
        return;
    }
    let key = text.clone();
    enqueue(Payload::Text { text, since }, key);
}

/// 请求显示一张图片：这里先按当前 DPI 解码并缩到最终尺寸（只做一次），
/// 再交给动画线程；解码失败则回退成「已复制图片」文字卡片。
/// 关闭缩略图时不解码，直接显示文字卡片。
pub fn request_image(raw: RawImage, since: Instant) {
    if !crate::settings::thumbnail() {
        request_text("已复制图片".to_string(), since);
        return;
    }
    let started = Instant::now();
    match unsafe { build_thumb(&raw) } {
        Some(thumb) => {
            crate::settings::log_line(&format!(
                "图片缩略图 {}x{}，解码缩放 {:.1}ms",
                thumb.w,
                thumb.h,
                started.elapsed().as_secs_f32() * 1000.0
            ));
            let key = thumb_key(&thumb);
            enqueue(Payload::Image { thumb, since }, key);
        }
        None => {
            crate::settings::log_line("图片解码失败，回退为文字提示");
            request_text("已复制图片".to_string(), since);
        }
    }
}

/// 请求显示一个本地图片文件的缩略图（资源管理器里直接复制单个图片文件时使用）。
/// 只读取用户复制的那一个路径；格式按扩展名（调用方判断）+ 文件头双重确认。
/// 文件不存在/无权限、不是图片、超出大小或像素上限、解码失败时回退「已复制文件」。
pub fn request_image_file(path: &std::path::Path, since: Instant) {
    if !crate::settings::thumbnail() {
        request_text("已复制文件".to_string(), since);
        return;
    }
    let started = Instant::now();
    match unsafe { build_file_thumb(path) } {
        Some(thumb) => {
            crate::settings::log_line(&format!(
                "文件图片缩略图 {}x{}，解码缩放 {:.1}ms",
                thumb.w,
                thumb.h,
                started.elapsed().as_secs_f32() * 1000.0
            ));
            let key = thumb_key(&thumb);
            enqueue(Payload::Image { thumb, since }, key);
        }
        None => {
            crate::settings::log_line("文件图片无法显示缩略图，回退为文件提示");
            request_text("已复制文件".to_string(), since);
        }
    }
}

/// 文件 → 缩略图。先只读文件头拿尺寸并检查上限，通过后才整图解码一次，
/// 再复用 scale_png 做面积平均缩放；GIF 只取首帧，不做动画。
unsafe fn build_file_thumb(path: &std::path::Path) -> Option<ImageThumb> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > IMAGE_FILE_MAX_BYTES {
        return None;
    }
    let reader = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    // 文件头识别出的格式必须是我们启用的图片格式（与扩展名一起构成双重判断）
    if reader
        .format()
        .is_none_or(|f| !is_decodable_image_format(f))
    {
        return None;
    }
    let mut decoder = reader.into_decoder().ok()?;
    // 竖拍的照片把真实方向写在 EXIF 里，缩略图要按它旋转，
    // 才和图片查看器里看到的一致。不支持方向信息的格式会返回 NoTransforms。
    let orientation =
        image::ImageDecoder::orientation(&mut decoder).unwrap_or(image::metadata::Orientation::NoTransforms);
    let (mut w, mut h) = image::ImageDecoder::dimensions(&decoder);
    if w == 0 || h == 0 || (w as u64) * (h as u64) > IMAGE_FILE_MAX_PIXELS {
        return None;
    }
    // 旋转 90°/270° 会交换宽高，目标尺寸要按旋转后的比例算，避免拉伸
    if matches!(
        orientation,
        image::metadata::Orientation::Rotate90
            | image::metadata::Orientation::Rotate270
            | image::metadata::Orientation::Rotate90FlipH
            | image::metadata::Orientation::Rotate270FlipH
    ) {
        std::mem::swap(&mut w, &mut h);
    }
    let scale = dpi_scale();
    let (tw, th) = target_size(w, h, THUMB_MAX_W * scale, THUMB_MAX_H * scale);
    let mut img = image::DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
    dynamic_thumb(&img, tw as usize, th as usize)
}

fn is_decodable_image_format(format: image::ImageFormat) -> bool {
    use image::ImageFormat::{Bmp, Gif, Ico, Jpeg, Png, Tiff, WebP};
    // 与 main.rs 的 single_image_file_path 扩展名白名单需同步维护。
    matches!(format, Png | Jpeg | Gif | WebP | Bmp | Tiff | Ico)
}

/// 已解码的图片 → 预乘 BGRA 缩略图。优先走 8 位原缓冲（不额外复制整图），
/// 16 位等少见格式才转一次 RGBA8；解码缓冲在本函数返回后随即释放。
fn dynamic_thumb(img: &image::DynamicImage, tw: usize, th: usize) -> Option<ImageThumb> {
    use image::DynamicImage;
    match img {
        DynamicImage::ImageRgb8(buf) => {
            let w = buf.width() as usize;
            scale_png(
                buf.as_raw(),
                png::ColorType::Rgb,
                w,
                buf.height() as usize,
                w * 3,
                tw,
                th,
            )
        }
        DynamicImage::ImageRgba8(buf) => {
            let w = buf.width() as usize;
            scale_png(
                buf.as_raw(),
                png::ColorType::Rgba,
                w,
                buf.height() as usize,
                w * 4,
                tw,
                th,
            )
        }
        DynamicImage::ImageLuma8(buf) => {
            let w = buf.width() as usize;
            scale_png(
                buf.as_raw(),
                png::ColorType::Grayscale,
                w,
                buf.height() as usize,
                w,
                tw,
                th,
            )
        }
        DynamicImage::ImageLumaA8(buf) => {
            let w = buf.width() as usize;
            scale_png(
                buf.as_raw(),
                png::ColorType::GrayscaleAlpha,
                w,
                buf.height() as usize,
                w * 2,
                tw,
                th,
            )
        }
        other => {
            let rgba = other.to_rgba8();
            let w = rgba.width() as usize;
            scale_png(
                rgba.as_raw(),
                png::ColorType::Rgba,
                w,
                rgba.height() as usize,
                w * 4,
                tw,
                th,
            )
        }
    }
}

fn thumb_hash(px: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in px {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 图片去重键：尺寸 + 像素哈希，避免同一张图连续复制时重复弹卡片。
fn thumb_key(thumb: &ImageThumb) -> String {
    format!(
        "image:{}x{}:{:016x}",
        thumb.w,
        thumb.h,
        thumb_hash(&thumb.px)
    )
}

/// 入队并触发一次展示；相同内容在去重窗口内只提示一次。
fn enqueue(payload: Payload, key: String) {
    {
        let recently = LAST_SHOWN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|(at, last)| at.elapsed().as_millis() < DEDUP_MS && *last == key);
        if recently {
            return;
        }
        *LAST_SHOWN.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), key));
    }
    {
        let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        queue.push_back(payload);
        while queue.len() > 4 {
            queue.pop_front();
        }
    }
    play_copy_sound();
    let hwnd = TOAST_HWND.load(Ordering::SeqCst) as HWND;
    if !hwnd.is_null() {
        unsafe { PostMessageW(hwnd, WM_APP_TOAST, 0, 0) };
    }
}

unsafe fn screen_dc() -> HDC {
    let cached = SCREEN_DC.load(Ordering::SeqCst);
    if cached != 0 {
        return cached as HDC;
    }
    let mut dc = GetDC(std::ptr::null_mut());
    if dc.is_null() {
        // 个别会话里桌面 DC 拿不到，退回到通知窗口自己的 DC，保证卡片仍能绘制
        let raw = TOAST_HWND.load(Ordering::SeqCst);
        if raw != 0 {
            dc = GetDC(raw as HWND);
        }
    }
    if !dc.is_null() {
        SCREEN_DC.store(dc as isize, Ordering::SeqCst);
    } else {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::SeqCst) {
            crate::settings::log_line("通知：屏幕 DC 获取失败，卡片无法绘制");
        }
    }
    dc
}

unsafe fn dpi_scale() -> f32 {
    // 优先用窗口所在显示器的 DPI，避免不同线程拿到的 DC 报不同值
    let raw = TOAST_HWND.load(Ordering::SeqCst);
    if raw != 0 {
        let dpi = GetDpiForWindow(raw as HWND);
        if dpi > 0 {
            return dpi as f32 / 96.0;
        }
    }
    let screen = screen_dc();
    if screen.is_null() {
        return 1.0;
    }
    let dpi = GetDeviceCaps(screen, LOGPIXELSX as i32);
    if dpi <= 0 { 1.0 } else { dpi as f32 / 96.0 }
}

unsafe fn system_dark() -> bool {
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let mut kind: u32 = 0;
    let ok = RegGetValueW(
        HKEY_CURRENT_USER,
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
        w!("AppsUseLightTheme"),
        RRF_RT_REG_DWORD,
        &mut kind,
        &mut value as *mut u32 as *mut _,
        &mut size,
    );
    ok == 0 && value == 0
}

// ---- 图片缩略图：剪贴板原始数据 → 最终显示尺寸的预乘 BGRA ----

/// 按当前 DPI 计算最大显示框，并在通知生成时一次性解码、缩放。
unsafe fn build_thumb(raw: &RawImage) -> Option<ImageThumb> {
    if raw.bytes.len() as u64 > IMAGE_FILE_MAX_BYTES {
        return None;
    }
    let scale = dpi_scale();
    if raw.png {
        png_thumb(&raw.bytes, THUMB_MAX_W * scale, THUMB_MAX_H * scale)
    } else {
        dib_thumb(&raw.bytes, THUMB_MAX_W * scale, THUMB_MAX_H * scale)
    }
}

/// 等比缩放到最大框内：小图保持原始像素尺寸（不放大），大图按宽高限制中更紧的一边缩。
fn target_size(w: u32, h: u32, max_w: f32, max_h: f32) -> (i32, i32) {
    let (w, h) = (w.max(1) as f32, h.max(1) as f32);
    if w <= max_w && h <= max_h {
        return (w as i32, h as i32);
    }
    let k = (max_w / w).min(max_h / h);
    let tw = (w * k).floor().max(1.0).min(max_w.floor().max(1.0)) as i32;
    let th = (h * k).floor().max(1.0).min(max_h.floor().max(1.0)) as i32;
    (tw, th)
}

struct DibHeader {
    w: u32,
    h: u32,
    bpp: u16,
    compression: u32,
    top_down: bool,
    offset: usize,
    stride: usize,
    r_mask: u32,
    g_mask: u32,
    b_mask: u32,
    a_mask: u32,
}

fn u32_at(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

/// 解析 CF_DIB / CF_DIBV5 头（不做像素解码），支持 16/24/32bpp。
fn parse_dib(bytes: &[u8]) -> Option<DibHeader> {
    if bytes.len() < 40 {
        return None;
    }
    let header = u32_at(bytes, 0) as usize;
    if header < 40 || header > bytes.len() {
        return None;
    }
    let w = u32_at(bytes, 4) as i32;
    let h = u32_at(bytes, 8) as i32;
    if w <= 0 || h == 0 {
        return None;
    }
    let bpp = u16::from_le_bytes([bytes[14], bytes[15]]);
    if !matches!(bpp, 16 | 24 | 32) {
        return None;
    }
    let compression = u32_at(bytes, 16);
    let top_down = h < 0;
    let h = h.unsigned_abs();

    let (mut r_mask, mut g_mask, mut b_mask, mut a_mask) = (0, 0, 0, 0);
    if compression == 3 || compression == 6 {
        // BI_BITFIELDS：V4/V5 头里的掩码在 40/44/48，40 字节旧头则紧跟其后。
        let m = if header >= 52 { 40 } else { header };
        if m + 12 > bytes.len() {
            return None;
        }
        r_mask = u32_at(bytes, m);
        g_mask = u32_at(bytes, m + 4);
        b_mask = u32_at(bytes, m + 8);
        if header >= 56 && 52 + 4 <= bytes.len() {
            a_mask = u32_at(bytes, 52);
        }
    } else {
        if bpp == 16 {
            (r_mask, g_mask, b_mask) = (0x7C00, 0x03E0, 0x001F);
        } else {
            (r_mask, g_mask, b_mask) = (0x00FF_0000, 0x0000_FF00, 0x0000_00FF);
        }
        if header >= 56 && 52 + 4 <= bytes.len() {
            a_mask = u32_at(bytes, 52);
        }
    }

    let mask_extra = if (compression == 3 || compression == 6) && header < 52 {
        12
    } else {
        0
    };
    let mut offset = header + mask_extra;
    let stride = ((w as usize * bpp as usize + 31) / 32) * 4;
    let pixels = stride.checked_mul(h as usize)?;
    // 有些写入器（如 .NET/GDI+ 的剪贴板 DIB）在 V4/V5 头之后又重复写了一遍颜色掩码，
    // 像素数据实际从 header + 12 开始。只在数据长度足以容纳这份重复掩码时才判定。
    if (compression == 3 || compression == 6) && header >= 52 && offset + 12 + pixels <= bytes.len()
    {
        let dup = u32_at(bytes, offset) == r_mask
            && u32_at(bytes, offset + 4) == g_mask
            && u32_at(bytes, offset + 8) == b_mask;
        if dup {
            offset += 12;
            if a_mask != 0 && offset + 4 + pixels <= bytes.len() && u32_at(bytes, offset) == a_mask {
                offset += 4;
            }
        }
    }
    if offset + pixels > bytes.len() {
        return None;
    }
    Some(DibHeader {
        w: w as u32,
        h,
        bpp,
        compression,
        top_down,
        offset,
        stride,
        r_mask,
        g_mask,
        b_mask,
        a_mask,
    })
}

#[inline]
fn mask_val(v: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = (mask >> shift) as u64;
    let raw = ((v & mask) >> shift) as u64;
    ((raw * 255 + max / 2) / max) as u8
}

/// 32bpp BI_RGB 的 alpha 字节常被写成 0（表示无 alpha）；只有出现非 0 才当作真 alpha。
fn dib_has_alpha(d: &DibHeader, bytes: &[u8]) -> bool {
    if d.a_mask != 0 {
        return true;
    }
    if d.bpp != 32 || d.compression != 0 {
        return false;
    }
    let total = (d.w as usize) * (d.h as usize);
    let step = (total / ALPHA_SAMPLE_BUDGET).max(1);
    let mut i = 0usize;
    while i < total {
        let x = i % d.w as usize;
        let y = i / d.w as usize;
        let row = if d.top_down {
            y
        } else {
            d.h as usize - 1 - y
        };
        if bytes[d.offset + row * d.stride + x * 4 + 3] != 0 {
            return true;
        }
        i += step;
    }
    false
}

/// DIB → 缩略图：直接按目标像素做面积平均采样，不生成全尺寸中间缓冲，
/// 4K 截图也不会出现几十 MB 的临时位图。
fn dib_thumb(bytes: &[u8], max_w: f32, max_h: f32) -> Option<ImageThumb> {
    let d = parse_dib(bytes)?;
    // 超大图直接拒绝，避免采样循环和“像素数”不受控（与文件图片共用同一上限）
    if (d.w as u64) * (d.h as u64) > IMAGE_FILE_MAX_PIXELS {
        return None;
    }
    let (tw, th) = target_size(d.w, d.h, max_w, max_h);
    let use_alpha = dib_has_alpha(&d, bytes);
    let bpp = d.bpp as usize;
    let (sw, sh) = (d.w as usize, d.h as usize);

    // 目标像素对应的源矩形，整数边界保证不重不漏。
    let mut xs = Vec::with_capacity(tw as usize);
    for dx in 0..tw as u32 {
        let x0 = (dx as u64 * d.w as u64 / tw as u64) as usize;
        let x1 = (((dx + 1) as u64 * d.w as u64 + tw as u64 - 1) / tw as u64) as usize;
        xs.push((x0, x1.min(sw).max(x0 + 1)));
    }
    let mut ys = Vec::with_capacity(th as usize);
    for dy in 0..th as u32 {
        let y0 = (dy as u64 * d.h as u64 / th as u64) as usize;
        let y1 = (((dy + 1) as u64 * d.h as u64 + th as u64 - 1) / th as u64) as usize;
        ys.push((y0, y1.min(sh).max(y0 + 1)));
    }

    let mut out = vec![0u8; (tw * th * 4) as usize];
    for (dy, &(y0, y1)) in ys.iter().enumerate() {
        for (dx, &(x0, x1)) in xs.iter().enumerate() {
            let (mut ar, mut ag, mut ab, mut aa) = (0u64, 0u64, 0u64, 0u64);
            let mut n = 0u64;
            for y in y0..y1 {
                let row = if d.top_down { y } else { sh - 1 - y };
                let base = d.offset + row * d.stride;
                for x in x0..x1 {
                    let v = match bpp {
                        32 => {
                            let o = base + x * 4;
                            u32::from_le_bytes([
                                bytes[o],
                                bytes[o + 1],
                                bytes[o + 2],
                                bytes[o + 3],
                            ])
                        }
                        24 => {
                            let o = base + x * 3;
                            (bytes[o] as u32)
                                | ((bytes[o + 1] as u32) << 8)
                                | ((bytes[o + 2] as u32) << 16)
                        }
                        _ => {
                            let o = base + x * 2;
                            (bytes[o] as u32) | ((bytes[o + 1] as u32) << 8)
                        }
                    };
                    let r = mask_val(v, d.r_mask) as u64;
                    let g = mask_val(v, d.g_mask) as u64;
                    let b = mask_val(v, d.b_mask) as u64;
                    let a = if d.a_mask != 0 {
                        mask_val(v, d.a_mask) as u64
                    } else if use_alpha {
                        bytes[base + x * 4 + 3] as u64
                    } else {
                        255
                    };
                    ar += r * a;
                    ag += g * a;
                    ab += b * a;
                    aa += a;
                    n += 1;
                }
            }
            let n = n.max(1);
            let o = (dy * tw as usize + dx) * 4;
            // 预乘 BGRA：B, G, R, A
            out[o] = (ab / n / 255) as u8;
            out[o + 1] = (ag / n / 255) as u8;
            out[o + 2] = (ar / n / 255) as u8;
            out[o + 3] = (aa / n) as u8;
        }
    }
    Some(ImageThumb { w: tw, h: th, px: out })
}

/// PNG → 缩略图：纯 Rust 解码（自动展开调色板/tRNS、16 位压到 8 位），
/// 再按面积平均直接缩到目标尺寸，输出预乘 BGRA。解码失败返回 None，由调用方回退文字卡片。
fn png_thumb(bytes: &[u8], max_w: f32, max_h: f32) -> Option<ImageThumb> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = reader.info().size();
    if w == 0 || h == 0 {
        return None;
    }
    // 先看尺寸再分配解码缓冲：8000 万像素上限之外的 PNG 直接回退文字卡片
    if (w as u64) * (h as u64) > IMAGE_FILE_MAX_PIXELS {
        return None;
    }
    let (tw, th) = target_size(w, h, max_w, max_h);
    let mut frame = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut frame).ok()?;
    let (sw, sh) = (info.width as usize, info.height as usize);
    let stride = info.line_size;
    if sw == 0 || sh == 0 || stride == 0 || info.buffer_size() > frame.len() {
        return None;
    }
    let scaled = scale_png(
        &frame[..info.buffer_size()],
        info.color_type,
        sw,
        sh,
        stride,
        tw as usize,
        th as usize,
    );
    // 解码缓冲（4K 约 33MB）在这里立即释放，通知存活期间只保留小缩略图。
    drop(frame);
    scaled
}

#[inline]
fn png_pixel(
    src: &[u8],
    ct: png::ColorType,
    stride: usize,
    x: usize,
    y: usize,
) -> (u64, u64, u64, u64) {
    let i = y * stride;
    match ct {
        png::ColorType::Rgba => {
            let o = i + x * 4;
            (
                src[o] as u64,
                src[o + 1] as u64,
                src[o + 2] as u64,
                src[o + 3] as u64,
            )
        }
        png::ColorType::Rgb => {
            let o = i + x * 3;
            (src[o] as u64, src[o + 1] as u64, src[o + 2] as u64, 255)
        }
        png::ColorType::GrayscaleAlpha => {
            let o = i + x * 2;
            let v = src[o] as u64;
            (v, v, v, src[o + 1] as u64)
        }
        _ => {
            let v = src[i + x] as u64;
            (v, v, v, 255)
        }
    }
}

/// 面积平均缩放：整数边界保证不重不漏，输出预乘 BGRA（B,G,R,A）。
fn scale_png(
    src: &[u8],
    ct: png::ColorType,
    sw: usize,
    sh: usize,
    stride: usize,
    tw: usize,
    th: usize,
) -> Option<ImageThumb> {
    if !matches!(
        ct,
        png::ColorType::Rgba
            | png::ColorType::Rgb
            | png::ColorType::GrayscaleAlpha
            | png::ColorType::Grayscale
    ) {
        return None;
    }
    let mut out = vec![0u8; tw * th * 4];
    for dy in 0..th {
        let y0 = dy * sh / th;
        let y1 = (((dy + 1) * sh + th - 1) / th).min(sh).max(y0 + 1);
        for dx in 0..tw {
            let x0 = dx * sw / tw;
            let x1 = (((dx + 1) * sw + tw - 1) / tw).min(sw).max(x0 + 1);
            let (mut ar, mut ag, mut ab, mut aa, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for y in y0..y1 {
                for x in x0..x1 {
                    let (r, g, b, a) = png_pixel(src, ct, stride, x, y);
                    ar += r * a;
                    ag += g * a;
                    ab += b * a;
                    aa += a;
                    n += 1;
                }
            }
            let n = n.max(1);
            let o = (dy * tw + dx) * 4;
            out[o] = (ab / n / 255) as u8;
            out[o + 1] = (ag / n / 255) as u8;
            out[o + 2] = (ar / n / 255) as u8;
            out[o + 3] = (aa / n) as u8;
        }
    }
    Some(ImageThumb {
        w: tw as i32,
        h: th as i32,
        px: out,
    })
}

fn round_rect_sdf(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (px - cx).abs() - hw;
    let qy = (py - cy).abs() - hh;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

unsafe fn new_surface(width: i32, height: i32) -> Option<Surface> {
    let screen = screen_dc();
    if screen.is_null() {
        return None;
    }
    let dc = CreateCompatibleDC(screen);
    if dc.is_null() {
        crate::settings::log_line("通知：CreateCompatibleDC 失败，跳过本次显示");
        return None;
    }
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // 自上而下
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: (width * height * 4) as u32,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    if bitmap.is_null() || bits.is_null() {
        crate::settings::log_line(&format!(
            "通知：CreateDIBSection 失败（{}x{}），跳过本次显示",
            width, height
        ));
        DeleteDC(dc);
        return None;
    }
    let old = SelectObject(dc, bitmap);
    Some(Surface {
        dc,
        bitmap,
        old,
        bits: bits as *mut u8,
        width,
        height,
    })
}

/// 按卡片尺寸准备绘制表面：尺寸不变时复用旧表面，否则新建。
/// 返回表面与包含阴影留白的窗口尺寸。
unsafe fn acquire_surface<'a>(
    slot: &'a mut Option<Surface>,
    card_w_px: i32,
    card_h_px: i32,
    scale: f32,
) -> Option<(&'a Surface, i32, i32)> {
    let shadow = (SHADOW * scale).round() as i32;
    let win_w = card_w_px + shadow * 2;
    let win_h = card_h_px + shadow * 2;
    if slot
        .as_ref()
        .is_none_or(|s| s.width != win_w || s.height != win_h)
    {
        *slot = new_surface(win_w, win_h);
    }
    let surface = slot.as_ref()?;
    Some((surface, win_w, win_h))
}

/// 1×1 的内存位图，只用来给文本测量提供一个 GpGraphics（创建一次，全局复用）。
unsafe fn measure_graphics() -> *mut GpGraphics {
    let cached = MEASURE_GFX.load(Ordering::SeqCst);
    if cached != 0 {
        return cached as *mut GpGraphics;
    }
    let mut bitmap: *mut GpBitmap = std::ptr::null_mut();
    if GdipCreateBitmapFromScan0(1, 1, 0, PIXEL_FORMAT_PARGB, std::ptr::null(), &mut bitmap) != 0
        || bitmap.is_null()
    {
        return std::ptr::null_mut();
    }
    let mut graphics: *mut GpGraphics = std::ptr::null_mut();
    if GdipGetImageGraphicsContext(bitmap as *mut GpImage, &mut graphics) != 0 || graphics.is_null()
    {
        GdipDisposeImage(bitmap as *mut GpImage);
        return std::ptr::null_mut();
    }
    MEASURE_BMP.store(bitmap as isize, Ordering::SeqCst);
    MEASURE_GFX.store(graphics as isize, Ordering::SeqCst);
    graphics
}

unsafe fn make_text_style(scale: f32) -> Option<TextStyle> {
    let mut family: *mut GpFontFamily = std::ptr::null_mut();
    if GdipCreateFontFamilyFromName(w!("Microsoft YaHei UI"), std::ptr::null_mut(), &mut family) != 0
        || family.is_null()
    {
        GdipCreateFontFamilyFromName(w!("Segoe UI"), std::ptr::null_mut(), &mut family);
    }
    if family.is_null() {
        return None;
    }
    let mut font: *mut GpFont = std::ptr::null_mut();
    if GdipCreateFont(family, FONT_SIZE * scale, 0, UnitPixel, &mut font) != 0 || font.is_null() {
        GdipDeleteFontFamily(family);
        return None;
    }
    let mut single: *mut GpStringFormat = std::ptr::null_mut();
    let mut multi: *mut GpStringFormat = std::ptr::null_mut();
    let mut ok = false;
    if GdipCreateStringFormat(0, 0, &mut single) == 0
        && !single.is_null()
        && GdipCreateStringFormat(0, 0, &mut multi) == 0
        && !multi.is_null()
    {
        GdipSetStringFormatTrimming(single, StringTrimmingEllipsisCharacter);
        GdipSetStringFormatFlags(single, STRING_FORMAT_NO_WRAP);
        GdipSetStringFormatTrimming(multi, StringTrimmingEllipsisCharacter);
        ok = true;
    }
    let style = TextStyle {
        family,
        font,
        single,
        multi,
    };
    if ok { Some(style) } else { None }
}

/// 按 DPI 缩放缓存一份字体/排版格式；scale 不变时跨通知复用，绘制参数完全一致。
unsafe fn cached_text_style<'a>(
    cache: &'a mut Option<(f32, TextStyle)>,
    scale: f32,
) -> Option<&'a TextStyle> {
    let reusable = cache
        .as_ref()
        .is_some_and(|(cached_scale, _)| (*cached_scale - scale).abs() < 0.001);
    if !reusable {
        *cache = make_text_style(scale).map(|style| (scale, style));
    }
    cache.as_ref().map(|(_, style)| style)
}

/// 测量单行文字的自然宽度（设备像素）。
unsafe fn measure_width(text: &str, style: &TextStyle) -> f32 {
    let graphics = measure_graphics();
    if graphics.is_null() {
        return 0.0;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let layout = RectF {
        X: 0.0,
        Y: 0.0,
        Width: 1_000_000.0,
        Height: 1000.0,
    };
    let mut bounds: RectF = std::mem::zeroed();
    let mut fitted: i32 = 0;
    let mut lines: i32 = 0;
    if GdipMeasureString(
        graphics,
        wide.as_ptr(),
        wide.len() as i32,
        style.font,
        &layout,
        style.single,
        &mut bounds,
        &mut fitted,
        &mut lines,
    ) != 0
    {
        return 0.0;
    }
    bounds.Width
}

/// 文字实际墨迹包围盒（相对绘制矩形原点），用于按墨迹做视觉居中。
struct InkBox {
    left: i32,
    top: i32,
    w: i32,
    h: i32,
}

/// 离屏画一遍文字并扫描 alpha，得到真实墨迹边界。
/// GDI+ 的 MeasureString 返回的是含字侧留白/行距的布局宽度，不能直接当作墨迹宽度。
unsafe fn measure_ink(
    text: &str,
    style: &TextStyle,
    w: i32,
    h: i32,
    two_lines: bool,
) -> Option<InkBox> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let stride = w * 4;
    let mut buf = vec![0u8; (stride * h) as usize];
    let mut bitmap: *mut GpBitmap = std::ptr::null_mut();
    if GdipCreateBitmapFromScan0(
        w,
        h,
        stride,
        PIXEL_FORMAT_PARGB,
        buf.as_mut_ptr() as *const u8,
        &mut bitmap,
    ) != 0
        || bitmap.is_null()
    {
        return None;
    }
    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if GdipGetImageGraphicsContext(bitmap as *mut GpImage, &mut g) != 0 || g.is_null() {
        GdipDisposeImage(bitmap as *mut GpImage);
        return None;
    }
    GdipSetTextRenderingHint(g, TextRenderingHintAntiAliasGridFit);
    let mut brush: *mut GpSolidFill = std::ptr::null_mut();
    GdipCreateSolidFill(0xFF00_0000, &mut brush);
    if !brush.is_null() {
        let rect = RectF {
            X: 0.0,
            Y: 0.0,
            Width: w as f32,
            Height: h as f32,
        };
        let format = if two_lines { style.multi } else { style.single };
        let wide: Vec<u16> = text.encode_utf16().collect();
        GdipDrawString(
            g,
            wide.as_ptr(),
            wide.len() as i32,
            style.font,
            &rect,
            format,
            brush as *const GpBrush,
        );
        GdipDeleteBrush(brush as *mut GpBrush);
    }
    GdipDeleteGraphics(g);

    // 阈值 128 = 50% 覆盖率，和像素验收口径一致
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = -1i32;
    let mut max_y = -1i32;
    for y in 0..h {
        let row = (y * stride) as usize;
        for x in 0..w {
            if buf[row + (x * 4) as usize + 3] >= INK_ALPHA_THRESHOLD {
                if x < min_x {
                    min_x = x;
                }
                if x > max_x {
                    max_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if y > max_y {
                    max_y = y;
                }
            }
        }
    }
    GdipDisposeImage(bitmap as *mut GpImage);
    if max_x < 0 || max_y < 0 {
        return None;
    }
    Some(InkBox {
        left: min_x,
        top: min_y,
        w: max_x - min_x + 1,
        h: max_y - min_y + 1,
    })
}

/// 先手工画阴影/圆角卡片，再用 GDI+ 叠加文字（无图标）。
#[allow(clippy::too_many_arguments)]
unsafe fn paint(
    surface: &Surface,
    text: Option<&TextLayer>,
    image: Option<&ImageLayer>,
    card_w: f32,
    card_h: f32,
    scale: f32,
    dark: bool,
) {
    let pal = palette(dark);
    let stride = surface.width * 4;

    // 阴影偏移按像素取整，且和窗口尺寸（卡片 + 2*off）用同一个值，避免 DIB 与卡片尺寸错开一像素。
    let off = (SHADOW * scale).round();
    let sx = off;
    let sy = off;
    let sw = card_w;
    let sh = card_h;
    let r = RADIUS * scale;
    let cx = sx + sw * 0.5;
    let cy = sy + sh * 0.5;
    let hw = sw * 0.5 - r;
    let hh = sh * 0.5 - r;
    // 贴边淡影：扩散只剩几像素，σ 小、最大 alpha 低，卡片四周不再有扩散光晕或大范围灰边。
    let sigma = 2.0 * scale;
    let drop = 1.0 * scale;

    // 表面复用时先把整张位图清成完全透明，杜绝上一张卡片边缘（尤其是底边）的残影。
    std::ptr::write_bytes(
        surface.bits,
        0,
        (stride as usize) * (surface.height as usize),
    );

    // 4x4 超采样：每个像素取 16 个子样本求覆盖率，圆角边缘按距离场做 alpha 混合，无台阶、无锯齿。
    const SS: u32 = 4;
    let inv_ss = 1.0 / (SS * SS) as f32;
    for y in 0..surface.height {
        for x in 0..surface.width {
            let mut acc_a = 0.0f32;
            let mut acc_r = 0.0f32;
            let mut acc_g = 0.0f32;
            let mut acc_b = 0.0f32;

            for j in 0..SS {
                let py = y as f32 + (j as f32 + 0.5) / SS as f32;
                for i in 0..SS {
                    let px = x as f32 + (i as f32 + 0.5) / SS as f32;

                    // 阴影：连续高斯衰减，贴着卡片边缘一圈很浅的淡影。
                    let ds = round_rect_sdf(px, py, cx, cy + drop, hw, hh, r).max(0.0);
                    let shadow_a =
                        pal.shadow_alpha * (-(ds * ds) / (2.0 * sigma * sigma)).exp();

                    // 无描边：只画卡片本体；边缘完全靠距离场覆盖率 + 4x4 超采样做 alpha 过渡，
                    // 不再额外画 1.5px 的外圈线条。
                    let d = round_rect_sdf(px, py, cx, cy, hw, hh, r);
                    let cov = (0.5 - d).clamp(0.0, 1.0);

                    // 单样本合成：黑阴影 → 背景，全程用预乘 RGBA 累加。
                    let mut a = shadow_a;
                    let mut cr = 0.0f32;
                    let mut cg = 0.0f32;
                    let mut cb = 0.0f32;
                    if cov > 0.0 {
                        cr = pal.bg[0] * cov + cr * (1.0 - cov);
                        cg = pal.bg[1] * cov + cg * (1.0 - cov);
                        cb = pal.bg[2] * cov + cb * (1.0 - cov);
                        a = cov + a * (1.0 - cov);
                    }

                    // 缩略图叠在卡片背景上：图片像素已是预乘 BGRA，圆角用同一套 4x4 超采样裁切。
                    if let Some(img) = image {
                        let ix = px - img.x;
                        let iy = py - img.y;
                        if ix >= 0.0 && iy >= 0.0 && ix < img.w as f32 && iy < img.h as f32 {
                            let inside = ix >= img.radius
                                && iy >= img.radius
                                && ix <= img.w as f32 - img.radius
                                && iy <= img.h as f32 - img.radius;
                            let icov = if inside {
                                1.0
                            } else {
                                let icx = img.x + img.w as f32 * 0.5;
                                let icy = img.y + img.h as f32 * 0.5;
                                let ihw = img.w as f32 * 0.5 - img.radius;
                                let ihh = img.h as f32 * 0.5 - img.radius;
                                let di = round_rect_sdf(px, py, icx, icy, ihw, ihh, img.radius);
                                (0.5 - di).clamp(0.0, 1.0)
                            };
                            if icov > 0.0 {
                                let si = (iy as usize * img.w as usize + ix as usize) * 4;
                                let b = img.px[si] as f32 / 255.0;
                                let g = img.px[si + 1] as f32 / 255.0;
                                let r = img.px[si + 2] as f32 / 255.0;
                                let ia = img.px[si + 3] as f32 / 255.0;
                                let ea = ia * icov;
                                cr = r * icov + cr * (1.0 - ea);
                                cg = g * icov + cg * (1.0 - ea);
                                cb = b * icov + cb * (1.0 - ea);
                                a = ea + a * (1.0 - ea);
                            }
                        }
                    }

                    acc_a += a;
                    acc_r += cr;
                    acc_g += cg;
                    acc_b += cb;
                }
            }

            let a = acc_a * inv_ss;
            let idx = (y as usize) * (stride as usize) + (x as usize) * 4;
            // cr/cg/cb 已是预乘色，直接乘 255 写出，不再二次乘 alpha（否则边缘会重复变暗）。
            surface
                .bits
                .add(idx)
                .write((acc_b * inv_ss * 255.0).round().clamp(0.0, 255.0) as u8);
            surface
                .bits
                .add(idx + 1)
                .write((acc_g * inv_ss * 255.0).round().clamp(0.0, 255.0) as u8);
            surface
                .bits
                .add(idx + 2)
                .write((acc_r * inv_ss * 255.0).round().clamp(0.0, 255.0) as u8);
            surface
                .bits
                .add(idx + 3)
                .write((a * 255.0).round().clamp(0.0, 255.0) as u8);
        }
    }

    let mut bitmap: *mut GpBitmap = std::ptr::null_mut();
    if GdipCreateBitmapFromScan0(
        surface.width,
        surface.height,
        stride,
        PIXEL_FORMAT_PARGB,
        surface.bits as *const u8,
        &mut bitmap,
    ) != 0
        || bitmap.is_null()
    {
        return;
    }

    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if GdipGetImageGraphicsContext(bitmap as *mut GpImage, &mut g) != 0 || g.is_null() {
        GdipDisposeImage(bitmap as *mut GpImage);
        return;
    }
    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    // 透明表面上用灰度抗锯齿（ClearType 会在透明像素上产生彩边）。
    GdipSetTextRenderingHint(g, TextRenderingHintAntiAliasGridFit);

    if let Some(t) = text {
        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        let argb: u32 = 0xFF00_0000
            | ((pal.body[0] * 255.0) as u32) << 16
            | ((pal.body[1] * 255.0) as u32) << 8
            | (pal.body[2] * 255.0) as u32;
        GdipCreateSolidFill(argb, &mut brush);
        if !brush.is_null() {
            let rect = RectF {
                X: sx + t.x,
                Y: sy + t.y,
                Width: t.w,
                Height: t.h,
            };
            let format = if t.two_lines {
                t.style.multi
            } else {
                t.style.single
            };
            let wide: Vec<u16> = t.text.encode_utf16().collect();
            GdipDrawString(
                g,
                wide.as_ptr(),
                wide.len() as i32,
                t.style.font,
                &rect,
                format,
                brush as *const GpBrush,
            );
            GdipDeleteBrush(brush as *mut GpBrush);
        }
    }

    GdipDeleteGraphics(g);
    GdipDisposeImage(bitmap as *mut GpImage);
}

/// 主显示器工作区右下角（扣除任务栏），按 DPI 缩放后的窗口尺寸计算。
unsafe fn position_for(width: i32, height: i32, scale: f32) -> (i32, i32) {
    let point = POINT { x: 0, y: 0 };
    let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTOPRIMARY);
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info) == 0 {
        return (0, 0);
    }
    let margin = (SCREEN_MARGIN * scale) as i32;
    (
        info.rcWork.right - width - margin,
        info.rcWork.bottom - height - margin,
    )
}

/// 只更新分层窗口的位置与整体 alpha，不重绘位图内容。
unsafe fn present(hwnd: HWND, surface: &Surface, x: i32, y: i32, alpha: u8) {
    let screen = screen_dc();
    if screen.is_null() {
        return;
    }
    let dst = POINT { x, y };
    let size = SIZE {
        cx: surface.width,
        cy: surface.height,
    };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER,
        BlendFlags: 0,
        SourceConstantAlpha: alpha,
        AlphaFormat: AC_SRC_ALPHA,
    };
    UpdateLayeredWindow(
        hwnd,
        screen,
        &dst,
        &size,
        surface.dc,
        &src,
        0,
        &blend,
        ULW_ALPHA,
    );
}

struct CardPlan {
    x: i32,
    y: i32,
}

/// 排版 + 绘制一条通知。表面尺寸没变就复用旧表面，只重画内容。
unsafe fn prepare(
    payload: &Payload,
    scale: f32,
    dark: bool,
    slot: &mut Option<Surface>,
    style_cache: &mut Option<(f32, TextStyle)>,
) -> Option<CardPlan> {
    match payload {
        Payload::Text { text, .. } => prepare_text(text, scale, dark, slot, style_cache),
        Payload::Image { thumb, .. } => prepare_image(thumb, scale, dark, slot),
    }
}

unsafe fn prepare_text(
    text: &str,
    scale: f32,
    dark: bool,
    slot: &mut Option<Surface>,
    style_cache: &mut Option<(f32, TextStyle)>,
) -> Option<CardPlan> {
    let style = cached_text_style(style_cache, scale)?;
    let natural = measure_width(text, style);

    let pad = PAD_X * scale;
    let max_w = crate::settings::max_width() as f32 * scale;
    let content_max = max_w - pad * 2.0;
    // 短内容按实测宽度收紧，长内容到上限后折两行；固定文案（已复制图片/文件）自然落在最小宽度。
    let (two_lines, card_w) = if natural <= content_max {
        (
            false,
            (natural + pad * 2.0 + WIDTH_SLACK).clamp(CARD_MIN_W * scale, max_w),
        )
    } else {
        (true, max_w)
    };
    let content_h = if two_lines { LINE_H * 2.0 } else { LINE_H } * scale;
    let card_h = content_h + PAD_Y * 2.0 * scale;
    let card_w_px = card_w.round();
    let card_h_px = card_h.round();

    // 以墨迹包围盒居中：GDI+ 的布局宽度含字侧留白与行距，按它居中会让中文短句明显偏左、整体偏上。
    // 先离屏画一遍取真实墨迹，再让墨迹在卡片内水平与垂直都居中。
    let layout_w = (card_w_px - pad * 2.0).round();
    let layout_h = content_h.round();
    let (text_x, text_y, text_w) =
        match measure_ink(text, style, layout_w as i32, layout_h as i32, two_lines) {
            Some(ink) => (
                ((card_w_px - ink.w as f32) * 0.5).round() - ink.left as f32,
                ((card_h_px - ink.h as f32) * 0.5).round() - ink.top as f32,
                layout_w,
            ),
            // 测量失败时退回原来的内边距起点，不影响出字
            None => (pad, PAD_Y * scale, layout_w),
        };

    let (surface, win_w, win_h) =
        acquire_surface(slot, card_w_px as i32, card_h_px as i32, scale)?;
    let layer = TextLayer {
        text,
        style,
        x: text_x,
        y: text_y,
        w: text_w,
        h: layout_h,
        two_lines,
    };
    paint(surface, Some(&layer), None, card_w_px, card_h_px, scale, dark);
    let (x, y) = position_for(win_w, win_h, scale);
    Some(CardPlan { x, y })
}

/// 图片卡片：卡片尺寸 = 缩略图 + 四周内边距；小图受最小宽度约束时居中留白。
unsafe fn prepare_image(
    thumb: &ImageThumb,
    scale: f32,
    dark: bool,
    slot: &mut Option<Surface>,
) -> Option<CardPlan> {
    let pad_x = (PAD_X * scale).round();
    let pad_y = (PAD_Y * scale).round();
    let card_w = ((thumb.w as f32 + pad_x * 2.0).max(CARD_MIN_W * scale))
        .min(crate::settings::max_width() as f32 * scale)
        .round();
    let card_h = (thumb.h as f32 + pad_y * 2.0).round();
    let card_w_px = card_w;
    let card_h_px = card_h;

    let (surface, win_w, win_h) =
        acquire_surface(slot, card_w_px as i32, card_h_px as i32, scale)?;

    let off = (SHADOW * scale).round();
    let img_x = off + ((card_w_px - thumb.w as f32) * 0.5).round();
    let img_y = off + ((card_h_px - thumb.h as f32) * 0.5).round();
    let radius = ((RADIUS - THUMB_RADIUS_INSET) * scale)
        .min(thumb.w as f32 * 0.5)
        .min(thumb.h as f32 * 0.5)
        .max(0.0);
    let layer = ImageLayer {
        px: &thumb.px,
        w: thumb.w,
        h: thumb.h,
        x: img_x,
        y: img_y,
        radius,
    };
    paint(surface, None, Some(&layer), card_w_px, card_h_px, scale, dark);
    let (x, y) = position_for(win_w, win_h, scale);
    Some(CardPlan { x, y })
}

fn ease_out_cubic(p: f32) -> f32 {
    1.0 - (1.0 - p).powi(3)
}

struct FrameStats {
    count: u32,
    total_ms: f32,
    max_ms: f32,
    last: Instant,
}

impl FrameStats {
    fn new() -> Self {
        FrameStats {
            count: 0,
            total_ms: 0.0,
            max_ms: 0.0,
            last: Instant::now(),
        }
    }

    /// 记录一帧与上一帧的真实间隔（首帧含准备耗时，不计入统计）。
    fn tick(&mut self) {
        let now = Instant::now();
        if self.count > 0 {
            let dt = (now - self.last).as_secs_f32() * 1000.0;
            self.total_ms += dt;
            if dt > self.max_ms {
                self.max_ms = dt;
            }
        }
        self.count += 1;
        self.last = now;
    }

    fn summary(&self) -> String {
        if self.count < 3 {
            return format!("样本过少（{} 帧）", self.count);
        }
        format!(
            "{} 帧，平均 {:.1}ms，最大 {:.1}ms",
            self.count,
            self.total_ms / (self.count - 1) as f32,
            self.max_ms
        )
    }
}

/// 睡到距起点第 idx 帧的时刻（60fps 步进）。最后 1.5ms 用 yield 兜住，避免 Sleep 精度不足造成的抖动。
fn wait_frame(origin: Instant, idx: u32) {
    let deadline = origin + Duration::from_nanos(FRAME_NS * idx as u64);
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remain = deadline - now;
        if remain > Duration::from_micros(1500) {
            std::thread::sleep(remain - Duration::from_micros(1500));
        } else {
            std::thread::yield_now();
        }
    }
}

enum CycleEnd {
    Done,
    Next(Payload),
    Shutdown,
}

/// 保证 timeBeginPeriod/timeEndPeriod 严格配对，无论从哪个出口返回。
struct PeriodGuard;

impl PeriodGuard {
    fn new() -> Self {
        unsafe { timeBeginPeriod(1) };
        Self
    }
}

impl Drop for PeriodGuard {
    fn drop(&mut self) {
        unsafe { timeEndPeriod(1) };
    }
}

/// 一轮动画：淡入上滑 180ms → 停留 1500ms → 淡出下滑 200ms。
/// 停留期间不重绘、不轮询，只阻塞等待新通知。
unsafe fn run_cycle(
    hwnd: HWND,
    payload: Payload,
    slot: &mut Option<Surface>,
    rx: &Receiver<Payload>,
    style_cache: &mut Option<(f32, TextStyle)>,
) -> CycleEnd {
    let scale = dpi_scale();
    let dark = system_dark();
    let Some(plan) = prepare(&payload, scale, dark, slot, style_cache) else {
        return CycleEnd::Done;
    };
    let Some(surface) = slot.as_ref() else {
        return CycleEnd::Done;
    };
    let slide = SLIDE * scale;
    let slide_px = slide.max(1.0) as i32;
    let max_alpha = ((crate::settings::opacity() * 255 + 50) / 100).min(255) as u8;
    let hold_ms = crate::settings::duration_ms() as u64;
    let since = payload.since();

    // 关闭动画：直接显示整卡，停留 duration_ms 后隐藏。
    if !crate::settings::animation() {
        present(hwnd, surface, plan.x, plan.y, max_alpha);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        crate::settings::log_line(&format!(
            "首帧（静态） {:.1}ms",
            since.elapsed().as_secs_f32() * 1000.0
        ));
        let hold_end = Instant::now() + Duration::from_millis(hold_ms);
        loop {
            let now = Instant::now();
            if now >= hold_end {
                break;
            }
            match rx.recv_timeout(hold_end - now) {
                Ok(next) => return CycleEnd::Next(next),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return CycleEnd::Shutdown;
                }
            }
        }
        ShowWindow(hwnd, SW_HIDE);
        return CycleEnd::Done;
    }

    let _period = PeriodGuard::new();
    let mut stats = FrameStats::new();

    // 先在屏幕外/全透明状态摆好第一帧，再显示，避免闪一下白块。
    present(hwnd, surface, plan.x, plan.y + slide_px, 0);
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    crate::settings::log_line(&format!(
        "首帧 {:.1}ms（含排版与绘制）",
        since.elapsed().as_secs_f32() * 1000.0
    ));

    let enter_origin = Instant::now();
    let mut idx: u32 = 0;
    loop {
        let t = enter_origin.elapsed().as_secs_f32() * 1000.0;
        if t >= FADE_IN_MS as f32 {
            break;
        }
        let eased = ease_out_cubic((t / FADE_IN_MS as f32).clamp(0.0, 1.0));
        present(
            hwnd,
            surface,
            plan.x,
            plan.y + (slide * (1.0 - eased)) as i32,
            (eased * max_alpha as f32) as u8,
        );
        stats.tick();
        match rx.try_recv() {
            Ok(next) => {
                return CycleEnd::Next(next);
            }
            Err(TryRecvError::Disconnected) => {
                return CycleEnd::Shutdown;
            }
            Err(TryRecvError::Empty) => {}
        }
        idx += 1;
        wait_frame(enter_origin, idx);
    }
    present(hwnd, surface, plan.x, plan.y, max_alpha);
    stats.tick();

    // 停留阶段：完全不重绘，只等待（有新通知就立刻换内容重新入场）。
    let hold_end = Instant::now() + Duration::from_millis(hold_ms);
    loop {
        let now = Instant::now();
        if now >= hold_end {
            break;
        }
        match rx.recv_timeout(hold_end - now) {
            Ok(next) => {
                return CycleEnd::Next(next);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return CycleEnd::Shutdown;
            }
        }
    }
    // 停留不算动画帧，重新计时，避免统计里出现 1500ms 的"假掉帧"。
    stats.last = Instant::now();

    let leave_origin = Instant::now();
    idx = 0;
    loop {
        let t = leave_origin.elapsed().as_secs_f32() * 1000.0;
        if t >= FADE_OUT_MS as f32 {
            break;
        }
        let eased = ease_out_cubic((t / FADE_OUT_MS as f32).clamp(0.0, 1.0));
        present(
            hwnd,
            surface,
            plan.x,
            plan.y + (slide * eased) as i32,
            ((1.0 - eased) * max_alpha as f32) as u8,
        );
        stats.tick();
        match rx.try_recv() {
            Ok(next) => {
                return CycleEnd::Next(next);
            }
            Err(TryRecvError::Disconnected) => {
                return CycleEnd::Shutdown;
            }
            Err(TryRecvError::Empty) => {}
        }
        idx += 1;
        wait_frame(leave_origin, idx);
    }

    present(hwnd, surface, plan.x, plan.y + slide_px, 0);
    ShowWindow(hwnd, SW_HIDE);
    crate::settings::log_line(&format!("通知动画 {}", stats.summary()));
    CycleEnd::Done
}

/// 预热：把字体 / GDI+ / DIB 的首次初始化成本在启动时付掉，
/// 否则启用后的第一次复制首帧会多出上百毫秒的冷启动开销。
unsafe fn warm_up(
    slot: &mut Option<Surface>,
    style_cache: &mut Option<(f32, TextStyle)>,
) {
    let payload = Payload::Text {
        text: "预热".to_string(),
        since: Instant::now(),
    };
    if prepare(&payload, dpi_scale(), system_dark(), slot, style_cache).is_some() {
        crate::settings::log_line("通知组件已预热");
    }
}

/// 常驻动画线程：空闲时阻塞在 channel 上，不占 CPU。
unsafe fn animation_loop(raw_hwnd: isize, rx: Receiver<Payload>) {
    let hwnd = raw_hwnd as HWND;
    let mut slot: Option<Surface> = None;
    let mut style_cache: Option<(f32, TextStyle)> = None;
    warm_up(&mut slot, &mut style_cache);
    loop {
        let payload = match rx.recv() {
            Ok(payload) => payload,
            Err(_) => return,
        };
        let mut next = payload;
        loop {
            match run_cycle(hwnd, next, &mut slot, &rx, &mut style_cache) {
                CycleEnd::Done => break,
                CycleEnd::Next(payload) => {
                    next = payload;
                }
                CycleEnd::Shutdown => return,
            }
        }
    }
}

/// UI 线程收到"有新通知"后，把最新内容交给动画线程（排版/绘制都在动画线程里做）。
fn on_request() {
    let payload = {
        let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        let last = queue.pop_back();
        queue.clear();
        last
    };
    let Some(payload) = payload else { return };
    let guard = CARD_TX.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(payload);
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> isize {
    match msg {
        WM_APP_TOAST => {
            on_request();
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod exif_test {
    use super::*;

    /// 用 .selftest/exif_rot.jpg（左红右蓝、EXIF Orientation=6）验证旋转方向。
    #[test]
    fn exif_orientation_is_applied() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".selftest")
            .join("exif_rot.jpg");
        let thumb = unsafe { build_file_thumb(&path) }.expect("应能生成缩略图");
        assert!(
            thumb.h > thumb.w,
            "旋转后应为竖图，实际 {}x{}",
            thumb.w,
            thumb.h
        );
        let w = thumb.w as usize;
        let h = thumb.h as usize;
        let pixel = |x: usize, y: usize| {
            let o = (y * w + x) * 4;
            (thumb.px[o + 2], thumb.px[o + 1], thumb.px[o])
        };
        let top = pixel(w / 2, h / 10);
        let bottom = pixel(w / 2, h - h / 10);
        println!(
            "EXIF Orientation=6，源 200x100 → 缩略图 {}x{}，顶部 RGB={:?}，底部 RGB={:?}",
            thumb.w, thumb.h, top, bottom
        );
        assert!(top.0 > 180 && top.2 < 80, "顶部应为红，实际 {:?}", top);
        assert!(bottom.2 > 180 && bottom.0 < 80, "底部应为蓝，实际 {:?}", bottom);
    }
}
