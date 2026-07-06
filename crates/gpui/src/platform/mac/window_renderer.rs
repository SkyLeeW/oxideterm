use super::renderer;
use crate::{
    DevicePixels, GpuSpecs, PlatformAtlas, PlatformRenderer, Scene, Size, SoftwareRenderer, size,
};
use anyhow::Result;
use cocoa::base::{id, nil};
use core_graphics::{
    color_space::CGColorSpace,
    context::CGContext,
    image::{CGImageAlphaInfo, CGImageByteOrderInfo},
};
use foreign_types::ForeignType;
use objc::{class, msg_send, runtime::Object, sel, sel_impl};
use std::{env, ffi::c_void, sync::Arc};

const SOFTWARE_RENDERER_ENV: &str = "OXIDETERM_RENDERER";
const SOFTWARE_RENDERER_VALUE: &str = "software";

pub(crate) enum MacWindowRenderer {
    Gpu(renderer::Renderer),
    Software(MacSoftwareRenderer),
}

pub(crate) struct MacSoftwareRenderer {
    layer: id,
    renderer: SoftwareRenderer,
    present_buffer: Vec<u8>,
    present_context: Option<CGContext>,
}

impl MacWindowRenderer {
    pub(crate) unsafe fn new(
        context: renderer::Context,
        native_window: *mut c_void,
        native_view: *mut c_void,
        bounds: Size<f32>,
        transparent: bool,
    ) -> Self {
        if software_renderer_forced() {
            Self::Software(MacSoftwareRenderer::new(bounds))
        } else {
            Self::Gpu(unsafe {
                renderer::new_renderer(context, native_window, native_view, bounds, transparent)
            })
        }
    }

    pub(crate) fn layer_ptr(&self) -> *mut Object {
        match self {
            Self::Gpu(renderer) => renderer.layer_ptr() as *mut Object,
            Self::Software(renderer) => renderer.layer,
        }
    }

    pub(crate) fn layer(&self) -> id {
        match self {
            Self::Gpu(renderer) => renderer.layer_ptr() as id,
            Self::Software(renderer) => renderer.layer,
        }
    }

    pub(crate) fn is_software(&self) -> bool {
        matches!(self, Self::Software(_))
    }

    pub(crate) fn update_transparency(&mut self, transparent: bool) {
        match self {
            Self::Gpu(renderer) => renderer.update_transparency(transparent),
            Self::Software(_) => {}
        }
    }

    pub(crate) fn destroy(&mut self) {
        match self {
            Self::Gpu(renderer) => renderer.destroy(),
            Self::Software(renderer) => renderer.destroy(),
        }
    }

    #[cfg(not(feature = "macos-blade"))]
    pub(crate) fn set_presents_with_transaction(&mut self, presents_with_transaction: bool) {
        if let Self::Gpu(renderer) = self {
            renderer.set_presents_with_transaction(presents_with_transaction);
        }
    }
}

impl PlatformRenderer for MacWindowRenderer {
    fn resize(&mut self, size: Size<DevicePixels>) -> Result<()> {
        match self {
            Self::Gpu(renderer) => PlatformRenderer::resize(renderer, size),
            Self::Software(renderer) => renderer.resize(size),
        }
    }

    fn draw(&mut self, scene: &Scene) -> Result<()> {
        match self {
            Self::Gpu(renderer) => PlatformRenderer::draw(renderer, scene),
            Self::Software(renderer) => renderer.draw(scene),
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        match self {
            Self::Gpu(renderer) => PlatformRenderer::sprite_atlas(renderer),
            Self::Software(renderer) => PlatformRenderer::sprite_atlas(&renderer.renderer),
        }
    }

    fn gpu_specs(&self) -> Result<Option<GpuSpecs>> {
        match self {
            Self::Gpu(renderer) => PlatformRenderer::gpu_specs(renderer),
            Self::Software(renderer) => PlatformRenderer::gpu_specs(&renderer.renderer),
        }
    }
}

impl MacSoftwareRenderer {
    fn new(bounds: Size<f32>) -> Self {
        let layer: id = unsafe { msg_send![class!(CALayer), new] };
        let initial_size = size(
            DevicePixels(bounds.width.ceil() as i32),
            DevicePixels(bounds.height.ceil() as i32),
        );
        Self {
            layer,
            renderer: SoftwareRenderer::new(initial_size),
            present_buffer: Vec::new(),
            present_context: None,
        }
    }

    fn resize(&mut self, size: Size<DevicePixels>) -> Result<()> {
        if self.renderer.size() != size {
            PlatformRenderer::resize(&mut self.renderer, size)?;
            self.rebuild_present_context();
        }
        Ok(())
    }

    fn draw(&mut self, scene: &Scene) -> Result<()> {
        PlatformRenderer::draw(&mut self.renderer, scene)?;
        self.present();
        Ok(())
    }

    fn present(&mut self) {
        let size = self.renderer.size();
        let width = size.width.0.max(0) as usize;
        let height = size.height.0.max(0) as usize;
        if width == 0 || height == 0 {
            return;
        }

        if self.present_context.is_none() || self.present_buffer.len() != width * height * 4 {
            self.rebuild_present_context();
        }

        if self.present_context.is_none()
            || self.present_buffer.len() != self.renderer.framebuffer().len()
        {
            return;
        }

        self.present_buffer
            .copy_from_slice(self.renderer.framebuffer());

        let Some(image) = self
            .present_context
            .as_ref()
            .and_then(|context| context.create_image())
        else {
            return;
        };
        unsafe {
            let _: () = msg_send![self.layer, setContents:image.as_ptr() as id];
        }
    }

    fn rebuild_present_context(&mut self) {
        self.present_context = None;

        let size = self.renderer.size();
        let width = size.width.0.max(0) as usize;
        let height = size.height.0.max(0) as usize;
        if width == 0 || height == 0 {
            self.present_buffer.clear();
            return;
        }

        let bytes_per_row = width * 4;
        self.present_buffer.resize(bytes_per_row * height, 0);
        let color_space = CGColorSpace::create_device_rgb();
        let bitmap_info = CGImageAlphaInfo::CGImageAlphaPremultipliedLast as u32
            | CGImageByteOrderInfo::CGImageByteOrder32Big as u32;

        // The bitmap context borrows the reusable presentation buffer. Rebuild it
        // only after resize so steady-state frames avoid allocating a new buffer.
        self.present_context = Some(CGContext::create_bitmap_context(
            Some(self.present_buffer.as_mut_ptr().cast()),
            width,
            height,
            8,
            bytes_per_row,
            &color_space,
            bitmap_info,
        ));
    }

    fn destroy(&mut self) {
        unsafe {
            let _: () = msg_send![self.layer, release];
        }
        self.layer = nil;
    }
}

fn software_renderer_forced() -> bool {
    env::var(SOFTWARE_RENDERER_ENV)
        .map(|value| value.eq_ignore_ascii_case(SOFTWARE_RENDERER_VALUE))
        .unwrap_or(false)
}
