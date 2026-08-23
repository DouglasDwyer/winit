use cfg_aliases::cfg_aliases;

fn main() {
    // The script doesn't depend on our code.
    println!("cargo:rerun-if-changed=build.rs");

    // Setup cfg aliases.
    cfg_aliases! {
        // Systems.
        android_platform: { target_os = "android" },
        // wasm32-unknown-emscripten also uses winit's web backend: it's
        // still a browser wasm-bindgen/web-sys target like
        // wasm32-unknown-unknown, just with libc/Emscripten runtime
        // support alongside (needed to statically link against a
        // non-Rust codebase sharing the same linear memory). `free_unix`
        // below deliberately excludes target_os = "emscripten" so this
        // is its only platform_impl match.
        web_platform: { all(target_family = "wasm", any(target_os = "unknown", target_os = "emscripten")) },
        macos_platform: { target_os = "macos" },
        ios_platform: { target_os = "ios" },
        windows_platform: { target_os = "windows" },
        apple: { any(target_os = "ios", target_os = "macos") },
        free_unix: { all(unix, not(apple), not(android_platform), not(target_os = "emscripten")) },
        redox: { target_os = "redox" },

        // Native displays.
        x11_platform: { all(feature = "x11", free_unix, not(redox)) },
        wayland_platform: { all(feature = "wayland", free_unix, not(redox)) },
        orbital_platform: { redox },
    }

    // Winit defined cfgs.
    println!("cargo:rustc-check-cfg=cfg(unreleased_changelogs)");
}
