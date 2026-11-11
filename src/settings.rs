//! 插件设置：由宿主通过 initialize / settingsChanged 下发。所有项存在原子量里，
//! 设置变更后立即生效，不需要重启插件进程。

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use serde_json::Value;

/// 默认音量（百分比），与 plugin.json 的 default 一致。
pub const DEFAULT_VOLUME: u32 = 70;
/// 默认同内容静默窗（秒）；默认 0 = 不静默，用户主动复制每次都提示。
pub const DEFAULT_QUIET_SEC: u32 = 0;
/// 默认无所有者记忆窗（分钟）；0 = 关闭这项静默。
pub const DEFAULT_NO_OWNER_MEMORY_MIN: u32 = 15;
/// 默认忽略进程。
pub const DEFAULT_IGNORE_PROCESS: &str = "winshun.exe";

static SOUND: AtomicBool = AtomicBool::new(true);
static VOLUME: AtomicU32 = AtomicU32::new(DEFAULT_VOLUME);
static IGNORE: Mutex<Vec<String>> = Mutex::new(Vec::new());
static DURATION_MS: AtomicU32 = AtomicU32::new(1500);
static MAX_WIDTH: AtomicU32 = AtomicU32::new(380);
static OPACITY: AtomicU32 = AtomicU32::new(100);
static THUMBNAIL: AtomicBool = AtomicBool::new(true);
static ANIMATION: AtomicBool = AtomicBool::new(true);
static QUIET_SEC: AtomicU32 = AtomicU32::new(DEFAULT_QUIET_SEC);
static NO_OWNER_MEMORY_MIN: AtomicU32 = AtomicU32::new(DEFAULT_NO_OWNER_MEMORY_MIN);
static LOG: AtomicBool = AtomicBool::new(false);
/// 宿主给的数据目录（WS_DATA_DIR 的等价物）；日志只写这里。
static DATA_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// 串行化日志写入，避免两个线程的时间戳和正文在文件里交错。
static LOG_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// initialize 里拿到的 dataDir；为空时回退到 WS_DATA_DIR 环境变量。
pub fn set_data_dir(dir: &str) {
    let path = if dir.trim().is_empty() {
        std::env::var("WS_DATA_DIR").ok().filter(|s| !s.is_empty())
    } else {
        Some(dir.to_string())
    };
    *DATA_DIR.lock().unwrap_or_else(|e| e.into_inner()) =
        path.map(PathBuf::from);
}

/// 把 JSON 里的设置应用到原子量。缺项保持当前值；数值越界会被夹到合法区间。
pub fn apply(v: &Value) {
    if let Some(b) = v.get("sound").and_then(as_bool) {
        SOUND.store(b, Ordering::SeqCst);
    }
    if let Some(n) = v.get("volume").and_then(as_u32) {
        VOLUME.store(n.min(100), Ordering::SeqCst);
    }
    if let Some(n) = v.get("duration_ms").and_then(as_u32) {
        DURATION_MS.store(n.clamp(500, 10_000), Ordering::SeqCst);
    }
    if let Some(n) = v.get("max_width").and_then(as_u32) {
        MAX_WIDTH.store(n.clamp(240, 800), Ordering::SeqCst);
    }
    if let Some(n) = v.get("opacity").and_then(as_u32) {
        OPACITY.store(n.clamp(30, 100), Ordering::SeqCst);
    }
    if let Some(b) = v.get("thumbnail").and_then(as_bool) {
        THUMBNAIL.store(b, Ordering::SeqCst);
    }
    if let Some(b) = v.get("animation").and_then(as_bool) {
        ANIMATION.store(b, Ordering::SeqCst);
    }
    if let Some(n) = v.get("quiet_window_sec").and_then(as_u32) {
        QUIET_SEC.store(n.min(300), Ordering::SeqCst);
    }
    if let Some(n) = v.get("no_owner_memory_min").and_then(as_u32) {
        NO_OWNER_MEMORY_MIN.store(n.min(1440), Ordering::SeqCst);
    }
    if let Some(b) = v.get("log").and_then(as_bool) {
        LOG.store(b, Ordering::SeqCst);
    }
    if let Some(text) = v.get("ignore_process").and_then(|x| x.as_str()) {
        let list = parse_ignore_process(text);
        *IGNORE.lock().unwrap_or_else(|e| e.into_inner()) = list;
    }
    log_line(&format!(
        "设置：sound={} volume={} duration={}ms max_width={} opacity={} thumbnail={} animation={} quiet={}s no_owner_mem={}min log={} ignore=[{}]",
        sound(),
        volume(),
        duration_ms(),
        max_width(),
        opacity(),
        thumbnail(),
        animation(),
        quiet_sec(),
        no_owner_memory_min(),
        log_enabled(),
        ignore_list().join(", ")
    ));
}

pub fn sound() -> bool {
    SOUND.load(Ordering::SeqCst)
}

pub fn volume() -> u32 {
    VOLUME.load(Ordering::SeqCst)
}

pub fn duration_ms() -> u32 {
    DURATION_MS.load(Ordering::SeqCst)
}

pub fn max_width() -> u32 {
    MAX_WIDTH.load(Ordering::SeqCst)
}

pub fn opacity() -> u32 {
    OPACITY.load(Ordering::SeqCst)
}

pub fn thumbnail() -> bool {
    THUMBNAIL.load(Ordering::SeqCst)
}

pub fn animation() -> bool {
    ANIMATION.load(Ordering::SeqCst)
}

pub fn quiet_sec() -> u32 {
    QUIET_SEC.load(Ordering::SeqCst)
}

/// 同内容静默窗（毫秒）；0 表示不做这项去重。
pub fn quiet_ms() -> u128 {
    quiet_sec() as u128 * 1000
}

pub fn no_owner_memory_min() -> u32 {
    NO_OWNER_MEMORY_MIN.load(Ordering::SeqCst)
}

/// 无所有者记忆窗（毫秒）；0 表示不做这项静默。
pub fn no_owner_memory_ms() -> u128 {
    no_owner_memory_min() as u128 * 60_000
}

pub fn log_enabled() -> bool {
    LOG.load(Ordering::SeqCst)
}

pub fn ignore_list() -> Vec<String> {
    IGNORE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 写入方进程是否在忽略名单内（名字由调用方转成小写、带 .exe）。
pub fn is_ignored_owner(process_name: &str) -> bool {
    let name = process_name.trim();
    let name = name.strip_suffix(".exe").unwrap_or(name);
    IGNORE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|item| item == name)
}

/// 初始化缺省值：未下发 ignore_process 时也默认忽略 WinShun。
pub fn reset_defaults() {
    *IGNORE.lock().unwrap_or_else(|e| e.into_inner()) =
        normalize_process(DEFAULT_IGNORE_PROCESS).into_iter().collect();
}

fn as_bool(v: &Value) -> Option<bool> {
    v.as_bool()
}

fn as_u32(v: &Value) -> Option<u32> {
    if let Some(n) = v.as_u64() {
        return Some(n.min(u32::MAX as u64) as u32);
    }
    if let Some(n) = v.as_i64() {
        return Some(n.max(0) as u32);
    }
    if let Some(n) = v.as_f64() {
        return Some(n.max(0.0).round() as u32);
    }
    v.as_str()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|n| n.max(0.0).round() as u32)
}

/// `ignore_process = a.exe, b`：逗号或分号分隔，可省略 .exe，不区分大小写。
fn parse_ignore_process(text: &str) -> Vec<String> {
    let mut valid = Vec::new();
    for raw in text.split([',', ';']) {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if let Some(name) = normalize_process(raw) {
            if !valid.contains(&name) {
                valid.push(name);
            }
        }
    }
    valid
}

/// 规范化进程名：小写、去首尾空格、去掉 .exe 后缀；含路径/通配符视为非法。
fn normalize_process(raw: &str) -> Option<String> {
    let name = raw.trim().to_lowercase();
    if name.is_empty() {
        return None;
    }
    if name
        .chars()
        .any(|c| matches!(c, '\\' | '/' | ':' | '*' | '?' | '<' | '>' | '|' | '"'))
    {
        return None;
    }
    let name = name.strip_suffix(".exe").unwrap_or(&name).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// 受 `log` 开关控制；写到 dataDir\plugin.log（超过 1MB 自动清空）。
/// 只写元数据（长度、尺寸、耗时、进程名），不写剪贴板明文。
pub fn log_line(msg: &str) {
    if !LOG.load(Ordering::SeqCst) {
        return;
    }
    let _write_guard = LOG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = DATA_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(dir) = dir else { return };
    let path = dir.join("plugin.log");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > LOG_MAX_BYTES {
            let _ = std::fs::remove_file(&path);
        }
    }
    let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st) };
    let stamp = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    );
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = writeln!(f, "[{stamp}] {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_process, parse_ignore_process};

    #[test]
    fn ignore_process_case_and_suffix() {
        assert_eq!(
            parse_ignore_process("WINSHUN.exe, foo ; bar"),
            vec!["winshun".to_string(), "foo".to_string(), "bar".to_string()]
        );
        assert!(parse_ignore_process("").is_empty());
        assert!(normalize_process(r"C:\Tools\a.exe").is_none());
        assert!(normalize_process(".exe").is_none());
    }
}
