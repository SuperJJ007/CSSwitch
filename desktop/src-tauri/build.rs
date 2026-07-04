fn main() {
    // tauri-build 仅在 desktop feature 启用时执行。
    // helper 二进制编译 (`--no-default-features`) 会跳过此步骤。
    #[cfg(feature = "tauri-build")]
    tauri_build::build()
}
