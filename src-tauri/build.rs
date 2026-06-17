fn main() {
    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
        std::env::set_var("PROTOC", protoc);
    }
    tauri_build::build();
}
