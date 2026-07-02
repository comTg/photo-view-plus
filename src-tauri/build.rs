fn main() {
    println!("cargo:rerun-if-env-changed=PVP_PROFILE");
    let profile = match std::env::var("PVP_PROFILE").as_deref() {
        Ok("dev" | "test" | "prod") => std::env::var("PVP_PROFILE").unwrap_or_default(),
        _ => "dev".to_string(),
    };
    println!("cargo:rustc-env=PVP_BUILD_PROFILE={profile}");

    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
        std::env::set_var("PROTOC", protoc);
    }

    embed_common_controls_manifest_into_tests();

    tauri_build::build();
}

/// 给 `cargo test` 生成的集成测试二进制嵌入 Common-Controls v6 清单。
///
/// 集成测试链接进 tauri 的 GUI 运行时后，会从 comctl32 导入 v6 专有符号
/// （如 `TaskDialogIndirect`）。测试二进制不经过 tauri-build，默认没有清单，加载器
/// 会绑定到 comctl32 v5，缺这些入口点，启动即报 STATUS_ENTRYPOINT_NOT_FOUND
/// (0xC0000139)。主程序 exe 由 `tauri_build::build()` 注入清单，不受影响。
///
/// 只对 MSVC 目标生效：`/MANIFEST:EMBED` 与 `/MANIFESTINPUT` 是 link.exe 的选项，
/// GNU 工具链不认。清单片段会与链接器自动生成的清单合并后嵌入。
fn embed_common_controls_manifest_into_tests() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("common-controls-v6.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}", manifest.display());
}
