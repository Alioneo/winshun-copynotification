//! 编译 VERSIONINFO 资源：让 WinShun 剪贴板历史把本插件的显示名识别为 "MIClipboard"。
//! 只影响 exe 的展示名，不改变任何进程匹配/去重逻辑。
use std::path::{Path, PathBuf};
use std::process::Command;

fn find_rc() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RC") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    // PATH 里的 rc.exe
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            let p = Path::new(dir).join("rc.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    // Windows SDK 常见安装位置，取版本号最大的一套
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(kits) {
        for entry in entries.flatten() {
            let p = entry.path().join("x64").join("rc.exe");
            if p.is_file() {
                candidates.push(p);
            }
        }
    }
    candidates.sort();
    candidates.pop()
}

fn main() {
    let rc_file = Path::new("res").join("version.rc");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", rc_file.display());

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let res_file = out_dir.join("version.res");
    let Some(rc) = find_rc() else {
        println!("cargo:warning=找不到 rc.exe，跳过 VERSIONINFO（剪贴板历史来源将显示为默认程序名）");
        return;
    };
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg(format!("/fo{}", res_file.display()))
        .arg(&rc_file)
        .output();
    match status {
        Ok(o) if o.status.success() => {
            println!("cargo:rustc-link-arg={}", res_file.display());
        }
        Ok(o) => {
            println!(
                "cargo:warning=rc.exe 编译 VERSIONINFO 失败：{}",
                String::from_utf8_lossy(&o.stderr).replace('\n', " ")
            );
        }
        Err(e) => {
            println!("cargo:warning=无法执行 rc.exe：{e}");
        }
    }
}
