use std::{env, path::PathBuf, process::Command};
fn main() {
    println!("cargo:rerun-if-changed=macos/Capture.swift");
    println!("cargo:rerun-if-changed=macos/CaptureCore.swift");
    println!("cargo:rerun-if-changed=macos/UpdateProxy.swift");
    println!("cargo:rerun-if-changed=macos/LoginItem.swift");
    println!("cargo:rerun-if-changed=macos/Notifications.swift");
    println!("cargo:rerun-if-changed=macos/ClipboardHistory.swift");
    println!("cargo:rerun-if-changed=macos/ClipboardHistoryStore.swift");
    println!("cargo:rerun-if-changed=macos/GitHubSocialPreview.swift");
    if env::var_os("CARGO_FEATURE_UI").is_none() {
        return;
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let arch = if env::var("CARGO_CFG_TARGET_ARCH").unwrap() == "aarch64" {
        "arm64"
    } else {
        "x86_64"
    };
    let status = Command::new("xcrun")
        .args([
            "swiftc",
            "-swift-version",
            "5",
            "-O",
            "-parse-as-library",
            "-emit-library",
            "-static",
            "-module-name",
            "ClipCapture",
            "-target",
            &format!("{arch}-apple-macosx12.0"),
            "macos/CaptureCore.swift",
            "macos/Capture.swift",
            "macos/UpdateProxy.swift",
            "macos/LoginItem.swift",
            "macos/Notifications.swift",
            "macos/ClipboardHistory.swift",
            "macos/ClipboardHistoryStore.swift",
            "macos/GitHubSocialPreview.swift",
            "-o",
        ])
        .arg(output.join("libclip_capture.a"))
        .status()
        .expect("Swift compiler required");
    assert!(status.success(), "Capture bridge compilation failed");
    let swift = Command::new("xcrun")
        .args(["--find", "swiftc"])
        .output()
        .unwrap();
    let swift = PathBuf::from(String::from_utf8(swift.stdout).unwrap().trim());
    println!("cargo:rustc-link-search=native={}", output.display());
    println!(
        "cargo:rustc-link-search=native={}",
        swift
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("lib/swift/macosx")
            .display()
    );
    let sdk = Command::new("xcrun")
        .arg("--show-sdk-path")
        .output()
        .unwrap();
    println!(
        "cargo:rustc-link-search=native={}/usr/lib/swift",
        String::from_utf8(sdk.stdout).unwrap().trim()
    );
    println!("cargo:rustc-link-lib=static=clip_capture");
    // rustc's -nodefaultlibs skips Clang's builtins. SwiftUI availability
    // checks emitted by Xcode 16.4 need __isPlatformVersionAtLeast from them.
    let clang_runtime = Command::new("xcrun")
        .args(["clang", "--print-file-name=libclang_rt.osx.a"])
        .output()
        .expect("Clang is required to link the Swift bridge");
    let clang_runtime_path = String::from_utf8(clang_runtime.stdout).unwrap();
    let clang_runtime_path = std::path::Path::new(clang_runtime_path.trim());
    assert!(
        clang_runtime.status.success()
            && clang_runtime_path.is_absolute()
            && clang_runtime_path.is_file(),
        "Clang did not locate libclang_rt.osx.a"
    );
    println!(
        "cargo:rustc-link-arg-bin=mi-clip={}",
        clang_runtime_path.display()
    );
    for framework in [
        "Cocoa",
        "Carbon",
        "ScreenCaptureKit",
        "CoreGraphics",
        "CFNetwork",
        "ServiceManagement",
        "UserNotifications",
        "QuickLookThumbnailing",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
