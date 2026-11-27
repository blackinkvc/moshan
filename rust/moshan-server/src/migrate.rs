// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：把「运行数据」从老位置（媒体根）搬到新位置（runtime_dir）。
// 不做：不碰媒体与文件（02-媒体、04-存图、08-阅读…）—— 那是用户的东西，
//       换个盘要他们自己决定，我们只搬自己产出的账与缓存。

use crate::paths::Ctx;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 搬迁标记：写过就代表「已经搬过一轮」，不再重复。
///
/// 用标记而不是每次比对目录，是因为搬迁是**破坏性**的（删源）。
/// 万一用户后来把某个目录又手动放回媒体根（比如从备份恢复），
/// 自动再搬一次只会把两份都搞乱 —— 那种情况交给人。
const MARKER: &str = ".runtime-migrated.json";

/// 统计一棵目录树：(文件数, 总字节)。符号链接按「文件」计，不跟随。
fn count_tree(p: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![p.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(_) => {
                    files += 1;
                    bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
                Err(_) => {}
            }
        }
    }
    (files, bytes)
}

/// 递归复制。目标已存在的同名文件直接覆盖 —— 调用方已经保证目标目录是空的。
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        let ft = e.file_type()?;
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_symlink() {
            // 链接照抄（不解析）：源里的相对链接换根之后仍然指得对；
            // 解析会把链接变成实体文件，体积可能一下翻好多倍。
            #[cfg(unix)]
            {
                let target = std::fs::read_link(&from)?;
                let _ = std::fs::remove_file(&to);
                std::os::unix::fs::symlink(target, &to)?;
            }
            #[cfg(not(unix))]
            {
                std::fs::copy(&from, &to)?;
            }
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 目录是否「空」：不存在或没有条目都算空。
fn is_empty_dir(p: &Path) -> bool {
    match std::fs::read_dir(p) {
        Ok(mut rd) => rd.next().is_none(),
        Err(_) => true,
    }
}

/// 把一个运行类子目录从 `old` 搬到 `new`。
///
/// 顺序固定：**先完整复制 → 核对文件数与字节数 → 一致才删源**。
/// 任何一步不对就保留源并记进失败清单，绝不「删了再说」。
fn move_one(old: &Path, new: &Path) -> Result<(u64, u64), String> {
    if !old.is_dir() {
        return Err("源不是目录".into());
    }
    if !is_empty_dir(new) {
        return Err("目标已存在且非空，跳过（不覆盖你的数据）".into());
    }
    let (f0, b0) = count_tree(old);
    if let Err(e) = copy_tree(old, new) {
        let _ = std::fs::remove_dir_all(new); // 半成品留着更麻烦
        return Err(format!("复制失败：{e}"));
    }
    let (f1, b1) = count_tree(new);
    if (f0, b0) != (f1, b1) {
        let _ = std::fs::remove_dir_all(new);
        return Err(format!(
            "校验不一致（源 {f0} 个文件 / {b0} 字节，副本 {f1} 个 / {b1} 字节），已丢弃副本、保留源"
        ));
    }
    if let Err(e) = std::fs::remove_dir_all(old) {
        // 副本是好的，源删不掉（权限/被占用）：留着源不影响正确性，
        // 只是会占双份空间，报出来让用户自己清。
        return Err(format!("副本已就位，但源删不掉：{e}"));
    }
    Ok((f0, b0))
}

/// 执行一轮搬迁，返回 `{moved: [...], failed: [{name, reason}], skipped: bool}`。
pub fn move_runtime(old_root: &Path, new_root: &Path) -> Value {
    let mut moved: Vec<Value> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();

    if let Err(e) = std::fs::create_dir_all(new_root) {
        return json!({
            "ok": false,
            "error": format!("新位置建不出来：{e}"),
            "moved": [], "failed": []
        });
    }

    for name in crate::paths::RUNTIME_SUBDIRS {
        let old = old_root.join(name);
        let new = new_root.join(name);
        if !old.is_dir() || old == new {
            continue;
        }
        match move_one(&old, &new) {
            Ok((files, bytes)) => moved.push(json!({
                "name": name, "files": files, "bytes": bytes
            })),
            Err(reason) => failed.push(json!({"name": name, "reason": reason})),
        }
    }

    json!({"ok": failed.is_empty(), "moved": moved, "failed": failed})
}

fn marker_path(ctx: &Ctx) -> PathBuf {
    ctx.runtime_dir.join(MARKER)
}

fn done_before(ctx: &Ctx) -> bool {
    marker_path(ctx).is_file()
}

fn write_marker(ctx: &Ctx, result: &Value) {
    let _ = std::fs::create_dir_all(&ctx.runtime_dir);
    let body = json!({
        "at": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string(),
        "from": ctx.library_dir.to_string_lossy(),
        "to": ctx.runtime_dir.to_string_lossy(),
        "result": result,
    });
    let _ = std::fs::write(
        marker_path(ctx),
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    );
}

/// 启动时跑一次（老用户升级上来的路径）。
///
/// 必须放在后台线程里调用：源目录在 ~/Documents 时，macOS 的 TCC 会弹授权框，
/// 而 `open()` 会一直等 —— 在启动主路径上做这件事，端口就永远绑不上。
pub fn runtime_once(ctx: &Ctx) -> Value {
    if done_before(ctx) {
        return json!({"ok": true, "skipped": true});
    }
    // 两个根是同一个地方（老配置、或用户主动指到一起）：没什么可搬的，只立标记。
    if ctx.runtime_dir == ctx.library_dir {
        write_marker(ctx, &json!({"skipped": "两个根相同"}));
        return json!({"ok": true, "skipped": true});
    }
    let result = move_runtime(&ctx.library_dir, &ctx.runtime_dir);
    write_marker(ctx, &result);
    result
}

/// 欢迎页「保存」时用：用户刚把运行目录改到别处，把旧根里的搬过来。
///
/// 与启动时那份的区别：这里不认标记（用户明确要求换位置），
/// 但同样只在目标为空时才搬，不会覆盖新位置已有的东西。
pub fn adopt(old_root: &Path, new_root: &Path) -> Value {
    if old_root == new_root {
        return json!({"ok": true, "skipped": true, "moved": [], "failed": []});
    }
    let result = move_runtime(old_root, new_root);
    // 换过位置就把标记写到新根，免得下次启动又拿新 runtime 去比老 library。
    let _ = std::fs::create_dir_all(new_root);
    let body = json!({
        "at": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string(),
        "from": old_root.to_string_lossy(),
        "to": new_root.to_string_lossy(),
        "by": "welcome",
        "result": result,
    });
    let _ = std::fs::write(
        new_root.join(MARKER),
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("moshan-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn moves_only_runtime_dirs_and_verifies() {
        let base = tmp("a");
        let old = base.join("lib");
        let new = base.join("rt");
        std::fs::create_dir_all(old.join("12-记账/流水")).unwrap();
        std::fs::create_dir_all(old.join("04-存图/图集")).unwrap();
        std::fs::write(old.join("12-记账/流水/a.json"), b"{\"x\":1}").unwrap();
        std::fs::write(old.join("04-存图/图集/p.jpg"), b"jpg").unwrap();

        let r = move_runtime(&old, &new);
        assert!(r["ok"].as_bool().unwrap());
        // 运行数据搬走了
        assert!(new.join("12-记账/流水/a.json").is_file());
        assert!(!old.join("12-记账").exists());
        // 媒体原样留着
        assert!(old.join("04-存图/图集/p.jpg").is_file());
        assert!(!new.join("04-存图").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn does_not_overwrite_non_empty_target() {
        let base = tmp("b");
        let old = base.join("lib");
        let new = base.join("rt");
        std::fs::create_dir_all(old.join("10-系统记录")).unwrap();
        std::fs::create_dir_all(new.join("10-系统记录")).unwrap();
        std::fs::write(old.join("10-系统记录/old.json"), b"1").unwrap();
        std::fs::write(new.join("10-系统记录/new.json"), b"2").unwrap();

        let r = move_runtime(&old, &new);
        assert_eq!(r["failed"].as_array().unwrap().len(), 1);
        // 目标没被动，源也没删
        assert!(new.join("10-系统记录/new.json").is_file());
        assert!(old.join("10-系统记录/old.json").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }
}
