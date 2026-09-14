use std::{
    cell::{Cell, RefCell},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use ashpd::{
    desktop::{screenshot::Screenshot, ResponseError},
    Error as PortalError,
};
use chrono::Local;
use gtk::{gdk, gio, glib, prelude::*};
use image::{DynamicImage, Rgba, RgbaImage};
use serde::Deserialize;

const APP_ID: &str = "io.github.stan.ScreenInk";
const CAPTURE_ARGUMENT: &str = "--capture";
const DAEMON_ARGUMENT: &str = "--daemon";
const OPEN_ARGUMENT: &str = "--open";
const PEN_WIDTH: f64 = 5.0;
// X11 keycode for the physical key in the Z position (evdev KEY_Z + 8).
// Keycodes are not affected by switching XKB language groups.
const X11_KEYCODE_Z: u32 = 52;

static LOG_FILE: OnceLock<Mutex<fs::File>> = OnceLock::new();

#[derive(Default)]
struct SessionState {
    busy: Cell<bool>,
    has_clipboard: Cell<bool>,
    clipboard_owner: RefCell<Option<gtk::ApplicationWindow>>,
}

impl SessionState {
    fn begin_capture(&self) -> bool {
        !self.busy.replace(true)
    }

    fn finish_capture(&self) {
        self.busy.set(false);
    }

    fn retain_clipboard(&self, window: &gtk::ApplicationWindow) {
        self.busy.set(false);
        self.has_clipboard.set(true);
        let previous = self.clipboard_owner.borrow_mut().replace(window.clone());
        window.hide();
        if let Some(previous) = previous {
            if previous != *window {
                previous.close();
            }
        }
    }

    fn release_window(&self, window: &gtk::ApplicationWindow) {
        let is_owner = self
            .clipboard_owner
            .borrow()
            .as_ref()
            .is_some_and(|owner| owner == window);
        if is_owner {
            self.clipboard_owner.borrow_mut().take();
            self.has_clipboard.set(false);
        }
        self.busy.set(false);
    }
}

fn application_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn log_path() -> PathBuf {
    application_dir().join("screenink.log")
}

/// Each newly launched ScreenInk instance starts a fresh diagnostic log.
fn initialize_log() {
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .truncate(true)
        .open(log_path());
    if let Ok(file) = file {
        let _ = LOG_FILE.set(Mutex::new(file));
        log_event("ScreenInk started");
    }
}

fn log_event(message: impl AsRef<str>) {
    let line = format!(
        "{} {}\n",
        Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        message.as_ref()
    );
    if LOG_FILE.get().is_none() {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path());
        if let Ok(file) = file {
            let _ = LOG_FILE.set(Mutex::new(file));
        }
    }
    if let Some(file) = LOG_FILE.get() {
        if let Ok(mut file) = file.lock() {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    output_dir: Option<PathBuf>,
    portal_timeout_seconds: Option<u64>,
}

#[derive(Debug)]
struct Config {
    output_dir: PathBuf,
    portal_timeout_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output_dir: dirs::picture_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Screenshots"),
            portal_timeout_seconds: 60,
        }
    }
}

fn config_path() -> PathBuf {
    application_dir().join("config.toml")
}

fn load_config() -> Config {
    let path = config_path();
    let file_config: FileConfig = fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default();
    let config = Config {
        output_dir: file_config
            .output_dir
            .unwrap_or_else(|| Config::default().output_dir),
        portal_timeout_seconds: file_config
            .portal_timeout_seconds
            .unwrap_or_else(|| Config::default().portal_timeout_seconds),
    };
    log_event(format!(
        "config: output_dir={}, portal_timeout_seconds={}",
        config.output_dir.display(),
        config.portal_timeout_seconds
    ));
    config
}

/// Ask the desktop portal for a screenshot. On GNOME this opens the same
/// system selector that the built-in screenshot UI uses, then returns a local
/// PNG path. Unlike the old X11 overlay it lets GNOME handle monitor layout
/// and HiDPI coordinates itself.
enum CaptureError {
    Portal(PortalError),
    Timeout(u64),
}

/// This runs in a new process for every Print press. GNOME associates an
/// interactive Portal request with its caller process; forwarding a second
/// activation to an already hidden GTK application causes GNOME Shell to
/// ignore the request in this session.
fn capture_with_portal(portal_timeout_seconds: u64) -> Result<Option<PathBuf>, String> {
    log_event("portal request started");
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("Could not start the portal runtime: {error}"))?;
    let response = runtime.block_on(async {
        let request = async {
            Screenshot::request()
                .interactive(true)
                .modal(true)
                .send()
                .await?
                .response()
        };
        if portal_timeout_seconds == 0 {
            request.await.map_err(CaptureError::Portal)
        } else {
            tokio::time::timeout(Duration::from_secs(portal_timeout_seconds), request)
                .await
                .map_err(|_| CaptureError::Timeout(portal_timeout_seconds))?
                .map_err(CaptureError::Portal)
        }
    });

    match response {
        Ok(screenshot) => {
            let path = gio::File::for_uri(screenshot.uri().as_str())
                .path()
                .map(Some)
                .ok_or_else(|| "The screenshot portal returned a non-local file".to_owned());
            match &path {
                Ok(Some(path)) => log_event(format!("portal returned image: {}", path.display())),
                Err(error) => log_event(format!("portal returned invalid image URI: {error}")),
                Ok(None) => log_event("portal returned no image"),
            }
            path
        }
        Err(CaptureError::Portal(PortalError::Response(ResponseError::Cancelled))) => {
            log_event("portal selection cancelled");
            Ok(None)
        }
        Err(CaptureError::Timeout(seconds)) => {
            log_event(format!("portal timed out after {seconds} seconds"));
            Err(format!(
                "The desktop screenshot portal timed out after {seconds} seconds"
            ))
        }
        Err(CaptureError::Portal(error)) => {
            log_event(format!("portal request failed: {error}"));
            Err(format!("The desktop screenshot portal failed: {error}"))
        }
    }
}

fn portal_preview_scale() -> f64 {
    gdk::Display::default()
        .and_then(|display| display.monitors().item(0))
        .and_then(|monitor| monitor.downcast::<gdk::Monitor>().ok())
        .map(|monitor| f64::from(monitor.scale_factor().max(1)))
        .unwrap_or(1.0)
}

#[cfg(any())]
fn temporary_capture_path() -> PathBuf {
    std::env::temp_dir().join(format!("screenink-source-{}.png", std::process::id()))
}

#[cfg(any())]
fn capture_desktop() -> Result<PathBuf, String> {
    let path = temporary_capture_path();
    let (connection, screen_index) =
        x11rb::connect(None).map_err(|error| format!("Could not connect to X11: {error}"))?;
    let screen = &connection.setup().roots[screen_index];
    let root = screen.root;
    let width = screen.width_in_pixels;
    let height = screen.height_in_pixels;
    let visual_id = screen.root_visual;
    let visual = connection
        .setup()
        .roots
        .iter()
        .flat_map(|root| root.allowed_depths.iter())
        .flat_map(|depth| depth.visuals.iter())
        .find(|visual| visual.visual_id == visual_id)
        .ok_or_else(|| "Could not find the X11 root visual".to_owned())?;
    let reply = connection
        .get_image(ImageFormat::Z_PIXMAP, root, 0, 0, width, height, u32::MAX)
        .map_err(|error| format!("Could not request the X11 screen image: {error}"))?
        .reply()
        .map_err(|error| format!("Could not receive the X11 screen image: {error}"))?;
    let bits_per_pixel = connection
        .setup()
        .pixmap_formats
        .iter()
        .find(|format| format.depth == reply.depth)
        .map(|format| format.bits_per_pixel)
        .ok_or_else(|| "Could not determine the X11 pixel format".to_owned())?;
    let bytes_per_pixel = usize::from(bits_per_pixel).div_ceil(8);
    let stride = reply.data.len() / usize::from(height);
    let mut image = RgbaImage::new(u32::from(width), u32::from(height));
    for y in 0..usize::from(height) {
        for x in 0..usize::from(width) {
            let offset = y * stride + x * bytes_per_pixel;
            let pixel = reply.data[offset..offset + bytes_per_pixel]
                .iter()
                .enumerate()
                .fold(0_u32, |value, (byte_index, byte)| {
                    value | (u32::from(*byte) << (byte_index * 8))
                });
            image.put_pixel(
                x as u32,
                y as u32,
                Rgba([
                    x11_channel(pixel, visual.red_mask),
                    x11_channel(pixel, visual.green_mask),
                    x11_channel(pixel, visual.blue_mask),
                    255,
                ]),
            );
        }
    }
    image
        .save(&path)
        .map_err(|error| format!("Could not save the X11 screen image: {error}"))?;
    Ok(path)
}

#[cfg(any())]
fn x11_channel(pixel: u32, mask: u32) -> u8 {
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    (((pixel & mask) >> shift) * 255 / maximum) as u8
}

#[cfg(any())]
fn draw_crosshair(context: &gtk::cairo::Context, state: &SelectionState) {
    context.set_source_rgba(0.0, 0.0, 0.0, 0.35);
    context.paint().expect("paint overlay");

    if let (Some(start), Some(current)) = (state.start, state.current) {
        let (x, y) = (start.0.min(current.0), start.1.min(current.1));
        let (width, height) = ((start.0 - current.0).abs(), (start.1 - current.1).abs());
        context.set_operator(gtk::cairo::Operator::Clear);
        context.rectangle(x, y, width, height);
        context.fill().expect("clear selected area");
        context.set_operator(gtk::cairo::Operator::Over);
        context.set_source_rgb(0.95, 0.2, 0.2);
        context.set_line_width(2.0);
        context.rectangle(x + 1.0, y + 1.0, width - 2.0, height - 2.0);
        context.stroke().expect("draw selection border");
    }
}

#[cfg(any())]
#[derive(Default)]
struct SelectionState {
    start: Option<(f64, f64)>,
    current: Option<(f64, f64)>,
}

#[cfg(any())]
#[derive(Clone, Copy, Debug)]
struct MonitorGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[cfg(any())]
impl MonitorGeometry {
    fn fallback(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Tool {
    #[default]
    Pen,
    Rectangle,
}

enum Annotation {
    Freehand {
        points: Vec<(f64, f64)>,
        style: Brush,
    },
    Rectangle {
        start: (f64, f64),
        end: (f64, f64),
        style: Brush,
    },
}

#[derive(Clone, Copy)]
struct Brush {
    red: f64,
    green: f64,
    blue: f64,
    width: f64,
}

impl Default for Brush {
    fn default() -> Self {
        Self {
            red: 0.9,
            green: 0.02,
            blue: 0.02,
            width: PEN_WIDTH,
        }
    }
}

#[cfg(any())]
fn begin_selection(app: &gtk::Application, source_path: PathBuf, config: Rc<Config>) {
    let source = match image::open(&source_path) {
        Ok(source) => source,
        Err(error) => {
            let window = gtk::ApplicationWindow::builder().application(app).build();
            show_error(&window, &format!("Could not open the screenshot: {error}"));
            window.present();
            return;
        }
    };
    let monitors = connected_monitors(source.width(), source.height());
    let use_monitor_fullscreen = monitors.len() > 1;
    let windows = Rc::new(RefCell::new(Vec::<gtk::ApplicationWindow>::new()));
    let finished = Rc::new(Cell::new(false));

    for (index, monitor) in monitors.into_iter().enumerate() {
        begin_monitor_selection(
            app,
            &source,
            source_path.clone(),
            config.clone(),
            monitor,
            index,
            use_monitor_fullscreen.then(|| gdk_monitor_at(index)),
            windows.clone(),
            finished.clone(),
        );
    }
}

#[cfg(any())]
fn begin_monitor_selection(
    app: &gtk::Application,
    source: &DynamicImage,
    source_path: PathBuf,
    config: Rc<Config>,
    monitor: MonitorGeometry,
    index: usize,
    fullscreen_monitor: Option<Option<gdk::Monitor>>,
    all_windows: Rc<RefCell<Vec<gtk::ApplicationWindow>>>,
    finished: Rc<Cell<bool>>,
) {
    let title = if fullscreen_monitor.is_some() {
        format!("ScreenInk Selection {}-{}", std::process::id(), index)
    } else {
        "ScreenInk — Select an area".to_owned()
    };
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(&title)
        .decorated(false)
        .default_width(monitor.width as i32)
        .default_height(monitor.height as i32)
        .build();
    window.set_resizable(false);
    window.set_cursor_from_name(Some("crosshair"));
    let monitor_image = crop_monitor_image(source, monitor);
    let texture = texture_from_image(&monitor_image);
    let picture = gtk::Picture::for_paintable(&texture);
    picture.set_hexpand(true);
    picture.set_vexpand(true);

    let canvas = gtk::DrawingArea::new();
    canvas.set_hexpand(true);
    canvas.set_vexpand(true);
    canvas.set_focusable(true);
    canvas.set_cursor_from_name(Some("crosshair"));
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&canvas);
    let cancel_button = gtk::Button::with_label("Cancel (Esc)");
    cancel_button.add_css_class("destructive-action");
    cancel_button.set_halign(gtk::Align::End);
    cancel_button.set_valign(gtk::Align::Start);
    cancel_button.set_margin_top(16);
    cancel_button.set_margin_end(16);
    cancel_button.set_tooltip_text(Some("Cancel the screenshot selection"));
    overlay.add_overlay(&cancel_button);
    window.set_child(Some(&overlay));

    let state = Rc::new(RefCell::new(SelectionState::default()));
    let draw_state = state.clone();
    canvas.set_draw_func(move |_, context, _, _| {
        draw_crosshair(context, &draw_state.borrow());
    });

    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    let begin_state = state.clone();
    let begin_canvas = canvas.downgrade();
    drag.connect_drag_begin(move |_, x, y| {
        let mut selection = begin_state.borrow_mut();
        selection.start = Some((x, y));
        selection.current = Some((x, y));
        if let Some(canvas) = begin_canvas.upgrade() {
            canvas.queue_draw();
        }
    });
    let update_state = state.clone();
    let update_canvas = canvas.downgrade();
    drag.connect_drag_update(move |_, offset_x, offset_y| {
        let start = update_state.borrow().start;
        if let Some((start_x, start_y)) = start {
            update_state.borrow_mut().current = Some((start_x + offset_x, start_y + offset_y));
            if let Some(canvas) = update_canvas.upgrade() {
                canvas.queue_draw();
            }
        }
    });
    let end_state = state.clone();
    let end_canvas = canvas.downgrade();
    let end_window = window.downgrade();
    let end_app = app.downgrade();
    let end_source = source_path;
    let end_config = config.clone();
    let end_monitor = monitor;
    let end_windows = all_windows.clone();
    let end_finished = finished.clone();
    drag.connect_drag_end(move |_, offset_x, offset_y| {
        if end_finished.get() {
            return;
        }
        let Some(canvas) = end_canvas.upgrade() else {
            return;
        };
        let Some(window) = end_window.upgrade() else {
            return;
        };
        let Some(app) = end_app.upgrade() else {
            return;
        };
        let selection = end_state.borrow();
        let Some((start_x, start_y)) = selection.start else {
            return;
        };
        let end = (start_x + offset_x, start_y + offset_y);
        let width = (start_x - end.0).abs();
        let height = (start_y - end.1).abs();
        if width < 3.0 || height < 3.0 {
            return;
        }
        drop(selection);

        let source = match image::open(&end_source) {
            Ok(source) => source,
            Err(error) => {
                show_error(&window, &format!("Could not open the screenshot: {error}"));
                return;
            }
        };
        let start = map_canvas_point(
            &canvas,
            start_x,
            start_y,
            end_monitor.width as f64,
            end_monitor.height as f64,
        );
        let end = map_canvas_point(
            &canvas,
            end.0,
            end.1,
            end_monitor.width as f64,
            end_monitor.height as f64,
        );
        let x = end_monitor.x as f64 + start.0.min(end.0);
        let y = end_monitor.y as f64 + start.1.min(end.1);
        let width = (start.0 - end.0).abs();
        let height = (start.1 - end.1).abs();
        let x = x.clamp(0.0, source.width() as f64);
        let y = y.clamp(0.0, source.height() as f64);
        let width = width.min(source.width() as f64 - x);
        let height = height.min(source.height() as f64 - y);
        if width < 3.0 || height < 3.0 {
            return;
        }
        let crop = source.crop_imm(
            x.max(0.0) as u32,
            y.max(0.0) as u32,
            width as u32,
            height as u32,
        );
        end_finished.set(true);
        for window in end_windows.borrow().iter() {
            window.close();
        }
        // GTK reads this scale directly from the active GNOME monitor. On X11,
        // a fractional scale of 175% usually maps to a backend scale of 2.
        let preview_scale = (canvas.scale_factor() as f64).max(1.0);
        begin_editor(&app, crop, end_config.clone(), preview_scale, preview_scale);
    });
    canvas.add_controller(drag);

    let secondary_click = gtk::GestureClick::new();
    secondary_click.set_button(3);
    let secondary_windows = all_windows.clone();
    let secondary_finished = finished.clone();
    secondary_click.connect_released(move |_, _, _, _| {
        if !secondary_finished.replace(true) {
            for window in secondary_windows.borrow().iter() {
                window.close();
            }
        }
    });
    canvas.add_controller(secondary_click);

    let keys = gtk::EventControllerKey::new();
    let key_window = window.downgrade();
    let key_windows = all_windows.clone();
    let key_finished = finished.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            if !key_finished.replace(true) {
                for window in key_windows.borrow().iter() {
                    window.close();
                }
            } else if let Some(window) = key_window.upgrade() {
                window.close();
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
    let cancel_windows = all_windows.clone();
    let cancel_finished = finished.clone();
    cancel_button.connect_clicked(move |_| {
        if !cancel_finished.replace(true) {
            for window in cancel_windows.borrow().iter() {
                window.close();
            }
        }
    });
    all_windows.borrow_mut().push(window.clone());
    if let Some(Some(fullscreen_monitor)) = fullscreen_monitor {
        window.fullscreen_on_monitor(&fullscreen_monitor);
        window.present();
        canvas.grab_focus();
    } else if fullscreen_monitor.is_some() {
        // This fallback should not normally be needed, but still gives the
        // user a cancellable fullscreen overlay if GDK cannot enumerate a
        // monitor that XRandR reported.
        window.fullscreen();
        window.present();
        canvas.grab_focus();
    } else {
        // The native GTK fullscreen path was already proven reliable on a
        // single display. It also avoids an unnecessary focus-sensitive
        // external window positioning while there is only one monitor.
        window.fullscreen();
        window.present();
        canvas.grab_focus();
        let timeout_windows = all_windows.clone();
        let timeout_finished = finished.clone();
        let timeout_state = state.clone();
        glib::timeout_add_local(Duration::from_secs(45), move || {
            if !timeout_finished.get() && timeout_state.borrow().start.is_none() {
                timeout_finished.set(true);
                for window in timeout_windows.borrow().iter() {
                    window.close();
                }
            }
            glib::ControlFlow::Break
        });
    }
}

fn begin_editor(
    app: &gtk::Application,
    image: DynamicImage,
    config: Rc<Config>,
    source_scale_x: f64,
    source_scale_y: f64,
    lifetime: Rc<RefCell<Option<gio::ApplicationHoldGuard>>>,
    state: Rc<SessionState>,
) {
    let image = image.to_rgba8();
    let (image_width, image_height) = image.dimensions();
    let preview_width = ((image_width as f64 / source_scale_x).round() as i32).max(1);
    let preview_height = ((image_height as f64 / source_scale_y).round() as i32).max(1);
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("ScreenInk")
        .default_width(preview_width)
        .default_height(preview_height)
        .build();
    let canvas = gtk::DrawingArea::builder().build();
    canvas.set_hexpand(true);
    canvas.set_vexpand(true);
    let annotations = Rc::new(RefCell::new(Vec::<Annotation>::new()));
    let preview_annotations = annotations.clone();
    canvas.set_draw_func(move |_, context, width, height| {
        draw_annotations(
            context,
            &preview_annotations.borrow(),
            width as f64,
            height as f64,
            preview_width as f64,
            preview_height as f64,
        );
    });

    let mut preview_png = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut preview_png),
            image::ImageFormat::Png,
        )
        .expect("encode preview");
    let pixbuf =
        gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(preview_png)).expect("decode preview");
    let texture = gdk::Texture::for_pixbuf(&pixbuf);
    let picture = gtk::Picture::for_paintable(&texture);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_can_shrink(true);

    let tool = Rc::new(RefCell::new(Tool::Pen));
    let brush = Rc::new(RefCell::new(Brush::default()));
    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    let begin_annotations = annotations.clone();
    let begin_tool = tool.clone();
    let begin_brush = brush.clone();
    let begin_canvas = canvas.downgrade();
    drag.connect_drag_begin(move |_, x, y| {
        let Some(canvas) = begin_canvas.upgrade() else {
            return;
        };
        let point = map_canvas_point(&canvas, x, y, preview_width as f64, preview_height as f64);
        let style = *begin_brush.borrow();
        let annotation = match *begin_tool.borrow() {
            Tool::Pen => Annotation::Freehand {
                points: vec![point],
                style,
            },
            Tool::Rectangle => Annotation::Rectangle {
                start: point,
                end: point,
                style,
            },
        };
        begin_annotations.borrow_mut().push(annotation);
        canvas.queue_draw();
    });
    let update_annotations = annotations.clone();
    let update_canvas = canvas.downgrade();
    drag.connect_drag_update(move |gesture, dx, dy| {
        if let Some((origin_x, origin_y)) = gesture.start_point() {
            if let Some(canvas) = update_canvas.upgrade() {
                let point = map_canvas_point(
                    &canvas,
                    origin_x + dx,
                    origin_y + dy,
                    preview_width as f64,
                    preview_height as f64,
                );
                if let Some(annotation) = update_annotations.borrow_mut().last_mut() {
                    match annotation {
                        Annotation::Freehand { points, .. } => points.push(point),
                        Annotation::Rectangle { end, .. } => *end = point,
                    }
                }
                canvas.queue_draw();
            }
        }
    });
    canvas.add_controller(drag);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&canvas);
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);

    let toolbar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    let pen_button = gtk::ToggleButton::with_label("Pen");
    pen_button.set_active(true);
    pen_button.set_tooltip_text(Some("Freehand drawing"));
    let rectangle_button = gtk::ToggleButton::with_label("Rectangle");
    rectangle_button.set_group(Some(&pen_button));
    rectangle_button.set_tooltip_text(Some("Draw a red rectangle"));
    let pen_tool = tool.clone();
    pen_button.connect_toggled(move |button| {
        if button.is_active() {
            *pen_tool.borrow_mut() = Tool::Pen;
        }
    });
    let rectangle_tool = tool.clone();
    rectangle_button.connect_toggled(move |button| {
        if button.is_active() {
            *rectangle_tool.borrow_mut() = Tool::Rectangle;
        }
    });
    let size_label = gtk::Label::new(Some("Size"));
    let size_value = gtk::SpinButton::with_range(1.0, 24.0, 1.0);
    size_value.set_value(PEN_WIDTH);
    size_value.set_tooltip_text(Some("Line thickness"));
    let value_brush = brush.clone();
    size_value.connect_value_changed(move |spin| value_brush.borrow_mut().width = spin.value());
    let color = gtk::ColorButton::new();
    color.set_rgba(&gdk::RGBA::new(0.9, 0.02, 0.02, 1.0));
    color.set_tooltip_text(Some("Line color"));
    let color_brush = brush.clone();
    color.connect_color_set(move |button| {
        let rgba = button.rgba();
        let width = color_brush.borrow().width;
        *color_brush.borrow_mut() = Brush {
            red: rgba.red() as f64,
            green: rgba.green() as f64,
            blue: rgba.blue() as f64,
            width,
        };
    });
    let undo_button = gtk::Button::with_label("Undo");
    undo_button.set_tooltip_text(Some("Undo last annotation (Ctrl+Z)"));
    let undo_annotations = annotations.clone();
    let undo_canvas = canvas.downgrade();
    undo_button.connect_clicked(move |_| {
        undo_annotations.borrow_mut().pop();
        if let Some(canvas) = undo_canvas.upgrade() {
            canvas.queue_draw();
        }
    });
    let save_button = gtk::Button::with_label("Save");
    save_button.add_css_class("suggested-action");
    save_button.set_tooltip_text(Some("Save and copy (Enter)"));
    let save_window = window.downgrade();
    let save_image = image.clone();
    let save_annotations = annotations.clone();
    let save_config = config.clone();
    let save_state = state.clone();
    save_button.connect_clicked(move |_| {
        if let Some(window) = save_window.upgrade() {
            finish_editor(
                &window,
                &save_image,
                &save_annotations.borrow(),
                &save_config.output_dir,
                source_scale_x,
                source_scale_y,
                save_state.clone(),
            );
        }
    });
    toolbar.append(&pen_button);
    toolbar.append(&rectangle_button);
    toolbar.append(&size_label);
    toolbar.append(&size_value);
    toolbar.append(&color);
    toolbar.append(&undo_button);
    toolbar.append(&save_button);
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    layout.append(&overlay);
    layout.append(&toolbar);
    window.set_child(Some(&layout));

    let (toolbar_min_width, _, _, _) = toolbar.measure(gtk::Orientation::Horizontal, -1);
    let (_, toolbar_height, _, _) = toolbar.measure(gtk::Orientation::Vertical, -1);
    let content_width = preview_width.max(toolbar_min_width);
    let content_height = preview_height.saturating_add(toolbar_height);
    window.set_default_size(content_width, content_height);

    let keys = gtk::EventControllerKey::new();
    let key_window = window.downgrade();
    let key_annotations = annotations.clone();
    let key_image = image.clone();
    let key_config = config.clone();
    let key_canvas = canvas.downgrade();
    let key_state = state.clone();
    keys.connect_key_pressed(move |_, key, keycode, modifiers| {
        let Some(window) = key_window.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if modifiers.contains(gdk::ModifierType::CONTROL_MASK) && keycode == X11_KEYCODE_Z {
            key_annotations.borrow_mut().pop();
            if let Some(canvas) = key_canvas.upgrade() {
                canvas.queue_draw();
            }
            return glib::Propagation::Stop;
        }
        match key {
            gdk::Key::Escape => {
                window.close();
                glib::Propagation::Stop
            }
            gdk::Key::Return => {
                finish_editor(
                    &window,
                    &key_image,
                    &key_annotations.borrow(),
                    &key_config.output_dir,
                    source_scale_x,
                    source_scale_y,
                    key_state.clone(),
                );
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(keys);
    let close_state = state.clone();
    window.connect_close_request(move |window| {
        close_state.release_window(window);
        lifetime.borrow_mut().take();
        glib::Propagation::Proceed
    });
    window.present();
}

#[cfg(any())]
fn connected_monitors(source_width: u32, source_height: u32) -> Vec<MonitorGeometry> {
    let monitors = x11rb::connect(None)
        .ok()
        .and_then(|(connection, screen_index)| {
            let root = connection.setup().roots[screen_index].root;
            connection
                .randr_get_monitors(root, true)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
        })
        .map(|reply| {
            reply
                .monitors
                .into_iter()
                .map(|monitor| MonitorGeometry {
                    x: i32::from(monitor.x),
                    y: i32::from(monitor.y),
                    width: u32::from(monitor.width),
                    height: u32::from(monitor.height),
                })
                .filter(|monitor| monitor.width > 0 && monitor.height > 0)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if monitors.is_empty() {
        vec![MonitorGeometry::fallback(source_width, source_height)]
    } else {
        monitors
    }
}

#[cfg(any())]
fn crop_monitor_image(source: &DynamicImage, monitor: MonitorGeometry) -> RgbaImage {
    let x = monitor.x.clamp(0, source.width().saturating_sub(1) as i32) as u32;
    let y = monitor.y.clamp(0, source.height().saturating_sub(1) as i32) as u32;
    let width = monitor.width.min(source.width().saturating_sub(x));
    let height = monitor.height.min(source.height().saturating_sub(y));
    source
        .crop_imm(x, y, width.max(1), height.max(1))
        .to_rgba8()
}

#[cfg(any())]
fn texture_from_image(image: &RgbaImage) -> gdk::Texture {
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(image.clone())
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encode monitor preview");
    let pixbuf =
        gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(png)).expect("decode monitor preview");
    gdk::Texture::for_pixbuf(&pixbuf)
}

#[cfg(any())]
fn gdk_monitor_at(index: usize) -> Option<gdk::Monitor> {
    gdk::Display::default()?
        .monitors()
        .item(index as u32)?
        .downcast::<gdk::Monitor>()
        .ok()
}

fn image_fit_transform(
    available_width: f64,
    available_height: f64,
    image_width: f64,
    image_height: f64,
) -> (f64, f64, f64) {
    let scale = (available_width / image_width)
        .min(available_height / image_height)
        .max(0.000_001);
    let offset_x = (available_width - image_width * scale) / 2.0;
    let offset_y = (available_height - image_height * scale) / 2.0;
    (scale, offset_x, offset_y)
}

fn map_canvas_point(
    canvas: &gtk::DrawingArea,
    x: f64,
    y: f64,
    image_width: f64,
    image_height: f64,
) -> (f64, f64) {
    let (scale, offset_x, offset_y) = image_fit_transform(
        canvas.allocated_width() as f64,
        canvas.allocated_height() as f64,
        image_width,
        image_height,
    );
    (
        ((x - offset_x) / scale).clamp(0.0, image_width),
        ((y - offset_y) / scale).clamp(0.0, image_height),
    )
}

fn draw_annotations(
    context: &gtk::cairo::Context,
    annotations: &[Annotation],
    available_width: f64,
    available_height: f64,
    image_width: f64,
    image_height: f64,
) {
    let (scale, offset_x, offset_y) =
        image_fit_transform(available_width, available_height, image_width, image_height);
    context.save().expect("save drawing state");
    context.translate(offset_x, offset_y);
    context.scale(scale, scale);
    context.set_line_cap(gtk::cairo::LineCap::Round);
    context.set_line_join(gtk::cairo::LineJoin::Round);
    for annotation in annotations {
        match annotation {
            Annotation::Freehand { points, style } => {
                context.set_source_rgb(style.red, style.green, style.blue);
                context.set_line_width(style.width);
                if let Some((x, y)) = points.first() {
                    context.move_to(*x, *y);
                    for (x, y) in &points[1..] {
                        context.line_to(*x, *y);
                    }
                    if points.len() == 1 {
                        context.arc(*x, *y, style.width / 2.0, 0.0, std::f64::consts::TAU);
                        context.fill().expect("draw red dot");
                    } else {
                        context.stroke().expect("draw red stroke");
                    }
                }
            }
            Annotation::Rectangle { start, end, style } => {
                context.set_source_rgb(style.red, style.green, style.blue);
                context.set_line_width(style.width);
                let x = start.0.min(end.0);
                let y = start.1.min(end.1);
                context.rectangle(x, y, (start.0 - end.0).abs(), (start.1 - end.1).abs());
                context.stroke().expect("draw red rectangle");
            }
        }
    }
    context.restore().expect("restore drawing state");
}

fn paint_circle(image: &mut RgbaImage, center_x: i32, center_y: i32, radius: i32, style: Brush) {
    for y in (center_y - radius)..=(center_y + radius) {
        for x in (center_x - radius)..=(center_x + radius) {
            if x >= 0
                && y >= 0
                && (x as u32) < image.width()
                && (y as u32) < image.height()
                && (x - center_x).pow(2) + (y - center_y).pow(2) <= radius.pow(2)
            {
                image.put_pixel(
                    x as u32,
                    y as u32,
                    Rgba([
                        (style.red * 255.0).round() as u8,
                        (style.green * 255.0).round() as u8,
                        (style.blue * 255.0).round() as u8,
                        255,
                    ]),
                );
            }
        }
    }
}

fn draw_line(
    image: &mut RgbaImage,
    start: (f64, f64),
    end: (f64, f64),
    pen_width: f64,
    style: Brush,
) {
    let dx = end.0 - start.0;
    let dy = end.1 - start.1;
    let steps = dx.abs().max(dy.abs()).max(1.0) as i32;
    for step in 0..=steps {
        let t = step as f64 / steps as f64;
        paint_circle(
            image,
            (start.0 + dx * t).round() as i32,
            (start.1 + dy * t).round() as i32,
            (pen_width / 2.0).ceil() as i32,
            style,
        );
    }
}

fn render_annotations(
    source: &RgbaImage,
    annotations: &[Annotation],
    source_scale_x: f64,
    source_scale_y: f64,
) -> RgbaImage {
    let mut result = source.clone();
    for annotation in annotations {
        match annotation {
            Annotation::Freehand { points, style } => {
                let scaled_points: Vec<(f64, f64)> = points
                    .iter()
                    .map(|(x, y)| (x * source_scale_x, y * source_scale_y))
                    .collect();
                for segment in scaled_points.windows(2) {
                    draw_line(
                        &mut result,
                        segment[0],
                        segment[1],
                        style.width * ((source_scale_x + source_scale_y) / 2.0),
                        *style,
                    );
                }
                if scaled_points.len() == 1 {
                    paint_circle(
                        &mut result,
                        scaled_points[0].0.round() as i32,
                        scaled_points[0].1.round() as i32,
                        (style.width * ((source_scale_x + source_scale_y) / 2.0) / 2.0).ceil()
                            as i32,
                        *style,
                    );
                }
            }
            Annotation::Rectangle { start, end, style } => {
                let top_left = (
                    start.0.min(end.0) * source_scale_x,
                    start.1.min(end.1) * source_scale_y,
                );
                let bottom_right = (
                    start.0.max(end.0) * source_scale_x,
                    start.1.max(end.1) * source_scale_y,
                );
                let top_right = (bottom_right.0, top_left.1);
                let bottom_left = (top_left.0, bottom_right.1);
                let scaled_width = style.width * ((source_scale_x + source_scale_y) / 2.0);
                draw_line(&mut result, top_left, top_right, scaled_width, *style);
                draw_line(&mut result, top_right, bottom_right, scaled_width, *style);
                draw_line(&mut result, bottom_right, bottom_left, scaled_width, *style);
                draw_line(&mut result, bottom_left, top_left, scaled_width, *style);
            }
        }
    }
    result
}

fn save_and_copy(
    source: &RgbaImage,
    annotations: &[Annotation],
    output_dir: &Path,
    source_scale_x: f64,
    source_scale_y: f64,
) -> Result<(PathBuf, gdk::Clipboard), String> {
    log_event(format!("rendering {} annotation(s)", annotations.len()));
    fs::create_dir_all(output_dir).map_err(|error| {
        format!(
            "Could not create output directory {}: {error}",
            output_dir.display()
        )
    })?;
    let path = output_dir.join(format!(
        "screenink-{}.png",
        Local::now().format("%Y-%m-%d_%H-%M-%S")
    ));
    render_annotations(source, annotations, source_scale_x, source_scale_y)
        .save(&path)
        .map_err(|error| format!("Could not save PNG: {error}"))?;
    log_event(format!("saved annotated PNG: {}", path.display()));
    let clipboard = gdk::Display::default()
        .ok_or_else(|| "No graphical display is available".to_owned())?
        .clipboard();
    let provider = png_content_provider(&path)?;
    clipboard
        .set_content(Some(&provider))
        .map_err(|error| format!("Could not place PNG in the clipboard: {error}"))?;
    log_event("annotated PNG placed in clipboard by editor");
    Ok((path, clipboard))
}

/// Publish the exact saved PNG, rather than a texture which GTK has to
/// re-encode for each paste target. This makes the advertised MIME type
/// unambiguously `image/png`.
fn png_content_provider(path: &Path) -> Result<gdk::ContentProvider, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("Could not read saved PNG for clipboard: {error}"))?;
    let bytes = glib::Bytes::from_owned(bytes);
    Ok(gdk::ContentProvider::for_bytes("image/png", &bytes))
}

fn finish_editor(
    window: &gtk::ApplicationWindow,
    image: &RgbaImage,
    annotations: &[Annotation],
    output_dir: &Path,
    source_scale_x: f64,
    source_scale_y: f64,
    state: Rc<SessionState>,
) {
    match save_and_copy(
        image,
        annotations,
        output_dir,
        source_scale_x,
        source_scale_y,
    ) {
        Ok((path, clipboard)) => {
            log_event(format!("editor save completed: {}", path.display()));
            state.retain_clipboard(window);
            clipboard.store_async(
                glib::Priority::DEFAULT,
                None::<&gio::Cancellable>,
                move |result| match result {
                    Ok(()) => {
                        log_event("clipboard stored remotely");
                    }
                    Err(error) => {
                        log_event(format!(
                            "clipboard cannot be stored explicitly: {error}; local owner remains active"
                        ));
                    }
                },
            );
        }
        Err(error) => show_error(window, &error),
    }
}

fn show_error(window: &gtk::ApplicationWindow, message: &str) {
    log_event(format!("error dialog: {message}"));
    let dialog = gtk::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .message_type(gtk::MessageType::Error)
        .text(message)
        .build();
    dialog.connect_response(|dialog, _| dialog.close());
    dialog.show();
}

fn open_editor_from_path(app: &gtk::Application, path: &Path, state: Rc<SessionState>) {
    if !state.begin_capture() {
        log_event(format!(
            "discarded capture while editor is active: {}",
            path.display()
        ));
        return;
    }
    if state.has_clipboard.get() {
        log_event("opening next editor while retaining clipboard data");
    }

    let image = image::open(path);
    // The capture helper creates this copy solely for handing off to the
    // persistent process. The editor now owns decoded pixels, so it is safe
    // to remove it before presenting the window.
    if let Err(error) = fs::remove_file(path) {
        log_event(format!(
            "could not remove temporary portal capture: {error}"
        ));
    }

    match image {
        Ok(image) => {
            let scale = portal_preview_scale();
            log_event(format!(
                "opening editor: source={}x{}, preview_scale={scale}",
                image.width(),
                image.height()
            ));
            let lifetime = Rc::new(RefCell::new(Some(app.hold())));
            begin_editor(
                app,
                image,
                Rc::new(load_config()),
                scale,
                scale,
                lifetime,
                state,
            );
        }
        Err(error) => {
            log_event(format!("could not decode portal image: {error}"));
            state.finish_capture();
            let window = gtk::ApplicationWindow::builder().application(app).build();
            show_error(
                &window,
                &format!("Could not open the portal screenshot: {error}"),
            );
            window.present();
        }
    }
}

fn current_executable() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| format!("Could not find ScreenInk executable: {error}"))
}

/// Ensure the hidden, long-lived instance exists before the short capture
/// helper asks GNOME for a screenshot. It is the instance that later owns the
/// Wayland clipboard source.
fn ensure_service() -> Result<(), String> {
    let executable = current_executable()?;
    Command::new(executable)
        .arg(DAEMON_ARGUMENT)
        .spawn()
        .map_err(|error| format!("Could not start the ScreenInk service: {error}"))?;
    // The first invocation has to register its D-Bus name before the portal
    // response is handed back. On later invocations this merely lets the
    // short remote --daemon command exit.
    std::thread::sleep(Duration::from_millis(150));
    Ok(())
}

fn handoff_copy(source: &Path) -> Result<PathBuf, String> {
    let suffix = Local::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| Local::now().timestamp_micros() * 1_000);
    let destination = std::env::temp_dir().join(format!(
        "screenink-portal-{}-{suffix}.png",
        std::process::id()
    ));
    fs::copy(source, &destination)
        .map_err(|error| format!("Could not prepare the portal image for editing: {error}"))?;
    Ok(destination)
}

fn deliver_capture_to_service(path: &Path) -> Result<(), String> {
    let executable = current_executable()?;
    let status = Command::new(executable)
        .arg(OPEN_ARGUMENT)
        .arg(path)
        .status()
        .map_err(|error| format!("Could not send screenshot to ScreenInk: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "ScreenInk rejected the screenshot (exit status {status})"
        ))
    }
}

/// Entry point used by the Print shortcut. This process deliberately does not
/// own the GTK application D-Bus name: it must be a fresh Portal caller on
/// every press, while the service remains alive for clipboard ownership.
fn run_capture_helper() -> i32 {
    if let Err(error) = ensure_service() {
        eprintln!("{error}");
        return 1;
    }

    let config = load_config();
    match capture_with_portal(config.portal_timeout_seconds) {
        Ok(Some(path)) => match handoff_copy(&path).and_then(|copy| {
            let result = deliver_capture_to_service(&copy);
            if result.is_err() {
                log_event(format!(
                    "handoff image retained for diagnosis: {}",
                    copy.display()
                ));
            }
            result
        }) {
            Ok(()) => {
                log_event("portal image delivered to the editor service");
                0
            }
            Err(error) => {
                log_event(format!("capture handoff failed: {error}"));
                eprintln!("{error}");
                1
            }
        },
        Ok(None) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn command_line_arguments(command_line: &gio::ApplicationCommandLine) -> Vec<String> {
    command_line
        .arguments()
        .iter()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

fn main() {
    if std::env::args().skip(1).next().as_deref() == Some(CAPTURE_ARGUMENT) {
        std::process::exit(run_capture_helper());
    }

    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    let service_lifetime = Rc::new(app.hold());
    // `startup` is emitted only by the primary GApplication instance. A
    // remote `--daemon` or `--open` invocation therefore appends to this log
    // instead of truncating it.
    app.connect_startup(|_| initialize_log());
    let state = Rc::new(SessionState::default());
    app.connect_command_line(move |app, command_line| {
        // Keep the primary instance running even with no visible GTK window.
        let _service_lifetime = &service_lifetime;
        let arguments = command_line_arguments(command_line);
        match arguments.as_slice() {
            [argument] if argument == DAEMON_ARGUMENT => {
                log_event("service activation received");
            }
            [argument, path] if argument == OPEN_ARGUMENT => {
                open_editor_from_path(app, Path::new(path), state.clone());
            }
            [] => {
                log_event("direct launch starts a fresh capture helper");
                match current_executable() {
                    Ok(executable) => {
                        if let Err(error) = Command::new(executable).arg(CAPTURE_ARGUMENT).spawn() {
                            log_event(format!("could not start capture helper: {error}"));
                        }
                    }
                    Err(error) => log_event(error),
                }
            }
            _ => {
                log_event(format!("unsupported ScreenInk command: {arguments:?}"));
            }
        }
        0
    });
    app.run();
}
