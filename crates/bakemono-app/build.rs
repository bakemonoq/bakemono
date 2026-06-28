fn main() {
    // only generate Tauri context/permissions for gui builds; the harness skips it
    if std::env::var_os("CARGO_FEATURE_GUI").is_some() {
        tauri_build::build();
    }
}
