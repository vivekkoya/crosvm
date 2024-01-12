// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

cfg_if::cfg_if! {
    if #[cfg(any(target_os = "android", target_os = "linux"))] {
        mod linux;
        use linux as platform;
    } else if #[cfg(windows)] {
        pub mod windows;
        use windows as platform;
    }
}

pub(crate) use platform::process_rx;
pub(crate) use platform::process_tx;
pub(crate) use platform::validate_and_configure_tap;
pub(crate) use platform::virtio_features_to_tap_offload;
