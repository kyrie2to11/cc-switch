//! 窗口状态文件(`.window-state.json`)的尺寸护栏。
//!
//! 背景:tao 的 GTK 后端 `inner_size()` 返回「缓存 logical 尺寸 × 缓存 scale」,
//! `set_size()` 又用「当时缓存 scale」做物理→逻辑换算。在混合缩放多屏
//! (如 scale 1.0 + 1.3333)或登录时显示器布局尚未就绪的竞态下,这一往返会把
//! 物理像素当 logical 写回(或反之),窗口尺寸按 scale 比率逐周期膨胀,实测可
//! 累积到 20480×11264 这类远超任何显示器的值。window-state 插件恢复该值后,
//! Wayland 合成器把窗口钳制到整屏:表现为「假最大化 + 窗口控制按钮失效 +
//! 巨型 WebView 视口拖影」,且退出时保存的仍是坏值,开机自启动每天复现。
//!
//! 本模块不修 tao(上游问题),只在 cc-switch 侧的收口点钳制:
//! 1. `startup_sanitize`:进程启动、插件读取状态文件之前,把超出静态上限
//!    (8K)或低于合理下限的窗口条目重置(删除条目,插件按默认尺寸创建)。
//! 2. `clamp_state_file_to_monitors`:配合 `save_window_state_before_exit`,
//!    在落盘后按当前显示器的最大物理尺寸钳制文件内容,保证坏值无法持久化。
//! 3. `reconcile_window_to_monitor`:窗口显示路径上,若 `inner_size` 超出
//!    所在显示器尺寸,立即修正(兜底同一会话内的异常,如运行中拔掉显示器)。

use serde_json::Value;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, WebviewWindow};

const STATE_FILENAME: &str = ".window-state.json";
// 与 tauri.conf.json 的 identifier 保持一致;此处无法通过 AppHandle 获取,
// 因为 startup_sanitize 必须在 Tauri Builder 构造(插件读状态文件)之前执行。
const APP_IDENTIFIER: &str = "com.ccswitch.desktop";

/// 静态护栏:任何消费级显示器都不超过 8K;低于最小值视为脏数据。
const HARD_MAX_W: u64 = 7680;
const HARD_MAX_H: u64 = 4320;
const HARD_MIN_W: u64 = 300;
const HARD_MIN_H: u64 = 200;

fn state_file_path() -> Option<PathBuf> {
    // 与 tauri 的 app_config_dir 一致:Linux ~/.config/{id}、
    // macOS ~/Library/Application Support/{id}、Windows %APPDATA%\{id}
    dirs::config_dir().map(|d| d.join(APP_IDENTIFIER).join(STATE_FILENAME))
}

/// 进程启动时调用(须早于 Tauri Builder 构造)。
pub fn startup_sanitize() {
    if let Some(path) = state_file_path() {
        sanitize_file(&path, HARD_MAX_W, HARD_MAX_H);
    }
}

/// 读取状态文件,丢弃 width/height 越界的窗口条目,原子写回。
fn sanitize_file(path: &Path, max_w: u64, max_h: u64) {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return, // 文件不存在或不可读:交给插件按首次启动处理
    };
    let mut root = match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => map,
        _ => return, // 结构损坏:不碰,插件自己会忽略/重建
    };

    let mut changed = false;
    root.retain(|_label, entry| {
        let (w, h) = match (
            entry.get("width").and_then(|v| v.as_u64()),
            entry.get("height").and_then(|v| v.as_u64()),
        ) {
            (Some(w), Some(h)) => (w, h),
            _ => return true, // 缺字段:结构由插件定义,不在这里猜
        };
        let keep = w >= HARD_MIN_W && h >= HARD_MIN_H && w <= max_w && h <= max_h;
        if !keep {
            changed = true;
            log::warn!("窗口状态条目尺寸 {w}x{h} 越界(上限 {max_w}x{max_h}),已重置");
        }
        keep
    });

    if !changed {
        return;
    }
    if root.is_empty() {
        // 所有条目都越界:直接删文件,等价于首次启动
        let _ = std::fs::remove_file(path);
        return;
    }
    let tmp = path.with_extension("json.tmp");
    match serde_json::to_string(&Value::Object(root)) {
        Ok(text) => {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
        Err(e) => log::warn!("窗口状态文件重写失败: {e}"),
    }
}

/// 在 `save_window_state` 落盘之后调用:按当前所有显示器的最大物理尺寸
/// (各轴独立取最大,允许窗口横跨到最大屏的尺寸)再次钳制,确保坏值不落盘。
/// 显示器查询失败时退化为静态 8K 上限。
pub fn clamp_state_file_to_monitors(app: &AppHandle) {
    let (mut max_w, mut max_h) = (HARD_MAX_W, HARD_MAX_H);
    if let Ok(monitors) = app.available_monitors() {
        let mut mw = 0;
        let mut mh = 0;
        for m in &monitors {
            let s = m.size();
            mw = mw.max(s.width as u64);
            mh = mh.max(s.height as u64);
        }
        if mw > 0 && mh > 0 {
            max_w = max_w.min(mw);
            max_h = max_h.min(mh);
        }
    }
    if let Some(path) = state_file_path() {
        sanitize_file(&path, max_w, max_h);
    }
}

/// 若窗口 inner_size 超出其所在显示器的物理尺寸,立即钳制到显示器边界。
/// 在窗口显示路径(nudge 之前)调用;查询失败时静默跳过。
pub fn reconcile_window_to_monitor(window: &WebviewWindow) {
    let size = match window.inner_size() {
        Ok(s) => s,
        Err(_) => return,
    };
    let monitor = match window.current_monitor() {
        Ok(Some(m)) => m,
        _ => return,
    };
    let m = monitor.size();
    if size.width > m.width || size.height > m.height {
        let w = size.width.min(m.width);
        let h = size.height.min(m.height);
        if let Err(e) = window.set_size(tauri::PhysicalSize::new(w, h)) {
            log::warn!("窗口尺寸钳制失败: {e}");
        } else {
            log::warn!(
                "窗口尺寸 {}x{} 超出显示器 {}x{},已钳制",
                size.width,
                size.height,
                m.width,
                m.height
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_state(dir: &Path, json: &str) -> PathBuf {
        let p = dir.join(STATE_FILENAME);
        std::fs::write(&p, json).unwrap();
        p
    }

    #[test]
    fn drops_oversized_entry_and_deletes_file_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_state(
            dir.path(),
            r#"{"main":{"width":20480,"height":11264,"x":0,"y":0,"maximized":false}}"#,
        );
        sanitize_file(&p, 2560, 1600);
        assert!(!p.exists(), "唯一条目越界时应删除整个文件");
    }

    #[test]
    fn keeps_sane_entries_when_dropping_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_state(
            dir.path(),
            r#"{"main":{"width":1000,"height":650,"x":10,"y":20,"maximized":false},"other":{"width":99999,"height":650,"x":0,"y":0,"maximized":false}}"#,
        );
        sanitize_file(&p, 2560, 1600);
        let text = std::fs::read_to_string(&p).unwrap();
        let root: Value = serde_json::from_str(&text).unwrap();
        assert!(root.get("main").is_some(), "正常条目应保留");
        assert!(root.get("other").is_none(), "越界条目应删除");
    }

    #[test]
    fn keeps_entry_smaller_than_monitor_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_state(
            dir.path(),
            r#"{"main":{"width":2560,"height":1440,"x":0,"y":0,"maximized":false}}"#,
        );
        sanitize_file(&p, 2560, 1600);
        assert!(p.exists(), "等于显示器尺寸的窗口(钳制后保存的)不应被重置");
    }

    #[test]
    fn ignores_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_state(dir.path(), "not json at all");
        sanitize_file(&p, 2560, 1600);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "not json at all");
    }

    #[test]
    fn drops_too_small_entry() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_state(
            dir.path(),
            r#"{"main":{"width":10,"height":10,"x":0,"y":0,"maximized":false}}"#,
        );
        sanitize_file(&p, 2560, 1600);
        assert!(!p.exists());
    }
}
