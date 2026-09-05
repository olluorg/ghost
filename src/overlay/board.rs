//! Табло: окно, которое только показывает.
//!
//! Отдельным окном оно стало не ради порядка, а потому что иначе нельзя иметь
//! одновременно настоящую прозрачность и сквозные клики. Разбор такой:
//!
//! * сквозной хит-тест система даёт только слоёному окну (`WS_EX_LAYERED`);
//! * альфу слоя, заданную `SetLayeredWindowAttributes`, она применяет к
//!   поверхности, мимо которой рисует видеокарта, — окно выходит сплошным;
//! * значит картинку с альфой надо отдавать самим (`UpdateLayeredWindow`);
//! * а этого нельзя сделать в окне, куда уже показывает eframe: его показ
//!   перекрывает нашу картинку, и окно снова чёрное.
//!
//! Отсюда и разделение: табло — своё окно, куда рисуем только мы, и оно
//! никогда не принимает мышь. Всё, с чем работают руками, живёт в окне eframe.
//!
//! Рисуется табло теми же средствами, что и остальной интерфейс: свой
//! `egui::Context`, отрисовка в текстуру на той же видеокарте, что у eframe,
//! затем чтение кадра в память и передача системе.

use eframe::egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};
use eframe::wgpu;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassW, SetWindowPos, ShowWindow, HWND_TOPMOST,
    SWP_NOACTIVATE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use super::layered::Layered;

pub struct Board {
    hwnd: HWND,
    ctx: egui::Context,
    renderer: Renderer,
    target: Option<Target>,
    layered: Layered,
    start: std::time::Instant,
}

/// Текстура, в которую рисуем, и буфер, через который забираем кадр.
struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    /// Ширина строки в буфере: wgpu требует кратности 256 байтам.
    stride: u32,
}

unsafe extern "system" fn board_proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // Значок в трее шлёт сообщения сюда: у табло единственная своя оконная
    // процедура, а окно eframe принадлежит winit.
    if super::tray::handle(h, msg, w, l) {
        return LRESULT(0);
    }
    DefWindowProcW(h, msg, w, l)
}

impl Board {
    pub fn new(device: &wgpu::Device) -> Option<Self> {
        let hwnd = unsafe { create_window()? };
        // Формат кадра — тот, в котором его ждёт GDI: восемь бит на канал,
        // без гамма-коррекции на выходе, иначе цвета уедут.
        let renderer = Renderer::new(
            device,
            wgpu::TextureFormat::Rgba8Unorm,
            RendererOptions { msaa_samples: 1, ..Default::default() },
        );
        Some(Self {
            hwnd,
            ctx: egui::Context::default(),
            renderer,
            target: None,
            layered: Layered::default(),
            start: std::time::Instant::now(),
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Ставит табло туда же, где стоит окно управления.
    pub fn place(&self, x: i32, y: i32, w: i32, h: i32) {
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
        }
    }

    pub fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
    }

    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// Строит кадр и показывает его.
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        pixels_per_point: f32,
        build: impl FnMut(&mut egui::Ui),
    ) {
        if width == 0 || height == 0 {
            return;
        }
        self.ensure_target(device, width, height);
        let Some(target) = &self.target else { return };

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width as f32 / pixels_per_point, height as f32 / pixels_per_point),
            )),
            time: Some(self.start.elapsed().as_secs_f64()),
            ..Default::default()
        };
        self.ctx.set_pixels_per_point(pixels_per_point);
        let mut build = build;
        let output = self.ctx.run_ui(input, |ui| build(ui));
        let primitives = self.ctx.tessellate(output.shapes, pixels_per_point);

        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                self.renderer.update_texture(device, queue, *id, delta);
            }
        }

        let desc = ScreenDescriptor { size_in_pixels: [width, height], pixels_per_point };
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("табло") });
        let extra = self.renderer.update_buffers(device, queue, &mut encoder, &primitives, &desc);

        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("табло"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Прозрачный фон: всё, что не нарисовано, остаётся
                        // дырой, сквозь которую видно экран.
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.renderer.render(&mut pass.forget_lifetime(), &primitives, &desc);
        }

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &target.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(target.stride),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );

        queue.submit(extra.into_iter().chain(std::iter::once(encoder.finish())));

        for id in &output.textures_delta.free {
            self.renderer.free_texture(id);
        }

        // Забираем кадр в память. Ожидание здесь осознанное: показать окно
        // всё равно нечем, пока кадр не готов.
        let slice = target.buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        if rx.recv().map(|r| r.is_err()).unwrap_or(true) {
            return;
        }

        let Ok(data) = slice.get_mapped_range() else {
            target.buffer.unmap();
            return;
        };
        let stride = target.stride as usize;
        let row = width as usize * 4;
        let mut pixels = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            pixels.extend_from_slice(&data[y * stride..y * stride + row]);
        }
        drop(data);
        target.buffer.unmap();

        self.layered.present(self.hwnd, &pixels, width as usize, height as usize);
    }

    fn ensure_target(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        if self.target.as_ref().is_some_and(|t| t.width == width && t.height == height) {
            return;
        }
        let stride = (width * 4).div_ceil(256) * 256;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("табло"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("табло"),
            size: (stride * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.target = Some(Target { texture, view, buffer, width, height, stride });
    }
}

/// Окно табло: сквозное навсегда, вне Alt+Tab, поверх всех.
unsafe fn create_window() -> Option<HWND> {
    let hmod = GetModuleHandleW(None).ok()?;
    let class = windows::core::w!("ghost_board");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(board_proc),
        hInstance: hmod.into(),
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc);

    let h = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class,
        windows::core::w!("ghost board"),
        WS_POPUP,
        0,
        0,
        100,
        100,
        None,
        None,
        Some(hmod.into()),
        None,
    )
    .ok()?;
    let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
    let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
    Some(h)
}
