// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

// Performs a recursive search for a file with name under path and returns the full path if such a
// file is found.
fn scan_path<P: AsRef<Path>, O: AsRef<OsStr>>(path: P, name: O) -> Option<PathBuf> {
    for entry in (fs::read_dir(path).ok()?).flatten() {
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };

        if file_type.is_file() && entry.file_name() == name.as_ref() {
            return Some(entry.path());
        } else if file_type.is_dir() {
            if let Some(found) = scan_path(entry.path(), name.as_ref()) {
                return Some(found);
            }
        }
    }
    None
}

// Searches for the given protocol in both the system wide and bundles protocols path.
fn find_protocol(name: &str) -> PathBuf {
    let protocol_file_name = PathBuf::from(format!("{}.xml", name));
    // Prioritize the systems wayland protocols before using the bundled ones.
    if let Ok(protocols_path) = pkg_config::get_variable("wayland-protocols", "pkgdatadir") {
        if let Some(found) = scan_path(protocols_path, &protocol_file_name) {
            return found;
        }
    }
    let protocols_path = format!("/usr/share/wayland-protocols/stable/{}", name);
    if let Some(found) = scan_path(protocols_path, &protocol_file_name) {
        return found;
    }
    // Use bundled protocols as a fallback.
    let protocol_path = Path::new("protocol").join(protocol_file_name);
    assert!(
        protocol_path.is_file(),
        "unable to locate wayland protocol specification for `{}`",
        name
    );
    protocol_path
}

fn compile_protocol<P: AsRef<Path>>(name: &str, out: P) -> PathBuf {
    let in_protocol = find_protocol(name);
    println!("cargo:rerun-if-changed={}", in_protocol.display());
    let out_code = out.as_ref().join(format!("{}.c", name));
    let out_header = out.as_ref().join(format!("{}.h", name));
    eprintln!("building protocol: {}", name);

    let wayland_scanner = which::which("wayland-scanner")
        .expect("missing wayland-scanner - please install libwayland-dev");

    Command::new(&wayland_scanner)
        .arg("code")
        .arg(&in_protocol)
        .arg(&out_code)
        .output()
        .expect("wayland-scanner code failed");
    Command::new(&wayland_scanner)
        .arg("client-header")
        .arg(&in_protocol)
        .arg(&out_header)
        .output()
        .expect("wayland-scanner client-header failed");
    out_code
}

fn build_wayland() {
    println!("cargo:rerun-if-env-changed=WAYLAND_PROTOCOLS_PATH");
    let out_dir = env::var("OUT_DIR").unwrap();

    let mut build = cc::Build::new();
    build.warnings(true);
    build.warnings_into_errors(true);
    build.include(&out_dir);
    build.flag("-std=gnu11");
    build.file("src/display_wl.c");
    println!("cargo:rerun-if-changed=src/display_wl.c");

    for protocol in &[
        "aura-shell",
        "linux-dmabuf-unstable-v1",
        "xdg-shell",
        "viewporter",
        "virtio-gpu-metadata-v1",
    ] {
        build.file(compile_protocol(protocol, &out_dir));
    }
    build.compile("display_wl");

    println!("cargo:rustc-link-lib=dylib=wayland-client");
}

fn main() {
    // Skip installing dependencies when generating documents.
    if std::env::var("CARGO_DOC").is_ok() {
        return;
    }

    match std::env::var("CARGO_CFG_TARGET_OS").as_deref().unwrap() {
        "linux" | "android" => {
            build_wayland();
        }
        _ => {}
    }
}
