//! docs/10 A6b：夜间后台分析——时间窗口（含跨午夜）、计划任务名与参数、单实例标记。
//! 不在测试里真的注册计划任务。

use std::path::Path;

use chrono::NaiveDate;
use photo_view_plus_lib::services::night_service::{
    background_mutex_name, create_task_args, delete_task_args, gui_mutex_name, night_deadline,
    task_name, BACKGROUND_ANALYZE_ARG,
};
use photo_view_plus_lib::utils::instance::InstanceLock;

fn at(day: u32, hour: u32, minute: u32) -> chrono::NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 10, day)
        .expect("date")
        .and_hms_opt(hour, minute, 0)
        .expect("time")
}

#[test]
fn deadline_inside_a_same_day_window() {
    assert_eq!(night_deadline(at(23, 2, 59), 3, 7), None);
    assert_eq!(night_deadline(at(23, 3, 0), 3, 7), Some(at(23, 7, 0)));
    assert_eq!(night_deadline(at(23, 6, 59), 3, 7), Some(at(23, 7, 0)));
    assert_eq!(night_deadline(at(23, 7, 0), 3, 7), None);
    assert_eq!(night_deadline(at(23, 12, 0), 3, 7), None);
}

#[test]
fn deadline_for_a_window_across_midnight() {
    assert_eq!(night_deadline(at(23, 23, 30), 23, 6), Some(at(24, 6, 0)));
    assert_eq!(night_deadline(at(24, 2, 0), 23, 6), Some(at(24, 6, 0)));
    assert_eq!(night_deadline(at(24, 6, 0), 23, 6), None);
    assert_eq!(night_deadline(at(24, 12, 0), 23, 6), None);
    // 开始等于结束：不是有效窗口。
    assert_eq!(night_deadline(at(24, 3, 0), 3, 3), None);
}

#[test]
fn task_names_do_not_collide_between_profiles() {
    assert_eq!(
        task_name("com.vetoer.photoviewplus"),
        "PhotoViewPlus\\BackgroundAnalyze"
    );
    assert_eq!(
        task_name("com.vetoer.photoviewplus.dev"),
        "PhotoViewPlus\\BackgroundAnalyze-dev"
    );
    assert_eq!(
        task_name("com.vetoer.photoviewplus.test"),
        "PhotoViewPlus\\BackgroundAnalyze-test"
    );
    assert_ne!(
        gui_mutex_name("com.vetoer.photoviewplus"),
        gui_mutex_name("com.vetoer.photoviewplus.dev")
    );
}

#[test]
fn schtasks_arguments_run_daily_with_the_background_flag() {
    let exe = Path::new(r"C:\Program Files\PhotoView+\photo-view-plus.exe");
    let args = create_task_args("PhotoViewPlus\\BackgroundAnalyze", exe, 3);
    let joined = args.join(" ");
    assert!(joined.contains("/SC DAILY /ST 03:00"), "{joined}");
    assert!(
        joined.contains("/TN PhotoViewPlus\\BackgroundAnalyze"),
        "{joined}"
    );
    let run = &args[args.iter().position(|arg| arg == "/TR").expect("/TR") + 1];
    // 路径带空格时要整体加引号。
    assert_eq!(
        run,
        &format!("\"C:\\Program Files\\PhotoView+\\photo-view-plus.exe\" {BACKGROUND_ANALYZE_ARG}")
    );
    assert!(joined.contains("/RL LIMITED"));
    assert!(joined.starts_with("/Create /F"));
    assert_eq!(
        delete_task_args("PhotoViewPlus\\BackgroundAnalyze"),
        ["/Delete", "/F", "/TN", "PhotoViewPlus\\BackgroundAnalyze"]
    );
}

#[cfg(windows)]
#[test]
fn instance_lock_is_exclusive_and_released_on_drop() {
    let name = background_mutex_name(&format!("test.{}", std::process::id()));
    assert!(!InstanceLock::is_held(&name));
    let lock = InstanceLock::acquire(&name).expect("first acquire");
    assert!(InstanceLock::is_held(&name));
    assert!(
        InstanceLock::acquire(&name).is_none(),
        "second acquire must fail"
    );
    drop(lock);
    assert!(!InstanceLock::is_held(&name));
    assert!(InstanceLock::acquire(&name).is_some());
}

/// 真的调一次 schtasks：注册一个指向不存在路径的临时任务，读回存下来的命令行，立即删除。
/// 验证 /TR 里带空格路径的引号经过 Rust 的命令行转义后仍然正确。手动运行：
/// `cargo test --test night_tests -- --ignored schtasks_round_trip`
#[cfg(windows)]
#[test]
#[ignore]
fn schtasks_round_trip() {
    use photo_view_plus_lib::services::night_service::{
        register_task, task_registered, unregister_task,
    };
    let task = r"PhotoViewPlus\SelfTest-DeleteMe";
    let exe = Path::new(r"C:\Program Files\PhotoView+ SelfTest\does-not-exist.exe");
    register_task(task, exe, 3).expect("register");
    let output = std::process::Command::new("schtasks")
        .args(["/Query", "/TN", task, "/XML"])
        .output()
        .expect("query");
    unregister_task(task).expect("unregister");
    assert!(!task_registered(task), "task must be gone");
    // 输出到管道时用控制台代码页（路径是 ASCII，按 UTF-8 读即可）；个别环境是 UTF-16LE。
    let looks_utf16 = output
        .stdout
        .iter()
        .skip(1)
        .step_by(2)
        .take(64)
        .all(|b| *b == 0);
    let xml = if looks_utf16 {
        let units = output
            .stdout
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(&output.stdout).to_string()
    };
    assert!(
        xml.contains(
            r#"<Command>"C:\Program Files\PhotoView+ SelfTest\does-not-exist.exe"</Command>"#
        ),
        "{xml}"
    );
    assert!(
        xml.contains(&format!("<Arguments>{BACKGROUND_ANALYZE_ARG}</Arguments>")),
        "{xml}"
    );
}
