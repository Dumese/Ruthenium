fn main() {
    tauri_build::build();
    // 测试二进制（tests/）不会经过 tauri-build 的 manifest 嵌入，而 wry/tauri
    // 按序号导入 comctl32 v6 的 TaskDialog* 等入口；无 manifest 时加载器绑定
    // comctl32 v5，进程启动即 0xC0000139。给测试目标注入 Common-Controls v6 依赖。
    //
    // 注意：`rustc-link-arg-tests` 只覆盖 `tests/` 集成测试，**不覆盖 lib 单测 harness**
    // （cargo test --lib）。lib 单测的同类注入见仓库根 `.cargo/config.toml`。
    #[cfg(target_os = "windows")]
    {
        println!(
            r#"cargo:rustc-link-arg-tests=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' publicKeyToken='6595b64144ccf1df' language='*' processorArchitecture='*'"#
        );
    }
}
