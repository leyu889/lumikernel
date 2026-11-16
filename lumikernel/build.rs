// 让 cargo 追踪汇编与链接脚本：global_asm!(include_str!) 的依赖不会被自动发现，
// 少了这个文件就会出现"改了 boot.S 但产物没变"的假象。
fn main() {
    println!("cargo:rerun-if-changed=src/boot.S");
    println!("cargo:rerun-if-changed=src/intr/stubs.S");
    println!("cargo:rerun-if-changed=../../linker.ld");
    println!("cargo:rerun-if-changed=build.rs");

    // 崩溃演示：设置环境变量 LUMI_CRASH_DEMO=1 时，内核会在自检末尾故意
    // 触发一次**未被探针接住**的缺页，用来验证"崩溃现场转储"这条路径。
    // 默认不开——正常版本必须干净启动并常驻。
    println!("cargo:rerun-if-env-changed=LUMI_CRASH_DEMO");
    println!("cargo:rustc-check-cfg=cfg(crash_demo)");
    if std::env::var("LUMI_CRASH_DEMO").map(|v| v == "1").unwrap_or(false) {
        println!("cargo:rustc-cfg=crash_demo");
    }

    // ---- M11 长跑（S3）----
    // LUMI_SOAK=1       ：自检跑完后进入**无限循环的长跑模式**（只能关机退出）
    // LUMI_SOAK_FAST=1  ：采样间隔从 10 分钟缩到 1 秒（演示用，正式跑 30 天不开）
    // LUMI_SOAK_LEAK=1  ：**故意每 5000 轮漏一帧**，用来验证"回归检测器抓得到"
    //                     ——这是负对照：不带它跑出来的"零漂移"才可信
    for (env, cfg) in [
        ("LUMI_SOAK", "soak"),
        ("LUMI_SOAK_FAST", "soak_fast"),
        ("LUMI_SOAK_LEAK", "soak_leak"),
    ] {
        println!("cargo:rerun-if-env-changed={}", env);
        println!("cargo:rustc-check-cfg=cfg({})", cfg);
        if std::env::var(env).map(|v| v == "1").unwrap_or(false) {
            println!("cargo:rustc-cfg={}", cfg);
        }
    }
}
