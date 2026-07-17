//! Terminal emulator implementation using pure Winit, WGPU 30.0, and VTE

use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use glyphon::{
    Attrs, Buffer as GlyphonBuffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache,
    TextAtlas, TextRenderer, Weight,
};
use vte::{Parser, Perform};
use wgpu::util::DeviceExt;

// --- DOMAIN MODELS ---

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Option<[f32; 4]>,
    pub bg: Option<[f32; 4]>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub reverse: bool,
}

impl Cell {
    #[inline]
    pub fn blank() -> Self {
        Self {
            ch: ' ',
            fg: None,
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            strikethrough: false,
            reverse: false,
        }
    }
}

pub struct TerminalState {
    pub grid: Vec<Vec<Cell>>,
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub cols: usize,
    pub rows: usize,
    pub fg_color: [f32; 4],
    pub bg_color: [f32; 4],
    pub font_size: f32,
    pub char_width: f64,
    pub char_height: f64,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub pty_fd: Option<RawFd>,
    pub saved_cursor_x: usize,
    pub saved_cursor_y: usize,
    pub current_fg: [f32; 4],
    pub current_bg: [f32; 4],
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub reverse: bool,
    pub scrollback: Vec<Vec<Cell>>,
    pub scrollback_limit: usize,
    pub scroll_offset: usize,
    pub pty_master_fd: Option<RawFd>,
    pub alt_screen: Option<(Vec<Vec<Cell>>, usize, usize)>,
    pub cursor_visible: bool,
    pub pending_wrap: bool,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    pub dirty: bool,
}

impl TerminalState {
    pub fn new(cols: usize, rows: usize) -> Self {
        let mut grid = Vec::with_capacity(rows);
        for _ in 0..rows {
            grid.push(vec![Cell::blank(); cols]);
        }

        let fg = [0.9, 0.9, 0.9, 1.0];
        let bg = [0.05, 0.05, 0.05, 1.0];

        Self {
            grid,
            cursor_x: 0,
            cursor_y: 0,
            cols,
            rows,
            fg_color: fg,
            bg_color: bg,
            font_size: 16.0,
            char_width: 9.6,
            char_height: 20.0,
            pixel_width: 800,
            pixel_height: 600,
            pty_fd: None,
            saved_cursor_x: 0,
            saved_cursor_y: 0,
            current_fg: fg,
            current_bg: bg,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            strikethrough: false,
            reverse: false,
            scrollback: Vec::new(),
            scrollback_limit: 10000,
            scroll_offset: 0,
            pty_master_fd: None,
            alt_screen: None,
            cursor_visible: true,
            pending_wrap: false,
            bracketed_paste: false,
            focus_reporting: false,
            dirty: true,
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }

        self.dirty = true;
        let mut new_grid = Vec::with_capacity(rows);
        for y in 0..rows {
            let mut row = Vec::with_capacity(cols);
            for x in 0..cols {
                let cell = if y < self.rows && x < self.cols {
                    self.grid[y][x].clone()
                } else {
                    Cell::blank()
                };
                row.push(cell);
            }
            new_grid.push(row);
        }

        self.grid = new_grid;
        self.cols = cols;
        self.rows = rows;
        self.cursor_x = self.cursor_x.min(cols.saturating_sub(1));
        self.cursor_y = self.cursor_y.min(rows.saturating_sub(1));

        if let Some(fd) = self.pty_master_fd {
            let winsize = libc::winsize {
                ws_row: rows as u16,
                ws_col: cols as u16,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            unsafe {
                libc::ioctl(fd, libc::TIOCSWINSZ, &winsize);
            }
        }
    }

    pub fn scroll_up(&mut self) {
        if self.grid.is_empty() {
            return;
        }
        self.dirty = true;
        let top_row = self.grid.remove(0);
        self.scrollback.push(top_row);
        if self.scrollback.len() > self.scrollback_limit {
            self.scrollback.remove(0);
        }
        self.grid.push(vec![Cell::blank(); self.cols]);
    }

    pub fn clear(&mut self) {
        self.dirty = true;
        for row in self.grid.iter_mut() {
            for cell in row.iter_mut() {
                *cell = Cell::blank();
            }
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.current_fg = self.fg_color;
        self.current_bg = self.bg_color;
        self.bold = false;
        self.reverse = false;
    }

    pub fn write_pty(&self, data: &[u8]) {
        if let Some(fd) = self.pty_fd {
            unsafe {
                libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
            }
        }
    }

    pub fn send_cursor_position(&self) {
        let response = format!("\x1b[{};{}R", self.cursor_y + 1, self.cursor_x + 1);
        self.write_pty(response.as_bytes());
    }

    pub fn send_background_color(&self) {
        let [r, g, b, _] = self.bg_color;
        let response = format!(
            "\x1b]11;rgb:{:04x}/{:04x}/{:04x}\x1b\\",
            (r * 65535.0) as u16,
            (g * 65535.0) as u16,
            (b * 65535.0) as u16,
        );
        self.write_pty(response.as_bytes());
    }

    pub fn enter_alt_screen(&mut self) {
        if self.alt_screen.is_some() {
            return;
        }
        self.dirty = true;
        self.alt_screen = Some((self.grid.clone(), self.cursor_x, self.cursor_y));
        self.grid = (0..self.rows)
            .map(|_| vec![Cell::blank(); self.cols])
            .collect();
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    pub fn exit_alt_screen(&mut self) {
        if let Some((saved_grid, cx, cy)) = self.alt_screen.take() {
            self.dirty = true;
            self.grid = saved_grid;
            self.cursor_x = cx;
            self.cursor_y = cy;
        }
    }

    pub fn color_from_256(index: u16) -> [f32; 4] {
        const ANSI16: [[f32; 3]; 16] = [
            [0.0, 0.0, 0.0],
            [0.8, 0.0, 0.0],
            [0.0, 0.8, 0.0],
            [0.8, 0.8, 0.0],
            [0.0, 0.0, 0.8],
            [0.8, 0.0, 0.8],
            [0.0, 0.8, 0.8],
            [0.8, 0.8, 0.8],
            [0.4, 0.4, 0.4],
            [1.0, 0.2, 0.2],
            [0.2, 1.0, 0.2],
            [1.0, 1.0, 0.2],
            [0.2, 0.2, 1.0],
            [1.0, 0.2, 1.0],
            [0.2, 1.0, 1.0],
            [1.0, 1.0, 1.0],
        ];
        if index < 16 {
            let [r, g, b] = ANSI16[index as usize];
            return [r, g, b, 1.0];
        }
        if index < 232 {
            let i = index - 16;
            let b = (i % 6) as f32;
            let g = ((i / 6) % 6) as f32;
            let r = (i / 36) as f32;
            let scale = |v: f32| {
                if v == 0.0 {
                    0.0
                } else {
                    (55.0 + v * 40.0) / 255.0
                }
            };
            return [scale(r), scale(g), scale(b), 1.0];
        }
        let level = (8 + (index - 232) * 10) as f32 / 255.0;
        [level, level, level, 1.0]
    }

    // --- PARSER LOGIC EXPOSED AS METHODS ---

    pub fn perform_print(&mut self, c: char) {
        self.dirty = true;
        if self.pending_wrap {
            self.pending_wrap = false;
            self.cursor_x = 0;
            self.cursor_y += 1;
            if self.cursor_y >= self.rows {
                self.scroll_up();
                self.cursor_y = self.rows - 1;
            }
        }

        let y = self.cursor_y;
        let x = self.cursor_x;
        if x < self.cols && y < self.rows {
            let (fg, bg) = if self.reverse {
                (Some(self.current_bg), Some(self.current_fg))
            } else {
                (Some(self.current_fg), Some(self.current_bg))
            };
            self.grid[y][x] = Cell {
                ch: c,
                fg,
                bg,
                bold: self.bold,
                dim: self.dim,
                italic: self.italic,
                underline: self.underline,
                strikethrough: self.strikethrough,
                reverse: self.reverse,
            };
        }

        self.cursor_x += 1;
        if self.cursor_x >= self.cols {
            self.cursor_x = self.cols - 1;
            self.pending_wrap = true;
        }
    }

    pub fn perform_execute(&mut self, byte: u8) {
        self.dirty = true;
        match byte {
            b'\r' => {
                self.cursor_x = 0;
                self.pending_wrap = false;
            }
            b'\n' => {
                self.cursor_y += 1;
                if self.cursor_y >= self.rows {
                    self.scroll_up();
                    self.cursor_y = self.rows - 1;
                }
            }
            b'\t' => {
                self.cursor_x = ((self.cursor_x / 8) + 1) * 8;
                if self.cursor_x >= self.cols {
                    self.cursor_x = self.cols - 1;
                }
            }
            b'\x08' if self.cursor_x > 0 => {
                self.cursor_x -= 1;
            }
            _ => {}
        }
    }

    pub fn perform_csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        command: char,
    ) {
        self.dirty = true;
        let mut p: Vec<i64> = Vec::new();
        for param in params.iter() {
            if let Some(&val) = param.first() {
                p.push(val as i64);
            }
        }
        let has_question = intermediates.first().copied() == Some(b'?');
        let has_gt = intermediates.first().copied() == Some(b'>');
        self.pending_wrap = false;

        match command {
            'c' if !has_question => {
                let response: &[u8] = if has_gt {
                    b"\x1b[>0;0;0c"
                } else {
                    b"\x1b[?1;0c"
                };
                self.write_pty(response);
            }
            'c' => {}
            'A' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_y = self.cursor_y.saturating_sub(n);
            }
            'B' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_y = (self.cursor_y + n).min(self.rows.saturating_sub(1));
            }
            'C' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_x = (self.cursor_x + n).min(self.cols.saturating_sub(1));
            }
            'D' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_x = self.cursor_x.saturating_sub(n);
            }
            'E' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_y = (self.cursor_y + n).min(self.rows.saturating_sub(1));
                self.cursor_x = 0;
            }
            'F' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_y = self.cursor_y.saturating_sub(n);
                self.cursor_x = 0;
            }
            'G' => {
                let col = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_x = (col - 1).min(self.cols.saturating_sub(1));
            }
            'H' | 'f' => {
                let row = p.first().copied().unwrap_or(1).max(1) as usize;
                let col = p.get(1).copied().unwrap_or(1).max(1) as usize;
                self.cursor_y = (row - 1).min(self.rows.saturating_sub(1));
                self.cursor_x = (col - 1).min(self.cols.saturating_sub(1));
            }
            'J' => {
                let cy = self.cursor_y;
                let cx = self.cursor_x;
                let rows = self.rows;
                let cols = self.cols;
                match p.first().copied().unwrap_or(0) {
                    0 => {
                        for y in cy..rows {
                            for x in 0..cols {
                                if y == cy && x < cx {
                                    continue;
                                }
                                self.grid[y][x] = Cell::blank();
                            }
                        }
                    }
                    1 => {
                        for y in 0..=cy {
                            for x in 0..cols {
                                if y == cy && x > cx {
                                    continue;
                                }
                                self.grid[y][x] = Cell::blank();
                            }
                        }
                    }
                    2 | 3 => {
                        for y in 0..rows {
                            for x in 0..cols {
                                self.grid[y][x] = Cell::blank();
                            }
                        }
                        self.cursor_x = 0;
                        self.cursor_y = 0;
                    }
                    _ => {}
                }
            }
            'K' => {
                let row = self.cursor_y;
                let cx = self.cursor_x;
                let cols = self.cols;
                match p.first().copied().unwrap_or(0) {
                    0 => {
                        for x in cx..cols {
                            self.grid[row][x] = Cell::blank();
                        }
                    }
                    1 => {
                        for x in 0..=cx {
                            self.grid[row][x] = Cell::blank();
                        }
                    }
                    2 => {
                        for x in 0..cols {
                            self.grid[row][x] = Cell::blank();
                        }
                    }
                    _ => {}
                }
            }
            'L' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                let cy = self.cursor_y;
                let cols = self.cols;
                let rows = self.rows;
                for _ in 0..n {
                    if self.grid.len() > 0 {
                        self.grid.pop();
                    }
                    self.grid.insert(cy, vec![Cell::blank(); cols]);
                }
                self.grid.truncate(rows);
            }
            'M' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                let cy = self.cursor_y;
                let cols = self.cols;
                let rows = self.rows;
                for _ in 0..n {
                    if cy < self.grid.len() {
                        self.grid.remove(cy);
                        self.grid.push(vec![Cell::blank(); cols]);
                    }
                }
                self.grid.truncate(rows);
            }
            'P' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                let cy = self.cursor_y;
                let cx = self.cursor_x;
                let row = &mut self.grid[cy];
                for _ in 0..n {
                    if cx < row.len() {
                        row.remove(cx);
                        row.push(Cell::blank());
                    }
                }
            }
            '@' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                let cy = self.cursor_y;
                let cx = self.cursor_x;
                let cols = self.cols;
                let row = &mut self.grid[cy];
                for _ in 0..n {
                    if cx < row.len() {
                        row.insert(cx, Cell::blank());
                        row.truncate(cols);
                    }
                }
            }
            'S' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                for _ in 0..n {
                    self.scroll_up();
                }
            }
            'd' => {
                let row = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_y = (row - 1).min(self.rows.saturating_sub(1));
            }
            'm' => {
                if p.is_empty() {
                    self.current_fg = self.fg_color;
                    self.current_bg = self.bg_color;
                    self.bold = false;
                    self.dim = false;
                    self.italic = false;
                    self.underline = false;
                    self.strikethrough = false;
                    self.reverse = false;
                } else {
                    let mut i = 0;
                    while i < p.len() {
                        match p[i] {
                            0 => {
                                self.current_fg = self.fg_color;
                                self.current_bg = self.bg_color;
                                self.bold = false;
                                self.dim = false;
                                self.italic = false;
                                self.underline = false;
                                self.strikethrough = false;
                                self.reverse = false;
                            }
                            1 => self.bold = true,
                            2 => self.dim = true,
                            3 => self.italic = true,
                            4 => self.underline = true,
                            7 => self.reverse = true,
                            9 => self.strikethrough = true,
                            22 => {
                                self.bold = false;
                                self.dim = false;
                            }
                            23 => self.italic = false,
                            24 => self.underline = false,
                            27 => self.reverse = false,
                            29 => self.strikethrough = false,
                            30..=37 => self.current_fg = ansi_color((p[i] - 30) as u16, false),
                            38 => match p.get(i + 1).copied() {
                                Some(5) if p.len() > i + 2 => {
                                    self.current_fg =
                                        TerminalState::color_from_256(p[i + 2] as u16);
                                    i += 2;
                                }
                                Some(2) if p.len() > i + 4 => {
                                    self.current_fg = rgb_color(p[i + 2], p[i + 3], p[i + 4]);
                                    i += 4;
                                }
                                _ => {}
                            },
                            39 => self.current_fg = self.fg_color,
                            40..=47 => self.current_bg = ansi_color((p[i] - 40) as u16, false),
                            48 => match p.get(i + 1).copied() {
                                Some(5) if p.len() > i + 2 => {
                                    self.current_bg =
                                        TerminalState::color_from_256(p[i + 2] as u16);
                                    i += 2;
                                }
                                Some(2) if p.len() > i + 4 => {
                                    self.current_bg = rgb_color(p[i + 2], p[i + 3], p[i + 4]);
                                    i += 4;
                                }
                                _ => {}
                            },
                            49 => self.current_bg = self.bg_color,
                            90..=97 => self.current_fg = ansi_color((p[i] - 90) as u16, true),
                            100..=107 => self.current_bg = ansi_color((p[i] - 100) as u16, true),
                            _ => {}
                        }
                        i += 1;
                    }
                }
            }
            'n' if !has_question && p.first().copied().unwrap_or(0) == 6 => {
                self.send_cursor_position();
            }
            'h' | 'l' if has_question => {
                let enable = command == 'h';
                for &mode in &p {
                    match mode {
                        25 => self.cursor_visible = enable,
                        1004 => self.focus_reporting = enable,
                        1049 => {
                            if enable {
                                self.enter_alt_screen();
                            } else {
                                self.exit_alt_screen();
                            }
                        }
                        2004 => self.bracketed_paste = enable,
                        _ => {}
                    }
                }
            }
            's' => {
                self.saved_cursor_x = self.cursor_x;
                self.saved_cursor_y = self.cursor_y;
            }
            'u' => {
                self.cursor_x = self.saved_cursor_x;
                self.cursor_y = self.saved_cursor_y;
            }
            _ => {}
        }
    }

    pub fn perform_osc_dispatch(&mut self, params: &[&[u8]], _command: bool) {
        self.dirty = true;
        if params.is_empty() {
            return;
        }

        let cmd = std::str::from_utf8(params[0])
            .unwrap_or("")
            .parse::<u16>()
            .unwrap_or(u16::MAX);

        let payload = if params.len() > 1 {
            params[1..].concat()
        } else {
            Vec::new()
        };
        let payload_str = String::from_utf8_lossy(&payload);

        match cmd {
            0 | 1 => {}
            11 if payload_str == "?" => {
                self.send_background_color();
            }
            52 => {}
            133 => {}
            _ => {}
        }
    }
}

// --- VTE PARSER HANDLERS ---

pub struct TerminalHandlerDirect<'a> {
    pub state: &'a mut TerminalState,
}

impl<'a> Perform for TerminalHandlerDirect<'a> {
    fn print(&mut self, c: char) {
        self.state.perform_print(c);
    }

    fn execute(&mut self, byte: u8) {
        self.state.perform_execute(byte);
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        command: char,
    ) {
        self.state
            .perform_csi_dispatch(params, intermediates, ignore, command);
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], command: bool) {
        self.state.perform_osc_dispatch(params, command);
    }
}

// --- WGPU SHADER & PIPELINE ---

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
}

const BG_SHADER: &str = "
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
};
struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};
@vertex
fn vs_main(model: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(model.position, 0.0, 1.0);
    out.color = model.color;
    return out;
}
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
";

// --- WINIT APPLICATION ---

struct AppState {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: glyphon::Viewport,
    text_atlas: TextAtlas,
    text_renderer: TextRenderer,

    bg_pipeline: wgpu::RenderPipeline,
    glyphon_buffer: GlyphonBuffer,
    bg_vertex_buffer: wgpu::Buffer,
    bg_vertex_capacity: usize,
    bg_vertex_count: u32,
}

pub struct TerminalApp {
    state: Arc<Mutex<TerminalState>>,
    app_state: Option<AppState>,
}

impl ApplicationHandler<()> for TerminalApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.app_state.is_some() {
            return;
        }

        let window_attrs = Window::default_attributes()
            .with_title("fluxvt")
            .with_inner_size(winit::dpi::PhysicalSize::new(800, 600));
        let window = Arc::new(event_loop.create_window(window_attrs).unwrap());

        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).unwrap();

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .unwrap();

        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();

        let size = window.inner_size();
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync, // Prevents swapchain blocking latency
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Srgb,
        };
        surface.configure(&device, &config);

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = glyphon::Cache::new(&device);
        let viewport = glyphon::Viewport::new(&device, &cache);
        let mut text_atlas = TextAtlas::new(&device, &queue, &cache, surface_format);
        let text_renderer = TextRenderer::new(
            &mut text_atlas,
            &device,
            wgpu::MultisampleState::default(),
            None,
        );

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Background Shader"),
            source: wgpu::ShaderSource::Wgsl(BG_SHADER.into()),
        });

        let bg_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[],
            immediate_size: 0,
        });

        let bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Background Pipeline"),
            layout: Some(&bg_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        let font_size = 16.0;
        let char_height = 20.0;
        let glyphon_buffer =
            GlyphonBuffer::new(&mut font_system, Metrics::new(font_size, char_height));

        let initial_bg_capacity = 80 * 24 * 6; // Standard 80x24 background block
        let bg_vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("BG Vertex Buffer"),
            size: (initial_bg_capacity * std::mem::size_of::<Vertex>()) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        self.spawn_shell(window.clone());

        self.app_state = Some(AppState {
            window,
            surface,
            device,
            queue,
            config,
            font_system,
            swash_cache,
            viewport,
            text_atlas,
            text_renderer,
            bg_pipeline,
            glyphon_buffer,
            bg_vertex_buffer,
            bg_vertex_capacity: initial_bg_capacity,
            bg_vertex_count: 0,
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let app = match &mut self.app_state {
            Some(app) => app,
            None => return,
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                app.config.width = size.width.max(1);
                app.config.height = size.height.max(1);
                app.surface.configure(&app.device, &app.config);

                let mut s = self.state.lock().unwrap();
                s.pixel_width = size.width;
                s.pixel_height = size.height;
                let cols = (size.width as f64 / s.char_width).floor() as usize;
                let rows = (size.height as f64 / s.char_height).floor() as usize;
                s.resize(cols.max(1), rows.max(1));
                app.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let mut bytes = Vec::new();
                    match event.logical_key {
                        Key::Named(NamedKey::Enter) => bytes.extend_from_slice(b"\r"),
                        Key::Named(NamedKey::Backspace) => bytes.extend_from_slice(b"\x7f"),
                        Key::Named(NamedKey::Escape) => bytes.extend_from_slice(b"\x1b"),
                        Key::Named(NamedKey::Tab) => bytes.extend_from_slice(b"\t"),
                        Key::Named(NamedKey::Space) => bytes.extend_from_slice(b" "),
                        Key::Named(NamedKey::ArrowUp) => bytes.extend_from_slice(b"\x1b[A"),
                        Key::Named(NamedKey::ArrowDown) => bytes.extend_from_slice(b"\x1b[B"),
                        Key::Named(NamedKey::ArrowRight) => bytes.extend_from_slice(b"\x1b[C"),
                        Key::Named(NamedKey::ArrowLeft) => bytes.extend_from_slice(b"\x1b[D"),
                        Key::Character(s) => {
                            bytes.extend_from_slice(s.as_bytes());
                        }
                        _ => {}
                    }
                    if !bytes.is_empty() {
                        self.state.lock().unwrap().write_pty(&bytes);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.render();
            }
            _ => {}
        }
    }
}

impl TerminalApp {
    fn spawn_shell(&self, window: Arc<Window>) {
        let cols = 80;
        let rows = 24;

        let (master_fd, slave_fd): (RawFd, RawFd) = unsafe {
            let mut master: RawFd = -1;
            let mut slave: RawFd = -1;
            let winsize = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &winsize,
            );
            (master, slave)
        };

        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            libc::tcgetattr(master_fd, &mut termios);
            termios.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
            libc::tcsetattr(master_fd, libc::TCSANOW, &termios);
        }

        {
            let mut state = self.state.lock().unwrap();
            state.pty_master_fd = Some(master_fd);
            state.pty_fd = Some(master_fd);
            state.cols = cols as usize;
            state.rows = rows as usize;
        }

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut command = Command::new(&shell);
        command.env("TERM", "xterm-256color");

        unsafe {
            command
                .stdin(Stdio::from_raw_fd(slave_fd))
                .stdout(Stdio::from_raw_fd(libc::dup(slave_fd)))
                .stderr(Stdio::from_raw_fd(libc::dup(slave_fd)))
                .pre_exec(move || {
                    libc::setsid();
                    libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                    Ok(())
                });
        }

        command.spawn().unwrap();

        let state_clone = self.state.clone();

        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut parser = Parser::new();
            loop {
                let n = unsafe {
                    libc::read(master_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n <= 0 {
                    break;
                }

                {
                    let mut state = state_clone.lock().unwrap();
                    let mut handler = TerminalHandlerDirect { state: &mut *state };

                    for &byte in &buf[..n as usize] {
                        parser.advance(&mut handler, byte);
                    }
                }

                // Call request_redraw directly-bypassing the slow CustomEvent channel
                window.request_redraw();
            }
        });
    }

    fn render(&mut self) {
        let app = match &mut self.app_state {
            Some(app) => app,
            None => return,
        };

        let output = match app.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(tex)
            | wgpu::CurrentSurfaceTexture::Suboptimal(tex) => tex,
            _ => return,
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut needs_update = false;
        let mut bg_color = [0.0; 4];
        let mut font_size_f32 = 0.0;
        let mut char_height_f32 = 0.0;
        let mut spans_data: Vec<(String, Attrs)> = Vec::new();
        let mut bg_vertices = Vec::new();

        // Safely extract all geometry data in a single continuous lock,
        // cleanly preventing race conditions without double-locking.
        {
            let mut s = self.state.lock().unwrap();
            bg_color = s.bg_color;

            if s.dirty {
                needs_update = true;
                s.dirty = false;

                font_size_f32 = s.font_size;
                char_height_f32 = s.char_height as f32;

                let default_fg = s.fg_color;
                let char_width = s.char_width;
                let char_height = s.char_height;
                let cursor_x = s.cursor_x;
                let cursor_y = s.cursor_y;
                let cursor_visible = s.cursor_visible;
                let tex_width = s.pixel_width;
                let tex_height = s.pixel_height;

                for (row_idx, row) in s.grid.iter().enumerate() {
                    let mut current_str = String::new();
                    let mut current_attrs: Option<Attrs> = None;

                    for (col_idx, cell) in row.iter().enumerate() {
                        let effective_bg = if cell.reverse {
                            cell.fg.unwrap_or(default_fg)
                        } else {
                            cell.bg.unwrap_or(bg_color)
                        };

                        if effective_bg != bg_color {
                            let px0 = col_idx as f32 * char_width as f32;
                            let py0 = row_idx as f32 * char_height as f32;
                            let px1 = px0 + char_width as f32;
                            let py1 = py0 + char_height as f32;

                            let nx0 = (px0 / tex_width as f32) * 2.0 - 1.0;
                            let ny0 = 1.0 - (py0 / tex_height as f32) * 2.0;
                            let nx1 = (px1 / tex_width as f32) * 2.0 - 1.0;
                            let ny1 = 1.0 - (py1 / tex_height as f32) * 2.0;

                            bg_vertices.push(Vertex {
                                position: [nx0, ny0],
                                color: effective_bg,
                            });
                            bg_vertices.push(Vertex {
                                position: [nx1, ny0],
                                color: effective_bg,
                            });
                            bg_vertices.push(Vertex {
                                position: [nx0, ny1],
                                color: effective_bg,
                            });
                            bg_vertices.push(Vertex {
                                position: [nx1, ny0],
                                color: effective_bg,
                            });
                            bg_vertices.push(Vertex {
                                position: [nx1, ny1],
                                color: effective_bg,
                            });
                            bg_vertices.push(Vertex {
                                position: [nx0, ny1],
                                color: effective_bg,
                            });
                        }

                        let effective_fg = if cell.reverse {
                            cell.bg.unwrap_or(bg_color)
                        } else {
                            cell.fg.unwrap_or(default_fg)
                        };
                        let fg_c = Color::rgba(
                            (effective_fg[0] * 255.0) as u8,
                            (effective_fg[1] * 255.0) as u8,
                            (effective_fg[2] * 255.0) as u8,
                            (effective_fg[3] * 255.0) as u8,
                        );

                        let attrs = Attrs::new()
                            .family(Family::Monospace)
                            .weight(if cell.bold {
                                Weight::BOLD
                            } else {
                                Weight::NORMAL
                            })
                            .color(fg_c);

                        if current_attrs.as_ref() == Some(&attrs) {
                            current_str.push(cell.ch);
                        } else {
                            if !current_str.is_empty() {
                                spans_data
                                    .push((current_str.clone(), current_attrs.take().unwrap()));
                            }
                            current_str = String::from(cell.ch);
                            current_attrs = Some(attrs);
                        }
                    }
                    if !current_str.is_empty() {
                        spans_data.push((current_str, current_attrs.take().unwrap()));
                    }
                    spans_data.push(("\n".to_string(), Attrs::new()));
                }

                if cursor_visible {
                    let px0 = cursor_x as f32 * char_width as f32;
                    let py0 = cursor_y as f32 * char_height as f32;
                    let nx0 = (px0 / tex_width as f32) * 2.0 - 1.0;
                    let ny0 = 1.0 - (py0 / tex_height as f32) * 2.0;
                    let nx1 = ((px0 + char_width as f32) / tex_width as f32) * 2.0 - 1.0;
                    let ny1 = 1.0 - ((py0 + char_height as f32) / tex_height as f32) * 2.0;
                    let color = [1.0, 1.0, 1.0, 0.5];

                    bg_vertices.push(Vertex {
                        position: [nx0, ny0],
                        color,
                    });
                    bg_vertices.push(Vertex {
                        position: [nx1, ny0],
                        color,
                    });
                    bg_vertices.push(Vertex {
                        position: [nx0, ny1],
                        color,
                    });
                    bg_vertices.push(Vertex {
                        position: [nx1, ny0],
                        color,
                    });
                    bg_vertices.push(Vertex {
                        position: [nx1, ny1],
                        color,
                    });
                    bg_vertices.push(Vertex {
                        position: [nx0, ny1],
                        color,
                    });
                }
            }
        }

        if needs_update {
            app.bg_vertex_count = bg_vertices.len() as u32;
            if bg_vertices.len() > app.bg_vertex_capacity {
                app.bg_vertex_capacity = bg_vertices.len().next_power_of_two();
                app.bg_vertex_buffer = app.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("BG Vertex Buffer"),
                    size: (app.bg_vertex_capacity * std::mem::size_of::<Vertex>())
                        as wgpu::BufferAddress,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }

            if !bg_vertices.is_empty() {
                app.queue.write_buffer(
                    &app.bg_vertex_buffer,
                    0,
                    bytemuck::cast_slice(&bg_vertices),
                );
            }

            let new_metrics = Metrics::new(font_size_f32, char_height_f32);
            if app.glyphon_buffer.metrics() != new_metrics {
                app.glyphon_buffer.set_metrics(new_metrics);
            }

            let default_attrs = Attrs::new().family(Family::Monospace);
            app.glyphon_buffer.set_rich_text(
                spans_data.iter().map(|(s, a)| (s.as_str(), a.clone())),
                &default_attrs,
                Shaping::Basic, // Drastically lowers CPU overhead for grids
                None,
            );

            app.glyphon_buffer
                .shape_until_scroll(&mut app.font_system, false);
        }

        app.viewport.update(
            &app.queue,
            glyphon::Resolution {
                width: app.config.width,
                height: app.config.height,
            },
        );

        app.text_renderer
            .prepare(
                &app.device,
                &app.queue,
                &mut app.font_system,
                &mut app.text_atlas,
                &app.viewport,
                [glyphon::TextArea {
                    buffer: &app.glyphon_buffer,
                    left: 0.0,
                    top: 0.0,
                    scale: 1.0,
                    bounds: glyphon::TextBounds {
                        left: 0,
                        top: 0,
                        right: app.config.width as i32,
                        bottom: app.config.height as i32,
                    },
                    default_color: Color::rgb(255, 255, 255),
                    custom_glyphs: &[],
                }],
                &mut app.swash_cache,
            )
            .unwrap();

        let mut encoder = app
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Terminal Encoder"),
            });

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Terminal Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg_color[0] as f64,
                            g: bg_color[1] as f64,
                            b: bg_color[2] as f64,
                            a: bg_color[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if app.bg_vertex_count > 0 {
                rpass.set_pipeline(&app.bg_pipeline);
                rpass.set_vertex_buffer(0, app.bg_vertex_buffer.slice(..));
                rpass.draw(0..app.bg_vertex_count, 0..1);
            }

            app.text_renderer
                .render(&app.text_atlas, &app.viewport, &mut rpass)
                .unwrap();
        }

        app.queue.submit(Some(encoder.finish()));
        app.queue.present(output);
    }
}

pub fn run() {
    let event_loop = EventLoop::new().unwrap();
    let state = Arc::new(Mutex::new(TerminalState::new(80, 24)));

    let mut app = TerminalApp {
        state,
        app_state: None,
    };

    event_loop.run_app(&mut app).unwrap();
}

/// Converts a 24-bit RGB triple (0–255 each, passed as i64) to `[f32, 4]` RGBA.
#[inline]
fn rgb_color(r: i64, g: i64, b: i64) -> [f32; 4] {
    [
        (r as f32 / 255.0).clamp(0.0, 1.0),
        (g as f32 / 255.0).clamp(0.0, 1.0),
        (b as f32 / 255.0).clamp(0.0, 1.0),
        1.0,
    ]
}

#[inline]
fn ansi_color(index: u16, bright: bool) -> [f32; 4] {
    const NORMAL: [[f32; 4]; 8] = [
        [0.0, 0.0, 0.0, 1.0],
        [0.8, 0.0, 0.0, 1.0],
        [0.0, 0.8, 0.0, 1.0],
        [0.8, 0.8, 0.0, 1.0],
        [0.0, 0.0, 0.8, 1.0],
        [0.8, 0.0, 0.8, 1.0],
        [0.0, 0.8, 0.8, 1.0],
        [0.8, 0.8, 0.8, 1.0],
    ];
    const BRIGHT: [[f32; 4]; 8] = [
        [0.4, 0.4, 0.4, 1.0],
        [1.0, 0.2, 0.2, 1.0],
        [0.2, 1.0, 0.2, 1.0],
        [1.0, 1.0, 0.2, 1.0],
        [0.2, 0.2, 1.0, 1.0],
        [1.0, 0.2, 1.0, 1.0],
        [0.2, 1.0, 1.0, 1.0],
        [1.0, 1.0, 1.0, 1.0],
    ];
    let table = if bright { &BRIGHT } else { &NORMAL };
    table[(index as usize).min(7)]
}
