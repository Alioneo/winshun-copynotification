//! WinShun 常驻插件：监听剪贴板变化，在桌面右下角弹出复制提示。
//! 协议：stdin 逐行 JSON-RPC（initialize / settingsChanged / query / action / shutdown），
//! stdout 只输出协议 JSON。宿主通过 runtime.program 以相对路径启动本 exe。
#![windows_subsystem = "windows"]

mod settings;
mod toast;

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::time::Instant;

use serde_json::Value;
use windows_sys::core::w;
use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats,
    GetClipboardData, GetClipboardOwner, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassW, TranslateMessage, CW_USEDEFAULT, IDC_ARROW, MSG, WM_APP,
    WM_DESTROY, WNDCLASSW, WS_EX_NOACTIVATE,
};

const WM_CLIPBOARDUPDATE_MSG: u32 = 0x031D;
/// 宿主要求退出（shutdown 请求或 stdin 关闭）。
const WM_APP_SHUTDOWN: u32 = WM_APP + 2;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
/// 通知里文字预览的最大字符数，超过追加省略号。
const PREVIEW_MAX_CHARS: usize = 400;
const PNG_MIN_BYTES: usize = 8;

const OPEN_CLIPBOARD_ATTEMPTS: usize = 10;
const OPEN_CLIPBOARD_RETRY_DELAY_MS: u64 = 5;
/// 小米接力：等待"内容后到"的重试节奏，总计约 5.65 秒（与旧版一致）。
const RETRY_DELAYS_MS: [u64; 12] = [
    150, 100, 150, 200, 300, 400, 500, 600, 700, 800, 900, 1000,
];
/// 同一次手机复制常连写两次剪贴板：2 秒内同内容不再重写。
const RELAY_COOLDOWN_MS: u128 = 2000;
/// 手机同步来源在通知与日志里的展示文案（只影响展示，不影响进程匹配与去重判定）。
const SYNC_DISPLAY_NAME: &str = "MIClipboard";

static NOTIFY_LOCK: Mutex<()> = Mutex::new(());
/// 一次复制常连发多条更新消息，只让最后一条真正去读剪贴板。
static NOTIFY_GEN: AtomicU32 = AtomicU32::new(0);
/// 最近一次已处理的剪贴板序号，同一序号不再重复处理。
static LAST_SEQ: AtomicU32 = AtomicU32::new(0);
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static REQUEST_EXIT: AtomicBool = AtomicBool::new(false);
/// 同内容静默窗的最近一次真正展示（内容指纹 + 时间）。
static LAST_NOTICE: Mutex<Option<(Instant, String)>> = Mutex::new(None);
/// 记忆窗内出现过的具体内容键（含被去重拦下的），供无所有者重写旧内容的判定。
static RECENT_KEYS: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());
/// 接力单飞：同一时刻只跑一个接力，期间同步方的重复写入直接丢弃（旧版行为）。
static RELAY_ACTIVE: AtomicBool = AtomicBool::new(false);
/// 最近一次成功接力的时刻和内容：防止同一次复制的第二次写入被再接力一遍。
static LAST_RELAY: Mutex<Option<(Instant, String)>> = Mutex::new(None);

/// 已知的手机→电脑剪贴板同步服务进程特征（小米互联等）。
fn is_sync_owner(process_name: &str) -> bool {
    process_name.contains("mipc") || process_name.contains("xiaomi")
}

/// 识别剪贴板写入方的进程名与进程号（如 WinShun 从历史选中粘贴时）。
unsafe fn clipboard_owner_process() -> Option<(String, u32)> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    let owner = GetClipboardOwner();
    if owner.is_null() {
        return None;
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(owner, &mut pid);
    if pid == 0 {
        return None;
    }
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if process.is_null() {
        return None;
    }
    let mut buf = [0u16; 260];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len);
    CloseHandle(process);
    if ok == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    let name = path
        .rsplit('\\')
        .next()
        .unwrap_or(path.as_str())
        .to_lowercase();
    Some((name, pid))
}

/// 抢占序号：返回 true 表示这是此前未处理过的新序号。
fn claim_new_seq(slot: &AtomicU32, seq: u32) -> bool {
    if seq == 0 {
        return true;
    }
    let mut prev = slot.load(Ordering::SeqCst);
    loop {
        if prev == seq {
            return false;
        }
        match slot.compare_exchange(prev, seq, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(actual) => prev = actual,
        }
    }
}

/// 读取结果：区分"剪贴板被占用"与"确实还没有内容"。
enum ReadOutcome {
    Text(String),
    Empty,
    Busy,
}

unsafe fn open_clipboard_with_retry() -> bool {
    for _ in 0..OPEN_CLIPBOARD_ATTEMPTS {
        if OpenClipboard(std::ptr::null_mut()) != 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(
            OPEN_CLIPBOARD_RETRY_DELAY_MS,
        ));
    }
    false
}

/// 剪贴板里原有的 HGLOBAL 数据备份。接力路径已确认没有图片/文件等富格式，
/// 这里仍按格式逐个复制，失败恢复用。
struct ClipboardBackup {
    items: Vec<(u32, *mut core::ffi::c_void)>,
}

/// 只备份能安全复制的 HGLOBAL 格式；GDI 句柄格式和延迟渲染格式跳过。
/// 任何一个格式复制失败都返回 None，调用方必须放弃写回，不能清空剪贴板。
unsafe fn snapshot_clipboard_formats() -> Option<ClipboardBackup> {
    let mut items = Vec::new();
    let mut format = 0u32;
    loop {
        format = EnumClipboardFormats(format);
        if format == 0 {
            break;
        }
        // CF_BITMAP/CF_METAFILEPICT/CF_DIB/CF_PALETTE/CF_ENHMETAFILE/CF_HDROP
        // 以及 CF_OWNERDISPLAY/私有 GDI 格式不是 HGLOBAL，不能按字节复制。
        if format >= 0x0080 || matches!(format, 2 | 3 | 8 | 9 | 14 | 15 | 17) {
            continue;
        }
        let handle = GetClipboardData(format);
        if handle.is_null() {
            continue;
        }
        let size = GlobalSize(handle);
        if size == 0 {
            continue;
        }
        let src = GlobalLock(handle);
        if src.is_null() {
            return None;
        }
        let copy = GlobalAlloc(GMEM_MOVEABLE, size);
        if copy.is_null() {
            GlobalUnlock(handle);
            return None;
        }
        let dst = GlobalLock(copy);
        if dst.is_null() {
            GlobalUnlock(handle);
            GlobalFree(copy);
            return None;
        }
        std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, size);
        GlobalUnlock(copy);
        GlobalUnlock(handle);
        items.push((format, copy));
    }
    Some(ClipboardBackup { items })
}

/// 写回失败时把原来备份的格式放回去（调用时剪贴板必须仍由本窗口打开且已清空）。
unsafe fn restore_clipboard_formats(mut backup: ClipboardBackup) {
    EmptyClipboard();
    for (format, handle) in backup.items.drain(..) {
        if SetClipboardData(format, handle).is_null() {
            GlobalFree(handle);
            settings::log_line(&format!("恢复原剪贴板格式失败：format={format}"));
        }
    }
}

unsafe fn discard_clipboard_backup(backup: ClipboardBackup) {
    for (_, handle) in backup.items {
        GlobalFree(handle);
    }
}

/// 先分配并写好全局内存，再打开剪贴板替换内容。
/// 写回失败时把原剪贴板格式恢复回去，绝不留下空剪贴板或半成品。
/// 用自己的窗口作为写入者，剪贴板历史软件才能把来源识别为本插件。
unsafe fn write_clipboard_text(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes);
    if hglobal.is_null() {
        return false;
    }
    let ptr = GlobalLock(hglobal);
    if ptr.is_null() {
        GlobalFree(hglobal);
        return false;
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
    GlobalUnlock(hglobal);

    let owner = MAIN_HWND.load(Ordering::SeqCst) as HWND;
    if OpenClipboard(owner) == 0 {
        GlobalFree(hglobal);
        return false;
    }
    // 打开后先完整备份原内容，备份不完整就不动剪贴板。
    let Some(backup) = snapshot_clipboard_formats() else {
        CloseClipboard();
        GlobalFree(hglobal);
        settings::log_line("备份原剪贴板失败，放弃写回，原内容未改动");
        return false;
    };
    if EmptyClipboard() == 0 {
        discard_clipboard_backup(backup);
        CloseClipboard();
        GlobalFree(hglobal);
        return false;
    }
    // 调试构建可用 CN_TEST_FAIL_WRITE 强制写回失败，验证“失败恢复原剪贴板”；
    // 正式构建里 cfg!(debug_assertions) 为 false，这段不会生效。
    let force_fail = cfg!(debug_assertions) && std::env::var("CN_TEST_FAIL_WRITE").is_ok();
    if force_fail || SetClipboardData(CF_UNICODETEXT, hglobal).is_null() {
        restore_clipboard_formats(backup);
        CloseClipboard();
        GlobalFree(hglobal);
        settings::log_line("接力失败：写回被拒绝，已恢复原剪贴板内容");
        return false;
    }
    discard_clipboard_backup(backup);
    CloseClipboard();
    true
}

/// 剪贴板里是否带富文本/图片/文件等格式（只查格式名，不触发渲染）。
/// 兜底：万一误判了来源，绝不把别人的富格式内容重写成纯文本。
unsafe fn has_rich_formats() -> bool {
    use windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable;
    const RICH_FORMAT_IDS: [u32; 4] = [15, 8, 17, 2]; // CF_HDROP, CF_DIB, CF_DIBV5, CF_BITMAP
    if RICH_FORMAT_IDS
        .iter()
        .any(|&id| IsClipboardFormatAvailable(id) != 0)
    {
        return true;
    }
    for name in ["HTML Format", "Rich Text Format", "PNG"] {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let id = windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW(wide.as_ptr());
        if id != 0 && IsClipboardFormatAvailable(id) != 0 {
            return true;
        }
    }
    false
}

unsafe fn read_clipboard_text() -> ReadOutcome {
    if !open_clipboard_with_retry() {
        return ReadOutcome::Busy;
    }
    let result = (|| {
        let handle = GetClipboardData(CF_UNICODETEXT);
        if handle.is_null() {
            return ReadOutcome::Empty;
        }
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            return ReadOutcome::Empty;
        }
        let size = GlobalSize(handle);
        let count = size / 2;
        let slice = std::slice::from_raw_parts(ptr as *const u16, count);
        let end = slice.iter().position(|&c| c == 0).unwrap_or(count);
        GlobalUnlock(handle);
        let text = String::from_utf16_lossy(&slice[..end]);
        if text.trim().is_empty() {
            ReadOutcome::Empty
        } else {
            ReadOutcome::Text(text)
        }
    })();
    CloseClipboard();
    result
}

/// 通知里展示的内容：文本截断到 400 字符；非文本给出图片/文件的说明。
fn preview_of(text: &str) -> String {
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(PREVIEW_MAX_CHARS).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

enum ClipboardPreview {
    Text(String),
    Image(toast::RawImage),
    ImageFile(PathBuf),
    File,
}

/// 一条待展示的通知：展示内容与完整内容指纹。
struct ClipboardNotice {
    preview: ClipboardPreview,
    /// 完整内容的指纹（文本/图片按原始字节哈希，单个文件按路径）。
    key: String,
}

/// FNV-1a 64 位哈希：只求快、不用于安全场景。
fn content_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn text_notice_key(text: &str) -> String {
    format!("text:{:016x}", content_hash(text.as_bytes()))
}

/// CF_HDROP 文件列表：恰好一个文件且扩展名属于常见图片格式时返回该路径。
unsafe fn single_image_file_path() -> Option<PathBuf> {
    if !open_clipboard_with_retry() {
        return None;
    }
    let result = (|| {
        let handle = GetClipboardData(CF_HDROP);
        if handle.is_null() {
            return None;
        }
        let hdrop = handle as HDROP;
        let count = DragQueryFileW(hdrop, u32::MAX, std::ptr::null_mut(), 0);
        if count != 1 {
            return None;
        }
        let len = DragQueryFileW(hdrop, 0, std::ptr::null_mut(), 0) as usize;
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len + 1];
        if DragQueryFileW(hdrop, 0, buf.as_mut_ptr(), buf.len() as u32) == 0 {
            return None;
        }
        let path = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        // 与 toast.rs 的 is_decodable_image_format 白名单需同步维护。
        if matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp" | "tiff" | "tif" | "ico"
        ) {
            Some(path)
        } else {
            None
        }
    })();
    CloseClipboard();
    result
}

/// 复制剪贴板里的图片原始字节：优先 CF_DIBV5（可能带 alpha），其次 CF_DIB，最后 PNG。
unsafe fn read_clipboard_image() -> Option<toast::RawImage> {
    use windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable;
    if !open_clipboard_with_retry() {
        return None;
    }
    let result = (|| {
        for format in [CF_DIBV5, CF_DIB] {
            if IsClipboardFormatAvailable(format) == 0 {
                continue;
            }
            let handle = GetClipboardData(format);
            if handle.is_null() {
                continue;
            }
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                continue;
            }
            let size = GlobalSize(handle) as usize;
            if size < 40 {
                GlobalUnlock(handle);
                continue;
            }
            let bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
            GlobalUnlock(handle);
            return Some(toast::RawImage { bytes, png: false });
        }
        let id = png_clipboard_format();
        if id != 0 && IsClipboardFormatAvailable(id) != 0 {
            let handle = GetClipboardData(id);
            if !handle.is_null() {
                let ptr = GlobalLock(handle);
                if !ptr.is_null() {
                    let size = GlobalSize(handle) as usize;
                    if size > PNG_MIN_BYTES {
                        let bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
                        GlobalUnlock(handle);
                        return Some(toast::RawImage { bytes, png: true });
                    }
                    GlobalUnlock(handle);
                }
            }
        }
        None
    })();
    CloseClipboard();
    result
}

unsafe fn png_clipboard_format() -> u32 {
    use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
    let name: Vec<u16> = "PNG".encode_utf16().chain(std::iter::once(0)).collect();
    RegisterClipboardFormatW(name.as_ptr())
}

unsafe fn clipboard_notice() -> Option<ClipboardNotice> {
    use windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable;
    match read_clipboard_text() {
        ReadOutcome::Text(text) => Some(ClipboardNotice {
            preview: ClipboardPreview::Text(preview_of(&text)),
            key: text_notice_key(&text),
        }),
        _ => {
            if IsClipboardFormatAvailable(CF_HDROP) != 0 {
                // 资源管理器直接复制单个图片文件时也显示缩略图（读取失败再回退文件提示）
                if let Some(path) = single_image_file_path() {
                    let key = format!("file:{}", path.display());
                    return Some(ClipboardNotice {
                        preview: ClipboardPreview::ImageFile(path),
                        key,
                    });
                }
                return Some(ClipboardNotice {
                    preview: ClipboardPreview::File,
                    key: "files".to_string(),
                });
            }
            let png = png_clipboard_format();
            if IsClipboardFormatAvailable(CF_DIB) != 0
                || IsClipboardFormatAvailable(CF_DIBV5) != 0
                || IsClipboardFormatAvailable(2) != 0
                || (png != 0 && IsClipboardFormatAvailable(png) != 0)
            {
                if let Some(raw) = read_clipboard_image() {
                    let key = format!("image:{:016x}", content_hash(&raw.bytes));
                    return Some(ClipboardNotice {
                        preview: ClipboardPreview::Image(raw),
                        key,
                    });
                }
                return Some(ClipboardNotice {
                    preview: ClipboardPreview::Text("已复制图片".to_string()),
                    key: "image-fallback".to_string(),
                });
            }
            None
        }
    }
}

/// 展示一条通知；同一内容在 quiet_window_sec 内被再次写入时跳过（0 = 关闭这项去重）。
unsafe fn show_notice(notice: ClipboardNotice, seq: u32, owner: &str, since: Instant) {
    // 无所有者的后台写入（远程工具退出回写、系统组件重写等）：
    // 具体内容在记忆窗内出现过则视为旧内容重写，静默跳过；新内容照常提示。
    // 粗粒度键（files / image-fallback）不代表具体内容，不参与该判定。
    // 窗长由设置 no_owner_memory_min 决定，0 = 关闭这项静默。
    let memory_ms = settings::no_owner_memory_ms();
    let key_specific = notice.key.starts_with("text:")
        || notice.key.starts_with("image:")
        || notice.key.starts_with("file:");
    if memory_ms > 0 && key_specific {
        let replay = {
            let mut recent = RECENT_KEYS.lock().unwrap_or_else(|e| e.into_inner());
            recent.retain(|(_, at)| at.elapsed().as_millis() < memory_ms);
            let replay =
                owner == "(无所有者)" && recent.iter().any(|(key, _)| *key == notice.key);
            if let Some(entry) = recent.iter_mut().find(|(key, _)| *key == notice.key) {
                entry.1 = Instant::now();
            } else {
                recent.push((notice.key.clone(), Instant::now()));
            }
            replay
        };
        if replay {
            settings::log_line(&format!(
                "无所有者重写旧内容（{} 分钟内已提示过），跳过 seq={seq} 键={}",
                memory_ms / 60_000,
                notice.key
            ));
            return;
        }
    }
    let quiet = settings::quiet_ms();
    let duplicate = {
        let mut last = LAST_NOTICE.lock().unwrap_or_else(|e| e.into_inner());
        let duplicate = quiet > 0
            && last.as_ref().is_some_and(|(at, key)| {
                at.elapsed().as_millis() < quiet && *key == notice.key
            });
        if !duplicate {
            *last = Some((Instant::now(), notice.key.clone()));
        }
        duplicate
    };
    if duplicate {
        settings::log_line(&format!(
            "跳过重复内容（{}s 内同内容）：源 {owner} seq={seq} 键={}",
            settings::quiet_sec(),
            notice.key
        ));
        return;
    }
    settings::log_line(&format!("弹出通知：源 {owner} seq={seq} 键={}", notice.key));
    match notice.preview {
        ClipboardPreview::Text(text) => toast::request_text(text, since),
        ClipboardPreview::Image(raw) => toast::request_image(raw, since),
        ClipboardPreview::ImageFile(path) => toast::request_image_file(&path, since),
        ClipboardPreview::File => toast::request_text("已复制文件".to_string(), since),
    }
}

/// 读取当前剪贴板内容并按去重规则决定是否展示。
unsafe fn notify_clipboard_content(seq: u32, owner: &str, since: Instant) {
    if let Some(notice) = clipboard_notice() {
        show_notice(notice, seq, owner, since);
    }
}

/// 内容延迟到达后的接力重写（与旧版一致）。
unsafe fn relay_when_ready(mut seq: u32, owner: String) {
    let mut busy_count = 0;
    for delay in RETRY_DELAYS_MS {
        std::thread::sleep(std::time::Duration::from_millis(delay));
        // 同步工具常分多步写入（先占位后补内容），序列号推进会一直更新到最新一次。
        let current = GetClipboardSequenceNumber();
        if current != seq {
            seq = current;
        }
        let outcome = {
            // 与通知读取串行化，避免两个线程同时操作剪贴板句柄。
            let _guard = NOTIFY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            read_clipboard_text()
        };
        match outcome {
            ReadOutcome::Text(text) => {
                // 写回前确认剪贴板没有被更新的内容覆盖，避免把新内容改回去。
                if GetClipboardSequenceNumber() != seq {
                    continue;
                }
                // 兜底：若内容带富格式，说明更可能是正常复制，不重写。
                if has_rich_formats() {
                    settings::log_line("内容带富格式，按正常复制处理，不重写");
                    show_notice(
                        ClipboardNotice {
                            preview: ClipboardPreview::Text(preview_of(&text)),
                            key: text_notice_key(&text),
                        },
                        seq,
                        &owner,
                        Instant::now(),
                    );
                    RELAY_ACTIVE.store(false, Ordering::SeqCst);
                    return;
                }
                // 同一内容刚接力过（小米一次复制常连写两次），跳过重写。
                let recently_relayed = LAST_RELAY
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .is_some_and(|(at, last)| {
                        at.elapsed().as_millis() < RELAY_COOLDOWN_MS && *last == text
                    });
                if recently_relayed {
                    settings::log_line("相同内容刚接力过，跳过重写");
                    RELAY_ACTIVE.store(false, Ordering::SeqCst);
                    return;
                }
                if write_clipboard_text(&text) {
                    *LAST_RELAY.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some((Instant::now(), text.clone()));
                    settings::log_line(&format!("接力完成，长度 {} 字符", text.chars().count()));
                    show_notice(
                        ClipboardNotice {
                            preview: ClipboardPreview::Text(preview_of(&text)),
                            key: text_notice_key(&text),
                        },
                        seq,
                        &owner,
                        Instant::now(),
                    );
                } else {
                    // 写回失败只记日志，不提示成功，也不留半成品内容。
                    settings::log_line("接力失败：无法写回剪贴板，原剪贴板已保留");
                }
                RELAY_ACTIVE.store(false, Ordering::SeqCst);
                return;
            }
            ReadOutcome::Empty => {}
            ReadOutcome::Busy => busy_count += 1,
        }
    }
    settings::log_line(&format!(
        "放弃接力：等待约 6 秒仍未取到内容（占用 {busy_count} 次）"
    ));
    // 手机同步过来的可能是图片/文件（没有文本），这里补一条说明性提示。
    let _guard = NOTIFY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    notify_clipboard_content(seq, &owner, Instant::now());
    RELAY_ACTIVE.store(false, Ordering::SeqCst);
}

/// 普通复制：只读取一次内容，绝不改动剪贴板。
fn notify_plain_copy(seq: u32, owner: String, since: Instant) {
    let gen = NOTIFY_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = NOTIFY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 同一轮复制里已经有更新的通知排队：这次直接让位，只保留最后一次读取。
    if gen != NOTIFY_GEN.load(Ordering::SeqCst) {
        return;
    }
    unsafe {
        if GetClipboardSequenceNumber() != seq {
            return;
        }
        notify_clipboard_content(seq, &owner, since);
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE_MSG => {
            let since = Instant::now();
            let seq = GetClipboardSequenceNumber();
            if GetClipboardOwner() == MAIN_HWND.load(Ordering::SeqCst) as HWND {
                settings::log_line(&format!("忽略自身写入 seq={seq}"));
                return 0;
            }
            let owner_process = clipboard_owner_process();
            if let Some((name, pid)) = owner_process.as_ref() {
                if settings::is_ignored_owner(name) {
                    settings::log_line(&format!("忽略来源 {name} (pid={pid}) seq={seq}"));
                    return 0;
                }
            }
            // 来源判定始终按真实进程名，展示文案才换成 MIClipboard。
            let is_sync = owner_process
                .as_ref()
                .map(|(name, _)| is_sync_owner(name))
                .unwrap_or(false);
            let owner_name = owner_process
                .as_ref()
                .map(|(name, _)| name.as_str())
                .unwrap_or("(无所有者)");
            if !claim_new_seq(&LAST_SEQ, seq) {
                settings::log_line(&format!(
                    "跳过同序号重复更新 seq={seq} ← {} (pid={})",
                    if is_sync { SYNC_DISPLAY_NAME } else { owner_name },
                    owner_process.as_ref().map(|(_, pid)| *pid).unwrap_or(0),
                ));
                return 0;
            }
            let owner_label = if is_sync {
                SYNC_DISPLAY_NAME.to_string()
            } else {
                owner_name.to_string()
            };
            settings::log_line(&format!(
                "剪贴板变化 ← {} (pid={}) seq={}",
                owner_label,
                owner_process.as_ref().map(|(_, pid)| *pid).unwrap_or(0),
                seq
            ));
            if is_sync {
                // 小米一次复制常连写两次剪贴板：已有接力在跑则跳过本次；
                // 新内容一律放行，是否重写在接力线程里按内容判断。
                if RELAY_ACTIVE.swap(true, Ordering::SeqCst) {
                    settings::log_line("接力进行中，跳过同步方的重复写入");
                    return 0;
                }
                settings::log_line(&format!("检测到同步来源写入：{owner_label}"));
                std::thread::spawn(move || unsafe { relay_when_ready(seq, owner_label) });
            } else {
                // 其它任意进程的复制：只提示，不碰剪贴板。
                std::thread::spawn(move || notify_plain_copy(seq, owner_label, since));
            }
            0
        }
        WM_APP_SHUTDOWN => {
            PostQuitMessage(0);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn request_exit() {
    REQUEST_EXIT.store(true, Ordering::SeqCst);
    let raw = MAIN_HWND.load(Ordering::SeqCst);
    if raw != 0 {
        unsafe { PostMessageW(raw as HWND, WM_APP_SHUTDOWN, 0, 0) };
    }
}

fn send_json(value: &Value) {
    use std::io::Write;
    let line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn reply_result(id: &Value, result: Value) {
    send_json(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }));
}

fn reply_error(id: &Value, code: i64, message: &str) {
    send_json(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    }));
}

/// 宿主命令行里输入关键词时的结果：只说明插件在运行，不复制任何内容。
fn query_items(params: &Value) -> Value {
    let en = params
        .get("locale")
        .and_then(|v| v.as_str())
        .is_some_and(|l| l == "en");
    let (title, subtitle) = if en {
        ("Clipboard notification is running", "Copy text or an image to see a card at the bottom right")
    } else {
        ("剪贴板通知已开启", "复制文字或图片后会在右下角弹出提示")
    };
    serde_json::json!({
        "items": [{
            "id": "status",
            "title": title,
            "subtitle": subtitle,
            "score": 90
        }]
    })
}

/// stdin 线程：逐行 JSON-RPC。initialize / settingsChanged 只更新设置，窗口由主线程创建。
fn stdin_loop() {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                let data_dir = params
                    .get("dataDir")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                settings::set_data_dir(data_dir);
                settings::reset_defaults();
                if let Some(s) = params.get("settings") {
                    settings::apply(s);
                }
                toast::reload_sound();
                if let Some(id) = id.as_ref() {
                    reply_result(
                        id,
                        serde_json::json!({"protocolVersion": "1", "name": "alioneo.copynotification"}),
                    );
                }
            }
            "settingsChanged" => {
                if let Some(s) = params.get("settings") {
                    settings::apply(s);
                    toast::reload_sound();
                }
            }
            "query" => {
                if let Some(id) = id.as_ref() {
                    reply_result(id, query_items(&params));
                }
            }
            "action" => {
                if let Some(id) = id.as_ref() {
                    reply_result(id, serde_json::json!({"effects": [{"type": "hide"}]}));
                }
            }
            "shutdown" => {
                if let Some(id) = id.as_ref() {
                    reply_result(id, Value::Object(Default::default()));
                }
                request_exit();
                return;
            }
            "cancel" => {}
            _ => {
                if let Some(id) = id.as_ref() {
                    reply_error(id, -32601, &format!("Method not found: {method}"));
                }
            }
        }
    }
    // stdin 关闭说明宿主已经走了。
    request_exit();
}

fn main() {
    std::thread::Builder::new()
        .name("stdin".to_string())
        .spawn(stdin_loop)
        .expect("stdin thread");

    unsafe {
        // 单实例：宿主已保证一个插件一个进程，这里只防手动重复启动。
        // 调试构建用不同的互斥名，便于在宿主运行时单独跑本地验证。
        let instance_name = if cfg!(debug_assertions) {
            w!("Local\\WinShunCopyNotify_alioneo.copynotification_dev")
        } else {
            w!("Local\\WinShunCopyNotify_alioneo.copynotification")
        };
        let mutex = windows_sys::Win32::System::Threading::CreateMutexW(
            std::ptr::null(),
            1,
            instance_name,
        );
        if mutex.is_null()
            || windows_sys::Win32::Foundation::GetLastError()
                == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS
        {
            return;
        }

        let hinstance = GetModuleHandleW(std::ptr::null());

        // 按显示器 DPI 缩放（必须在创建任何窗口之前设置），通知卡片才能对齐右下角。
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
        toast::init();

        let class_name = w!("WinShunCopyNotifyWindow");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinstance;
        wc.lpszClassName = class_name;
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE,
            class_name,
            w!("WinShunCopyNotify"),
            0,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            -3isize as HWND, // HWND_MESSAGE：仅消息窗口，不显示
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            settings::log_line("启动失败：无法创建消息窗口，进程退出");
            return;
        }
        MAIN_HWND.store(hwnd as isize, Ordering::SeqCst);
        AddClipboardFormatListener(hwnd);

        let toast_hwnd = toast::create(hinstance);
        if toast_hwnd.is_null() {
            settings::log_line("警告：通知窗口创建失败，复制提示将不可用");
        }
        settings::log_line(&format!("插件已启动，进程号 {}", std::process::id()));

        // stdin 线程可能在窗口创建前就收到了 shutdown。
        if REQUEST_EXIT.load(Ordering::SeqCst) {
            return;
        }

        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r == 0 {
                settings::log_line("收到退出消息，正常退出");
                break;
            }
            if r < 0 {
                settings::log_line("消息循环异常，200ms 后继续");
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{claim_new_seq, preview_of};
    use std::sync::atomic::AtomicU32;

    #[test]
    fn preview_boundary_400_and_401() {
        let exactly = "中".repeat(400);
        assert_eq!(preview_of(&exactly), exactly);

        let overflow = "中".repeat(401);
        assert_eq!(preview_of(&overflow), format!("{}…", "中".repeat(400)));
    }

    #[test]
    fn same_seq_is_skipped_once() {
        let slot = AtomicU32::new(0);
        assert!(claim_new_seq(&slot, 100));
        assert!(!claim_new_seq(&slot, 100));
        assert!(claim_new_seq(&slot, 101));
        assert!(!claim_new_seq(&slot, 101));
        assert!(claim_new_seq(&slot, 0));
        assert!(!claim_new_seq(&slot, 101));
    }
}
