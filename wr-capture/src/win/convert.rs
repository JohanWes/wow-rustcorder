// SPDX-License-Identifier: GPL-3.0-or-later

//! BGRA frame to NV12 encoder input, scaled and letterboxed into the fixed
//! output size. The D3D11 video processor does it on the GPU; systems without
//! one (virtual machines with a display-only adapter) convert on the CPU.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0,
    D3D11_VIDEO_COLOR_RGBA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use super::fail;

/// Encoder input textures in flight. The encoders run in low-latency mode
/// without B-frames, so none holds more than a couple of frames.
const OUTPUTS: usize = 8;
const BLACK_SIZE: u32 = 16;

/// A converted frame: a GPU texture, or NV12 bytes in system memory.
pub enum Frame {
    Texture(ID3D11Texture2D),
    Memory(Vec<u8>),
}

pub enum Converter {
    Gpu(GpuConverter),
    Cpu(CpuConverter),
}

impl Converter {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, String> {
        match GpuConverter::new(device, context, width, height, fps) {
            Ok(gpu) => Ok(Self::Gpu(gpu)),
            Err(message) => {
                eprintln!("{message}; converting frames on the CPU");
                Ok(Self::Cpu(CpuConverter {
                    device: device.clone(),
                    context: context.clone(),
                    width,
                    height,
                    staging: None,
                }))
            }
        }
    }

    /// Convert `source` (texture, width, height); `None` gives black.
    pub fn convert(
        &mut self,
        source: Option<&(ID3D11Texture2D, u32, u32)>,
    ) -> Result<Frame, String> {
        match self {
            Self::Gpu(gpu) => gpu.convert(source).map(Frame::Texture),
            Self::Cpu(cpu) => cpu.convert(source).map(Frame::Memory),
        }
    }
}

pub struct GpuConverter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    width: u32,
    height: u32,
    black: ID3D11Texture2D,
    /// Input view for the last source texture; sources change only on resize.
    input: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
}

impl GpuConverter {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, String> {
        let video_device: ID3D11VideoDevice =
            device.cast().map_err(fail("query the video device"))?;
        let video_context: ID3D11VideoContext =
            context.cast().map_err(fail("query the video context"))?;
        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc) }
            .map_err(fail("create the video processor enumerator"))?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(fail("create the video processor"))?;
        unsafe {
            // Input: full-range RGB. Output: limited-range BT.709 YCbCr.
            video_context.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 },
            );
            video_context.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: 0x4 | 0x10,
                },
            );
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
        }
        let mut outputs = Vec::with_capacity(OUTPUTS);
        for _ in 0..OUTPUTS {
            let texture = texture(device, width, height, DXGI_FORMAT_NV12.0 as u32, None)?;
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut view = None;
            unsafe {
                video_device.CreateVideoProcessorOutputView(
                    &texture,
                    &enumerator,
                    &view_desc,
                    Some(&mut view),
                )
            }
            .map_err(fail("create a video processor output view"))?;
            outputs.push((texture, view.ok_or("no output view")?));
        }
        let zeros = vec![0u8; (BLACK_SIZE * BLACK_SIZE * 4) as usize];
        let black = texture(
            device,
            BLACK_SIZE,
            BLACK_SIZE,
            DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32,
            Some(&zeros),
        )?;
        Ok(Self {
            video_device,
            video_context,
            enumerator,
            processor,
            outputs,
            next: 0,
            width,
            height,
            black,
            input: None,
        })
    }

    /// Scale `source` (texture, width, height) into the next NV12 texture;
    /// `None` produces a black frame.
    fn convert(
        &mut self,
        source: Option<&(ID3D11Texture2D, u32, u32)>,
    ) -> Result<ID3D11Texture2D, String> {
        let (source, width, height) = match source {
            Some((texture, width, height)) => (texture, *width, *height),
            None => (&self.black, BLACK_SIZE, BLACK_SIZE),
        };
        if self
            .input
            .as_ref()
            .is_none_or(|(texture, _)| texture != source)
        {
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut view = None;
            unsafe {
                self.video_device.CreateVideoProcessorInputView(
                    source,
                    &self.enumerator,
                    &desc,
                    Some(&mut view),
                )
            }
            .map_err(fail("create a video processor input view"))?;
            self.input = Some((source.clone(), view.ok_or("no input view")?));
        }
        let (_, input) = self.input.as_ref().expect("set above");
        let (output, output_view) = &self.outputs[self.next];
        self.next = (self.next + 1) % OUTPUTS;

        let source_rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let dest_rect = fit(width, height, self.width, self.height);
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(input.clone())),
            ..Default::default()
        };
        let result = unsafe {
            self.video_context.VideoProcessorSetStreamSourceRect(
                &self.processor,
                0,
                true,
                Some(&source_rect),
            );
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&dest_rect),
            );
            self.video_context.VideoProcessorBlt(
                &self.processor,
                output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.map_err(fail("convert the frame"))?;
        Ok(output.clone())
    }
}

/// The largest centered rectangle of the source's aspect ratio inside the
/// output.
fn fit(width: u32, height: u32, out_width: u32, out_height: u32) -> RECT {
    let (width, height, out_width, out_height) = (
        width as i64,
        height as i64,
        out_width as i64,
        out_height as i64,
    );
    let (fit_width, fit_height) = if width * out_height > height * out_width {
        (out_width, (height * out_width / width) & !1)
    } else {
        ((width * out_height / height) & !1, out_height)
    };
    let left = (out_width - fit_width) / 2;
    let top = (out_height - fit_height) / 2;
    RECT {
        left: left as i32,
        top: top as i32,
        right: (left + fit_width) as i32,
        bottom: (top + fit_height) as i32,
    }
}

fn texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: u32,
    data: Option<&[u8]>,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(format as i32),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    let initial = data.map(|data| D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr().cast(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    });
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(
            &desc,
            initial.as_ref().map(|data| data as *const _),
            Some(&mut texture),
        )
    }
    .map_err(fail("create a texture"))?;
    texture.ok_or_else(|| "no texture".into())
}

pub struct CpuConverter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

impl CpuConverter {
    /// Read the frame back and convert it to limited-range BT.709 NV12 with
    /// nearest-neighbour scaling.
    fn convert(&mut self, source: Option<&(ID3D11Texture2D, u32, u32)>) -> Result<Vec<u8>, String> {
        let (out_width, out_height) = (self.width as usize, self.height as usize);
        let mut nv12 = vec![16u8; out_width * out_height];
        nv12.resize(out_width * out_height * 3 / 2, 128);
        let Some((texture, width, height)) = source else {
            return Ok(nv12);
        };
        let (width, height) = (*width, *height);
        if self
            .staging
            .as_ref()
            .is_none_or(|(_, w, h)| (*w, *h) != (width, height))
        {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            };
            let mut staging = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
                .map_err(fail("create a staging texture"))?;
            self.staging = Some((staging.ok_or("no staging texture")?, width, height));
        }
        let (staging, _, _) = self.staging.as_ref().expect("set above");
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context.CopyResource(staging, texture);
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(fail("read the frame back"))?;
        }
        let pitch = mapped.RowPitch as usize;
        let pixels = unsafe {
            std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * height as usize)
        };
        let rect = fit(width, height, self.width, self.height);
        let (left, top) = (rect.left as usize, rect.top as usize);
        let (fit_width, fit_height) = (
            (rect.right - rect.left) as usize,
            (rect.bottom - rect.top) as usize,
        );
        let (luma, chroma) = nv12.split_at_mut(out_width * out_height);
        for y in 0..fit_height {
            let row = &pixels[(y * height as usize / fit_height) * pitch..];
            for x in 0..fit_width {
                let pixel = &row[(x * width as usize / fit_width) * 4..];
                let (b, g, r) = (
                    i32::from(pixel[0]),
                    i32::from(pixel[1]),
                    i32::from(pixel[2]),
                );
                let (out_x, out_y) = (left + x, top + y);
                luma[out_y * out_width + out_x] = (16 + ((47 * r + 157 * g + 16 * b) >> 8)) as u8;
                if out_x % 2 == 0 && out_y % 2 == 0 {
                    let at = (out_y / 2) * out_width + out_x;
                    chroma[at] = (128 + ((-26 * r - 87 * g + 112 * b) >> 8)) as u8;
                    chroma[at + 1] = (128 + ((112 * r - 102 * g - 10 * b) >> 8)) as u8;
                }
            }
        }
        unsafe { self.context.Unmap(staging, 0) };
        Ok(nv12)
    }
}
