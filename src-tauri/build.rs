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
    tauri_build::build();
}
