//! Android APK 入口：cargo apk build --example android
//!
//! APK 的入口不是 main 而是 android_main（native-activity 约定）：
//! 先把 AndroidApp 存进 bevy 的全局（bevy_winit 建事件循环时要取），
//! 再跑与 PC 完全相同的启动流程

fn main() {
    bevy_hello::run_app();
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: bevy::android::android_activity::AndroidApp) {
    bevy::android::ANDROID_APP.set(app);
    main();
}
