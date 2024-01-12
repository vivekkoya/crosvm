// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(missing_docs)]

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::rc::Rc;

use anyhow::anyhow;
use anyhow::Result;
use base::IntoRawDescriptor;
use base::MappedRegion;
use base::MemoryMappingArena;
use cros_codecs::decoder::stateless::h264::H264;
use cros_codecs::decoder::stateless::h265::H265;
use cros_codecs::decoder::stateless::vp8::Vp8;
use cros_codecs::decoder::stateless::vp9::Vp9;
use cros_codecs::decoder::stateless::DecodeError;
use cros_codecs::decoder::stateless::StatelessVideoDecoder;
use cros_codecs::decoder::DecodedHandle;
use cros_codecs::libva;
use cros_codecs::libva::Display;
use cros_codecs::multiple_desc_type;
use cros_codecs::utils::DmabufFrame;
use cros_codecs::DecodedFormat;
use cros_codecs::FrameLayout;
use cros_codecs::PlaneLayout;

use crate::virtio::video::decoder::Capability;
use crate::virtio::video::decoder::DecoderBackend;
use crate::virtio::video::decoder::DecoderEvent;
use crate::virtio::video::decoder::DecoderSession;
use crate::virtio::video::error::VideoError;
use crate::virtio::video::error::VideoResult;
use crate::virtio::video::format::Format;
use crate::virtio::video::format::FormatDesc;
use crate::virtio::video::format::FormatRange;
use crate::virtio::video::format::FrameFormat;
use crate::virtio::video::format::Level;
use crate::virtio::video::format::Profile;
use crate::virtio::video::format::Rect;
use crate::virtio::video::resource::BufferHandle;
use crate::virtio::video::resource::GuestMemHandle;
use crate::virtio::video::resource::GuestResource;
use crate::virtio::video::resource::GuestResourceHandle;
use crate::virtio::video::utils::EventQueue;

/// A guest memory descriptor that uses a managed buffer as a shadow that will be copied into the
/// guest memory once decoding is over.
struct GuestMemDescriptor(GuestMemHandle);

impl libva::SurfaceMemoryDescriptor for GuestMemDescriptor {
    fn add_attrs(
        &mut self,
        attrs: &mut Vec<libva::VASurfaceAttrib>,
    ) -> Option<Box<dyn std::any::Any>> {
        // Decode into a managed buffer.
        ().add_attrs(attrs)
    }
}

multiple_desc_type! {
    enum BufferDescriptor {
        GuestMem(GuestMemDescriptor),
        Dmabuf(DmabufFrame),
    }
}

struct BufferDescWithPicId {
    desc: BufferDescriptor,
    picture_buffer_id: i32,
}

impl libva::SurfaceMemoryDescriptor for BufferDescWithPicId {
    fn add_attrs(
        &mut self,
        attrs: &mut Vec<libva::VASurfaceAttrib>,
    ) -> Option<Box<dyn std::any::Any>> {
        self.desc.add_attrs(attrs)
    }
}

/// Represents a buffer we have not yet sent to the accelerator.
struct PendingJob {
    resource_id: u32,
    timestamp: u64,
    resource: GuestResourceHandle,
    offset: usize,
    bytes_used: usize,
    remaining: usize,
}

impl TryFrom<DecodedFormat> for Format {
    type Error = anyhow::Error;

    fn try_from(value: DecodedFormat) -> Result<Self, Self::Error> {
        match value {
            DecodedFormat::NV12 => Ok(Format::NV12),
            _ => Err(anyhow!("Unsupported format")),
        }
    }
}

impl TryFrom<Format> for DecodedFormat {
    type Error = anyhow::Error;

    fn try_from(value: Format) -> Result<Self, Self::Error> {
        match value {
            Format::NV12 => Ok(DecodedFormat::NV12),
            _ => Err(anyhow!("Unsupported format")),
        }
    }
}

impl TryFrom<libva::VAProfile::Type> for Profile {
    type Error = anyhow::Error;

    fn try_from(value: libva::VAProfile::Type) -> Result<Self, Self::Error> {
        match value {
            libva::VAProfile::VAProfileH264Baseline => Ok(Self::H264Baseline),
            libva::VAProfile::VAProfileH264Main => Ok(Self::H264Main),
            libva::VAProfile::VAProfileH264High => Ok(Self::H264High),
            libva::VAProfile::VAProfileH264StereoHigh => Ok(Self::H264StereoHigh),
            libva::VAProfile::VAProfileH264MultiviewHigh => Ok(Self::H264MultiviewHigh),
            libva::VAProfile::VAProfileHEVCMain => Ok(Self::HevcMain),
            libva::VAProfile::VAProfileHEVCMain10 => Ok(Self::HevcMain10),
            libva::VAProfile::VAProfileVP8Version0_3 => Ok(Self::VP8Profile0),
            libva::VAProfile::VAProfileVP9Profile0 => Ok(Self::VP9Profile0),
            libva::VAProfile::VAProfileVP9Profile1 => Ok(Self::VP9Profile1),
            libva::VAProfile::VAProfileVP9Profile2 => Ok(Self::VP9Profile2),
            libva::VAProfile::VAProfileVP9Profile3 => Ok(Self::VP9Profile3),
            _ => Err(anyhow!(
                "Conversion failed for unexpected VAProfile: {}",
                value
            )),
        }
    }
}

/// The state for the output queue containing the buffers that will receive the
/// decoded data.
enum OutputQueueState {
    /// Waiting for the client to call `set_output_buffer_count`.
    AwaitingBufferCount,
    /// Codec is capable of decoding frames.
    Decoding,
    /// Dynamic Resolution Change - we can still accept buffers in the old
    /// format, but are waiting for new parameters before doing any decoding.
    Drc,
}

///A safe decoder abstraction over libva for a single vaContext
pub struct VaapiDecoder {
    /// The capabilities for the decoder
    caps: Capability,
}

// The VA capabilities for the coded side
struct CodedCap {
    profile: libva::VAProfile::Type,
    max_width: u32,
    max_height: u32,
}

// The VA capabilities for the raw side
struct RawCap {
    fourcc: u32,
    min_width: u32,
    min_height: u32,
    max_width: u32,
    max_height: u32,
}

impl VaapiDecoder {
    // Query the capabilities for the coded format
    fn get_coded_cap(
        display: &libva::Display,
        profile: libva::VAProfile::Type,
    ) -> Result<CodedCap> {
        let mut attrs = vec![
            libva::VAConfigAttrib {
                type_: libva::VAConfigAttribType::VAConfigAttribMaxPictureWidth,
                value: 0,
            },
            libva::VAConfigAttrib {
                type_: libva::VAConfigAttribType::VAConfigAttribMaxPictureHeight,
                value: 0,
            },
        ];

        display.get_config_attributes(profile, libva::VAEntrypoint::VAEntrypointVLD, &mut attrs)?;

        let mut max_width = 1u32;
        let mut max_height = 1u32;

        for attr in &attrs {
            if attr.value == libva::constants::VA_ATTRIB_NOT_SUPPORTED {
                continue;
            }

            match attr.type_ {
                libva::VAConfigAttribType::VAConfigAttribMaxPictureWidth => max_width = attr.value,
                libva::VAConfigAttribType::VAConfigAttribMaxPictureHeight => {
                    max_height = attr.value
                }

                _ => panic!("Unexpected VAConfigAttribType {}", attr.type_),
            }
        }

        Ok(CodedCap {
            profile,
            max_width,
            max_height,
        })
    }

    // Query the capabilities for the raw format
    fn get_raw_caps(display: Rc<libva::Display>, coded_cap: &CodedCap) -> Result<Vec<RawCap>> {
        let mut raw_caps = Vec::new();

        let mut config = display.create_config(
            vec![],
            coded_cap.profile,
            libva::VAEntrypoint::VAEntrypointVLD,
        )?;

        let fourccs = config.query_surface_attributes_by_type(
            libva::VASurfaceAttribType::VASurfaceAttribPixelFormat,
        )?;

        for fourcc in fourccs {
            let fourcc = match fourcc {
                libva::GenericValue::Integer(i) => i as u32,
                other => panic!("Unexpected VAGenericValue {:?}", other),
            };

            let min_width = config.query_surface_attributes_by_type(
                libva::VASurfaceAttribType::VASurfaceAttribMinWidth,
            )?;

            let min_width = match min_width.get(0) {
                Some(libva::GenericValue::Integer(i)) => *i as u32,
                Some(other) => panic!("Unexpected VAGenericValue {:?}", other),
                None => 1,
            };

            let min_height = config.query_surface_attributes_by_type(
                libva::VASurfaceAttribType::VASurfaceAttribMinHeight,
            )?;
            let min_height = match min_height.get(0) {
                Some(libva::GenericValue::Integer(i)) => *i as u32,
                Some(other) => panic!("Unexpected VAGenericValue {:?}", other),
                None => 1,
            };

            let max_width = config.query_surface_attributes_by_type(
                libva::VASurfaceAttribType::VASurfaceAttribMaxWidth,
            )?;
            let max_width = match max_width.get(0) {
                Some(libva::GenericValue::Integer(i)) => *i as u32,
                Some(other) => panic!("Unexpected VAGenericValue {:?}", other),
                None => coded_cap.max_width,
            };

            let max_height = config.query_surface_attributes_by_type(
                libva::VASurfaceAttribType::VASurfaceAttribMaxHeight,
            )?;
            let max_height = match max_height.get(0) {
                Some(libva::GenericValue::Integer(i)) => *i as u32,
                Some(other) => panic!("Unexpected VAGenericValue {:?}", other),
                None => coded_cap.max_height,
            };

            raw_caps.push(RawCap {
                fourcc,
                min_width,
                min_height,
                max_width,
                max_height,
            });
        }

        Ok(raw_caps)
    }

    /// Creates a new instance of the Vaapi decoder.
    pub fn new() -> Result<Self> {
        let display = libva::Display::open().ok_or_else(|| anyhow!("failed to open VA display"))?;

        let va_profiles = display.query_config_profiles()?;

        let mut in_fmts = Vec::new();
        let mut out_fmts = Vec::new();
        let mut profiles_map: BTreeMap<Format, Vec<Profile>> = Default::default();

        // VA has no API for querying the levels supported by the driver.
        // vaQueryProcessingRate is close, but not quite a solution here
        // for all codecs.
        let levels: BTreeMap<Format, Vec<Level>> = Default::default();

        for va_profile in va_profiles {
            let mut profiles = Vec::new();

            let entrypoints = display.query_config_entrypoints(va_profile)?;
            if !entrypoints
                .iter()
                .any(|e| *e == libva::VAEntrypoint::VAEntrypointVLD)
            {
                // All formats we are aiming to support require
                // VAEntrypointVLD.
                continue;
            }

            let profile = match Profile::try_from(va_profile) {
                Ok(p) => p,
                // Skip if we cannot convert to a valid virtio format
                Err(_) => continue,
            };

            // Manually push all VP8 profiles, since VA exposes only a single
            // VP8 profile for all of these
            if va_profile == libva::VAProfile::VAProfileVP8Version0_3 {
                profiles.push(Profile::VP8Profile0);
                profiles.push(Profile::VP8Profile1);
                profiles.push(Profile::VP8Profile2);
                profiles.push(Profile::VP8Profile3);
            } else {
                profiles.push(profile);
            }

            let coded_cap = VaapiDecoder::get_coded_cap(display.as_ref(), va_profile)?;
            let raw_caps = VaapiDecoder::get_raw_caps(Rc::clone(&display), &coded_cap)?;

            let coded_frame_fmt = FrameFormat {
                width: FormatRange {
                    min: 1,
                    max: coded_cap.max_width,
                    step: 1,
                },

                height: FormatRange {
                    min: 1,
                    max: coded_cap.max_height,
                    step: 1,
                },

                bitrates: Default::default(),
            };

            let coded_format = profile.to_format();
            match profiles_map.entry(coded_format) {
                Entry::Vacant(e) => {
                    e.insert(profiles);
                }
                Entry::Occupied(mut ps) => {
                    ps.get_mut().push(profile);
                }
            }

            let mut n_out = 0;
            for raw_cap in raw_caps {
                if raw_cap.fourcc != libva::constants::VA_FOURCC_NV12 {
                    // Apparently only NV12 is currently supported by virtio video
                    continue;
                }

                let raw_frame_fmt = FrameFormat {
                    width: FormatRange {
                        min: raw_cap.min_width,
                        max: raw_cap.max_width,
                        step: 1,
                    },

                    height: FormatRange {
                        min: raw_cap.min_height,
                        max: raw_cap.max_height,
                        step: 1,
                    },

                    bitrates: Default::default(),
                };

                out_fmts.push(FormatDesc {
                    mask: 0,
                    format: Format::NV12,
                    frame_formats: vec![raw_frame_fmt],
                    plane_align: 1,
                });

                n_out += 1;
            }

            let mask = !(u64::MAX << n_out) << (out_fmts.len() - n_out);

            if mask != 0 {
                in_fmts.push(FormatDesc {
                    mask,
                    format: coded_format,
                    frame_formats: vec![coded_frame_fmt],
                    plane_align: 1,
                });
            }
        }

        Ok(Self {
            caps: Capability::new(in_fmts, out_fmts, profiles_map, levels),
        })
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolution {
    width: u32,
    height: u32,
}

trait AsBufferHandle {
    type BufferHandle: BufferHandle;
    fn as_buffer_handle(&self) -> &Self::BufferHandle;
}

impl AsBufferHandle for GuestResource {
    type BufferHandle = GuestResourceHandle;

    fn as_buffer_handle(&self) -> &Self::BufferHandle {
        &self.handle
    }
}

impl AsBufferHandle for GuestMemHandle {
    type BufferHandle = Self;

    fn as_buffer_handle(&self) -> &Self::BufferHandle {
        self
    }
}

impl AsBufferHandle for GuestResourceHandle {
    type BufferHandle = Self;

    fn as_buffer_handle(&self) -> &Self::BufferHandle {
        self
    }
}

/// A convenience type implementing persistent slice access for BufferHandles.
struct BufferMapping<'a, T: AsBufferHandle> {
    #[allow(dead_code)]
    /// The underlying resource. Must be kept so as not to drop the BufferHandle
    resource: &'a T,
    /// The mapping that backs the underlying slices returned by AsRef and AsMut
    mapping: MemoryMappingArena,
}

impl<'a, T: AsBufferHandle> BufferMapping<'a, T> {
    /// Creates a new BufferMap
    pub fn new(resource: &'a T, offset: usize, size: usize) -> Result<Self> {
        let mapping = resource.as_buffer_handle().get_mapping(offset, size)?;

        Ok(Self { resource, mapping })
    }
}

impl<'a, T: AsBufferHandle> AsRef<[u8]> for BufferMapping<'a, T> {
    fn as_ref(&self) -> &[u8] {
        let mapping = &self.mapping;
        // SAFETY:
        // Safe because the mapping is linear and we own it, so it will not be unmapped during
        // the lifetime of this slice.
        unsafe { std::slice::from_raw_parts(mapping.as_ptr(), mapping.size()) }
    }
}

impl<'a, T: AsBufferHandle> AsMut<[u8]> for BufferMapping<'a, T> {
    fn as_mut(&mut self) -> &mut [u8] {
        let mapping = &self.mapping;
        // SAFETY:
        // Safe because the mapping is linear and we own it, so it will not be unmapped during
        // the lifetime of this slice.
        unsafe { std::slice::from_raw_parts_mut(mapping.as_ptr(), mapping.size()) }
    }
}

/// A frame that is currently not available for being decoded into, either because it has been
/// decoded and is waiting for us to release it (`Decoded`), or because we temporarily removed it
/// from the decoder pool after a reset and are waiting for the client to tell us we can use it
/// (`Held`).
enum BorrowedFrame {
    Decoded(Box<dyn DecodedHandle<Descriptor = BufferDescWithPicId>>),
    Held(Box<dyn AsRef<BufferDescWithPicId>>),
}

/// A decoder session for the libva backend
pub struct VaapiDecoderSession {
    /// The implementation for the codec specific logic.
    codec: Box<dyn StatelessVideoDecoder<BufferDescWithPicId>>,
    /// The state for the output queue. Updated when `set_output_buffer_count`
    /// is called or when we detect a dynamic resolution change.
    output_queue_state: OutputQueueState,
    /// Frames currently held by us, indexed by `picture_buffer_id`.
    held_frames: BTreeMap<i32, BorrowedFrame>,
    /// Queue containing the buffers we have not yet submitted to the codec.
    submit_queue: VecDeque<PendingJob>,
    /// The event queue we can use to signal new events.
    event_queue: EventQueue<DecoderEvent>,
    /// Whether the decoder is currently flushing.
    flushing: bool,
}

impl VaapiDecoderSession {
    /// Copy raw decoded data from `image` into the output buffer
    fn output_picture(
        decoded_frame: &dyn DecodedHandle<Descriptor = BufferDescWithPicId>,
        event_queue: &mut EventQueue<DecoderEvent>,
    ) -> Result<()> {
        let display_resolution = decoded_frame.display_resolution();
        let timestamp = decoded_frame.timestamp();

        let buffer_desc = decoded_frame.resource();
        let picture_buffer_id = buffer_desc.picture_buffer_id;

        // Sync the frame if it is in guest memory, as we are going to map and read it.
        // This statement is in its own block so we can drop the `buffer_desc` reference
        // before calling `sync`, which does a mutable borrow.
        if let BufferDescriptor::GuestMem(_) = &buffer_desc.desc {
            drop(buffer_desc);
            decoded_frame.sync()?;
        }

        // Copy guest memory buffers into their destination.
        if let BufferDescriptor::GuestMem(handle) = &decoded_frame.resource().desc {
            let picture = decoded_frame.dyn_picture();
            let mut backend_handle = picture.dyn_mappable_handle()?;
            let buffer_size = backend_handle.image_size();

            // Get a mapping from the start of the buffer to the size of the
            // underlying decoded data in the Image.
            let mut output_map = BufferMapping::new(&handle.0, 0, buffer_size)?;
            let output_bytes = output_map.as_mut();

            backend_handle.read(output_bytes)?;
        }

        // Say that we are done decoding this picture.
        event_queue
            .queue_event(DecoderEvent::PictureReady {
                picture_buffer_id,
                timestamp,
                visible_rect: Rect {
                    left: 0,
                    top: 0,
                    right: display_resolution.width as i32,
                    bottom: display_resolution.height as i32,
                },
            })
            .map_err(|e| {
                VideoError::BackendFailure(anyhow!("Can't queue the PictureReady event {}", e))
            })?;

        Ok(())
    }

    fn try_emit_flush_completed(&mut self) -> Result<()> {
        if self.submit_queue.is_empty() {
            self.flushing = false;

            let event_queue = &mut self.event_queue;

            event_queue
                .queue_event(DecoderEvent::FlushCompleted(Ok(())))
                .map_err(|e| anyhow!("Can't queue the PictureReady event {}", e))
        } else {
            Ok(())
        }
    }

    fn drain_submit_queue(&mut self) -> VideoResult<()> {
        while let Some(job) = self.submit_queue.front_mut() {
            let bitstream_map = BufferMapping::new(&job.resource, job.offset, job.bytes_used)
                .map_err(VideoError::BackendFailure)?;

            let slice_start = job.bytes_used - job.remaining;
            match self
                .codec
                .decode(job.timestamp, &bitstream_map.as_ref()[slice_start..])
            {
                Ok(processed) => {
                    job.remaining = job.remaining.saturating_sub(processed);
                    // We have completed the buffer.
                    if job.remaining == 0 {
                        // We are always done with the input buffer after decode returns.
                        self.event_queue
                            .queue_event(DecoderEvent::NotifyEndOfBitstreamBuffer(job.resource_id))
                            .map_err(|e| {
                                VideoError::BackendFailure(anyhow!(
                                    "Can't queue the NotifyEndOfBitstream event {}",
                                    e
                                ))
                            })?;
                        self.submit_queue.pop_front();
                    }
                }
                Err(DecodeError::CheckEvents) => {
                    self.process_decoder_events()?;
                    break;
                }
                // We will succeed once buffers are returned by the client. This could be optimized
                // to only retry decoding once buffers are effectively returned.
                Err(DecodeError::NotEnoughOutputBuffers(_)) => break,
                // TODO add an InvalidInput error to cros-codecs so we can detect these cases and
                // just throw a warning instead of a fatal error?
                Err(e) => {
                    self.event_queue
                        .queue_event(DecoderEvent::NotifyError(VideoError::BackendFailure(
                            anyhow!("Decoding buffer {} failed", job.resource_id),
                        )))
                        .map_err(|e| {
                            VideoError::BackendFailure(anyhow!(
                                "Can't queue the NotifyError event {}",
                                e
                            ))
                        })?;
                    return Err(VideoError::BackendFailure(e.into()));
                }
            }
        }

        Ok(())
    }

    fn process_decoder_events(&mut self) -> VideoResult<()> {
        while let Some(event) = self.codec.next_event() {
            match event {
                cros_codecs::decoder::DecoderEvent::FrameReady(frame) => {
                    Self::output_picture(frame.as_ref(), &mut self.event_queue)
                        .map_err(VideoError::BackendFailure)?;
                    let picture_id = frame.resource().picture_buffer_id;
                    self.held_frames
                        .insert(picture_id, BorrowedFrame::Decoded(frame));
                }
                cros_codecs::decoder::DecoderEvent::FormatChanged(mut format) => {
                    let coded_resolution = format.stream_info().coded_resolution;
                    let display_resolution = format.stream_info().display_resolution;

                    // Ask the client for new buffers.
                    self.event_queue
                        .queue_event(DecoderEvent::ProvidePictureBuffers {
                            min_num_buffers: format.stream_info().min_num_frames as u32,
                            width: coded_resolution.width as i32,
                            height: coded_resolution.height as i32,
                            visible_rect: Rect {
                                left: 0,
                                top: 0,
                                right: display_resolution.width as i32,
                                bottom: display_resolution.height as i32,
                            },
                        })
                        .map_err(|e| VideoError::BackendFailure(e.into()))?;

                    format.frame_pool().clear();

                    // Drop our output queue and wait for the new number of output buffers.
                    self.output_queue_state = match &self.output_queue_state {
                        // If this is part of the initialization step, then do not switch states.
                        OutputQueueState::AwaitingBufferCount => {
                            OutputQueueState::AwaitingBufferCount
                        }
                        OutputQueueState::Decoding => OutputQueueState::Drc,
                        OutputQueueState::Drc => {
                            return Err(VideoError::BackendFailure(anyhow!(
                                "Invalid state during DRC."
                            )))
                        }
                    };
                }
            }
        }

        Ok(())
    }

    fn try_make_progress(&mut self) -> VideoResult<()> {
        self.process_decoder_events()?;
        self.drain_submit_queue()?;

        Ok(())
    }
}

impl DecoderSession for VaapiDecoderSession {
    fn set_output_parameters(&mut self, _: usize, _: Format) -> VideoResult<()> {
        let output_queue_state = &mut self.output_queue_state;

        // This logic can still be improved, in particular it needs better
        // support at the virtio-video protocol level.
        //
        // We must ensure that set_output_parameters is only called after we are
        // sure that we have processed some stream metadata, which currently is
        // not the case. In particular, the {SET|GET}_PARAMS logic currently
        // takes place *before* we had a chance to parse any stream metadata at
        // all.
        //
        // This can lead to a situation where we accept a format (say, NV12),
        // but then discover we are unable to decode it after processing some
        // buffers (because the stream indicates that the bit depth is 10, for
        // example). Note that there is no way to reject said stream as of right
        // now unless we hardcode NV12 in cros-codecs itself.
        //
        // Nevertheless, the support is already in place in cros-codecs: the
        // decoders will queue buffers until they read some metadata. At this
        // point, it will allow for the negotiation of the decoded format until
        // a new call to decode() is made. At the crosvm level, we can use this
        // window of time to try different decoded formats with .try_format().
        //
        // For now, we accept the default format chosen by cros-codecs instead.
        // In practice, this means NV12 if it the stream can be decoded into
        // NV12 and if the hardware can do so.

        match output_queue_state {
            OutputQueueState::AwaitingBufferCount | OutputQueueState::Drc => {
                // Accept the default format chosen by cros-codecs instead.
                //
                // if let Some(backend_format) = self.backend.backend().format() {
                //     let backend_format = Format::try_from(backend_format);

                //     let format_matches = match backend_format {
                //         Ok(backend_format) => backend_format != format,
                //         Err(_) => false,
                //     };

                //     if !format_matches {
                //         let format =
                //             DecodedFormat::try_from(format).map_err(VideoError::BackendFailure)?;

                //         self.backend.backend().try_format(format).map_err(|e| {
                //             VideoError::BackendFailure(anyhow!(
                //                 "Failed to set the codec backend format: {}",
                //                 e
                //             ))
                //         })?;
                //     }
                // }

                *output_queue_state = OutputQueueState::Decoding;

                Ok(())
            }
            OutputQueueState::Decoding => {
                // Covers the slightly awkward ffmpeg v4l2 stateful
                // implementation for the capture queue setup.
                //
                // ffmpeg will queue a single OUTPUT buffer and immediately
                // follow up with a VIDIOC_G_FMT call on the CAPTURE queue.
                // This leads to a race condition, because it takes some
                // appreciable time for the real resolution to propagate back to
                // the guest as the virtio machinery processes and delivers the
                // event.
                //
                // In the event that VIDIOC_G_FMT(capture) returns the default
                // format, ffmpeg allocates buffers of the default resolution
                // (640x480) only to immediately reallocate as soon as it
                // processes the SRC_CH v4l2 event. Otherwise (if the resolution
                // has propagated in time), this path will not be taken during
                // the initialization.
                //
                // This leads to the following workflow in the virtio video
                // worker:
                // RESOURCE_QUEUE -> QUEUE_CLEAR -> RESOURCE_QUEUE
                //
                // Failing to accept this (as we previously did), leaves us
                // with bad state and completely breaks the decoding process. We
                // should replace the queue even if this is not 100% according
                // to spec.
                //
                // On the other hand, this branch still exists to highlight the
                // fact that we should assert that we have emitted a buffer with
                // the LAST flag when support for buffer flags is implemented in
                // a future CL. If a buffer with the LAST flag hasn't been
                // emitted, it's technically a mistake to be here because we
                // still have buffers of the old resolution to deliver.
                *output_queue_state = OutputQueueState::Decoding;

                // TODO: check whether we have emitted a buffer with the LAST
                // flag before returning.
                Ok(())
            }
        }
    }

    fn decode(
        &mut self,
        resource_id: u32,
        timestamp: u64,
        resource: GuestResourceHandle,
        offset: u32,
        bytes_used: u32,
    ) -> VideoResult<()> {
        let job = PendingJob {
            resource_id,
            timestamp,
            resource,
            offset: offset as usize,
            bytes_used: bytes_used as usize,
            remaining: bytes_used as usize,
        };

        self.submit_queue.push_back(job);
        self.try_make_progress()?;

        Ok(())
    }

    fn flush(&mut self) -> VideoResult<()> {
        self.flushing = true;

        self.try_make_progress()?;

        if !self.submit_queue.is_empty() {
            return Ok(());
        }

        // Retrieve ready frames from the codec, if any.
        self.codec
            .flush()
            .map_err(|e| VideoError::BackendFailure(e.into()))?;
        self.process_decoder_events()?;

        self.try_emit_flush_completed()
            .map_err(VideoError::BackendFailure)
    }

    fn reset(&mut self) -> VideoResult<()> {
        self.submit_queue.clear();

        // Make sure the codec is not active.
        self.codec
            .flush()
            .map_err(|e| VideoError::BackendFailure(e.into()))?;

        self.process_decoder_events()?;

        // Drop the queued output buffers.
        self.clear_output_buffers()?;

        self.event_queue
            .queue_event(DecoderEvent::ResetCompleted(Ok(())))
            .map_err(|e| {
                VideoError::BackendFailure(anyhow!("Can't queue the ResetCompleted event {}", e))
            })?;

        Ok(())
    }

    fn clear_output_buffers(&mut self) -> VideoResult<()> {
        // Cancel any ongoing flush.
        self.flushing = false;

        // Drop all decoded frames signaled as ready and cancel any reported flush.
        self.event_queue.retain(|event| {
            !matches!(
                event,
                DecoderEvent::PictureReady { .. } | DecoderEvent::FlushCompleted(_)
            )
        });

        // Now hold all the imported frames until reuse_output_buffer is called on them.
        let frame_pool = self.codec.frame_pool();
        while let Some(frame) = frame_pool.take_free_frame() {
            let picture_id = (*frame).as_ref().picture_buffer_id;
            self.held_frames
                .insert(picture_id, BorrowedFrame::Held(frame));
        }

        Ok(())
    }

    fn event_pipe(&self) -> &dyn base::AsRawDescriptor {
        &self.event_queue
    }

    fn use_output_buffer(
        &mut self,
        picture_buffer_id: i32,
        resource: GuestResource,
    ) -> VideoResult<()> {
        let output_queue_state = &mut self.output_queue_state;
        if let OutputQueueState::Drc = output_queue_state {
            // Reusing buffers during DRC is valid, but we won't use them and can just drop them.
            return Ok(());
        }

        let desc = match resource.handle {
            GuestResourceHandle::GuestPages(handle) => {
                BufferDescriptor::GuestMem(GuestMemDescriptor(handle))
            }
            GuestResourceHandle::VirtioObject(handle) => {
                // SAFETY: descriptor is expected to be valid
                let fd = unsafe { OwnedFd::from_raw_fd(handle.desc.into_raw_descriptor()) };
                let modifier = handle.modifier;

                let frame = DmabufFrame {
                    fds: vec![fd],
                    layout: FrameLayout {
                        format: (cros_codecs::Fourcc::from(b"NV12"), modifier),
                        size: cros_codecs::Resolution::from((resource.width, resource.height)),
                        planes: resource
                            .planes
                            .iter()
                            .map(|p| PlaneLayout {
                                buffer_index: 0,
                                offset: p.offset,
                                stride: p.stride,
                            })
                            .collect(),
                    },
                };

                BufferDescriptor::Dmabuf(frame)
            }
        };

        let desc_with_pic_id = BufferDescWithPicId {
            desc,
            picture_buffer_id,
        };

        self.codec
            .frame_pool()
            .add_frames(vec![desc_with_pic_id])
            .map_err(VideoError::BackendFailure)?;

        self.try_make_progress()
    }

    fn reuse_output_buffer(&mut self, picture_buffer_id: i32) -> VideoResult<()> {
        let output_queue_state = &mut self.output_queue_state;
        if let OutputQueueState::Drc = output_queue_state {
            // Reusing buffers during DRC is valid, but we won't use them and can just drop them.
            return Ok(());
        }

        self.held_frames.remove(&picture_buffer_id);

        self.try_make_progress()?;

        if self.flushing {
            // Try flushing again now that we have a new buffer. This might let
            // us progress further in the flush operation.
            self.flush()?;
        }
        Ok(())
    }

    fn read_event(&mut self) -> VideoResult<DecoderEvent> {
        self.event_queue
            .dequeue_event()
            .map_err(|e| VideoError::BackendFailure(anyhow!("Can't read event {}", e)))
    }
}

impl DecoderBackend for VaapiDecoder {
    type Session = VaapiDecoderSession;

    fn get_capabilities(&self) -> Capability {
        self.caps.clone()
    }

    fn new_session(&mut self, format: Format) -> VideoResult<Self::Session> {
        let display = Display::open()
            .ok_or_else(|| VideoError::BackendFailure(anyhow!("failed to open VA display")))?;

        let codec: Box<dyn StatelessVideoDecoder<BufferDescWithPicId>> = match format {
            Format::VP8 => Box::new(
                cros_codecs::decoder::stateless::StatelessDecoder::<Vp8, _>::new_vaapi(
                    display,
                    cros_codecs::decoder::BlockingMode::NonBlocking,
                ),
            ),
            Format::VP9 => Box::new(
                cros_codecs::decoder::stateless::StatelessDecoder::<Vp9, _>::new_vaapi(
                    display,
                    cros_codecs::decoder::BlockingMode::NonBlocking,
                ),
            ),
            Format::H264 => Box::new(
                cros_codecs::decoder::stateless::StatelessDecoder::<H264, _>::new_vaapi(
                    display,
                    cros_codecs::decoder::BlockingMode::NonBlocking,
                ),
            ),
            Format::Hevc => Box::new(
                cros_codecs::decoder::stateless::StatelessDecoder::<H265, _>::new_vaapi(
                    display,
                    cros_codecs::decoder::BlockingMode::NonBlocking,
                ),
            ),
            _ => return Err(VideoError::InvalidFormat),
        };

        Ok(VaapiDecoderSession {
            codec,
            output_queue_state: OutputQueueState::AwaitingBufferCount,
            held_frames: Default::default(),
            submit_queue: Default::default(),
            event_queue: EventQueue::new().map_err(|e| VideoError::BackendFailure(anyhow!(e)))?,
            flushing: Default::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    #[test]
    // Ignore this test by default as it requires libva-compatible hardware.
    #[ignore]
    fn test_get_capabilities() {
        let decoder = VaapiDecoder::new().unwrap();
        let caps = decoder.get_capabilities();
        assert!(!caps.input_formats().is_empty());
        assert!(!caps.output_formats().is_empty());
    }

    // Decode using guest memory input and output buffers.
    #[test]
    // Ignore this test by default as it requires libva-compatible hardware.
    #[ignore]
    fn test_decode_h264_guestmem_to_guestmem() {
        decode_h264_generic(
            &mut VaapiDecoder::new().unwrap(),
            build_guest_mem_handle,
            build_guest_mem_handle,
        );
    }
}
