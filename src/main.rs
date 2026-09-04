use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use chrono::Local;
use gtk::{gdk, gio, glib, prelude::*};
use image::{DynamicImage, Rgba, RgbaImage};
use serde::Deserialize;
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, ImageFormat},
};

const APP_ID: &str = "io.github.stan.ScreenInk";
const PEN_WIDTH: f64 = 5.0;
// X11 keycode for the physical key in the Z position (evdev KEY_Z + 8).
// Keycodes are not affected by switching XKB language groups.
const X11_KEYCODE_Z: u32 = 52;
static APP_HELD: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    output_dir: Option<PathBuf>,
}

#[derive(Debug)]
struct Config {
    output_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output_dir: dirs::picture_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Screenshots"),
        }
    }
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("screenink/config.toml")
}

fn load_config() -> Config {
    let path = config_path();
    let file_config: FileConfig = fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default();
    Config {
        output_dir: file_config
            .output_dir
            .unwrap_or_else(|| Config::default().output_dir),
    }
}

fn temporary_capture_path() -> PathBuf {
    std::env::temp_dir().join(format!("screenink-source-{}.png", std::process::id()))
}

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

fn x11_channel(pixel: u32, mask: u32) -> u8 {
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    (((pixel & mask) >> shift) * 255 / maximum) as u8
}

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

#[derive(Default)]
struct SelectionState {
    start: Option<(f64, f64)>,
    current: Option<(f64, f64)>,
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

fn begin_selection(app: &gtk::Application, source_path: PathBuf, config: Rc<Config>) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("ScreenInk — Select an area")
        .decorated(false)
        .build();
    window.fullscreen();

    let screenshot_file = gio::File::for_path(&source_path);
    let texture = match gdk::Texture::from_file(&screenshot_file) {
        Ok(texture) => texture,
        Err(error) => {
            show_error(
                &window,
                &format!("Could not display the screenshot: {error}"),
            );
            window.present();
            return;
        }
    };
    let picture = gtk::Picture::for_paintable(&texture);
    picture.set_hexpand(true);
    picture.set_vexpand(true);

    let canvas = gtk::DrawingArea::new();
    canvas.set_hexpand(true);
    canvas.set_vexpand(true);
    canvas.set_cursor_from_name(Some("crosshair"));
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&canvas);
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
    let end_source = source_path.clone();
    let end_config = config.clone();
    drag.connect_drag_end(move |_, offset_x, offset_y| {
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
            source.width() as f64,
            source.height() as f64,
        );
        let end = map_canvas_point(
            &canvas,
            end.0,
            end.1,
            source.width() as f64,
            source.height() as f64,
        );
        let x = start.0.min(end.0);
        let y = start.1.min(end.1);
        let width = (start.0 - end.0).abs();
        let height = (start.1 - end.1).abs();
        if width < 3.0 || height < 3.0 {
            return;
        }
        let crop = source.crop_imm(
            x.max(0.0) as u32,
            y.max(0.0) as u32,
            width as u32,
            height as u32,
        );
        window.close();
        // GTK reads this scale directly from the active GNOME monitor. On X11,
        // a fractional scale of 175% usually maps to a backend scale of 2.
        let preview_scale = (canvas.scale_factor() as f64).max(1.0);
        begin_editor(
            &app,
            crop,
            end_config.clone(),
            preview_scale,
            preview_scale,
            x.round() as i32,
            y.round() as i32,
        );
    });
    canvas.add_controller(drag);

    let keys = gtk::EventControllerKey::new();
    let key_window = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            if let Some(window) = key_window.upgrade() {
                window.close();
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
    window.present();
}

fn begin_editor(
    app: &gtk::Application,
    image: DynamicImage,
    config: Rc<Config>,
    source_scale_x: f64,
    source_scale_y: f64,
    editor_x: i32,
    editor_y: i32,
) {
    let image = image.to_rgba8();
    let (image_width, image_height) = image.dimensions();
    let preview_width = ((image_width as f64 / source_scale_x).round() as i32).max(1);
    let preview_height = ((image_height as f64 / source_scale_y).round() as i32).max(1);
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("ScreenInk")
        .default_width(preview_width.min(1200))
        .default_height(preview_height.min(800))
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
    save_button.connect_clicked(move |_| {
        if let Some(window) = save_window.upgrade() {
            finish_editor(
                &window,
                &save_image,
                &save_annotations.borrow(),
                &save_config.output_dir,
                source_scale_x,
                source_scale_y,
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

    let keys = gtk::EventControllerKey::new();
    let key_window = window.downgrade();
    let key_annotations = annotations.clone();
    let key_image = image.clone();
    let key_config = config.clone();
    let key_canvas = canvas.downgrade();
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
                );
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(keys);
    window.present();
    glib::timeout_add_local(Duration::from_millis(120), move || {
        move_active_x11_window(editor_x, editor_y);
        glib::ControlFlow::Break
    });
}

fn move_active_x11_window(x: i32, y: i32) {
    let Ok(output) = Command::new("xdotool").arg("getactivewindow").output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let window_id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if window_id.is_empty() {
        return;
    }
    let target_y = (y - x11_frame_top_inset(&window_id)).max(0);
    let _ = Command::new("xdotool")
        .args([
            "windowmove",
            "--sync",
            &window_id,
            &x.to_string(),
            &target_y.to_string(),
        ])
        .status();
}

fn x11_frame_top_inset(window_id: &str) -> i32 {
    let Ok(output) = Command::new("xprop")
        .args(["-id", window_id, "_NET_FRAME_EXTENTS"])
        .output()
    else {
        return 0;
    };
    let values: Vec<i32> = String::from_utf8_lossy(&output.stdout)
        .split(|character: char| !character.is_ascii_digit())
        .filter_map(|value| value.parse().ok())
        .collect();
    // EWMH specifies: left, right, top, bottom.
    values.get(2).copied().unwrap_or(0)
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
) -> Result<PathBuf, String> {
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
    let file = gio::File::for_path(&path);
    let texture = gdk::Texture::from_file(&file)
        .map_err(|error| format!("Could not prepare the clipboard: {error}"))?;
    gdk::Display::default()
        .ok_or_else(|| "No graphical display is available".to_owned())?
        .clipboard()
        .set_texture(&texture);
    Ok(path)
}

fn finish_editor(
    window: &gtk::ApplicationWindow,
    image: &RgbaImage,
    annotations: &[Annotation],
    output_dir: &Path,
    source_scale_x: f64,
    source_scale_y: f64,
) {
    match save_and_copy(
        image,
        annotations,
        output_dir,
        source_scale_x,
        source_scale_y,
    ) {
        Ok(path) => {
            eprintln!("Screenshot saved: {}", path.display());
            window.close();
        }
        Err(error) => show_error(window, &error),
    }
}

fn show_error(window: &gtk::ApplicationWindow, message: &str) {
    let dialog = gtk::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .message_type(gtk::MessageType::Error)
        .text(message)
        .build();
    dialog.connect_response(|dialog, _| dialog.close());
    dialog.show();
}

fn main() {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        // On X11, the clipboard belongs to the source process. Hold only the
        // primary D-Bus instance; remote Print launches activate it and exit.
        if app.is_remote() {
            return;
        }
        if !APP_HELD.swap(true, Ordering::SeqCst) {
            std::mem::forget(app.hold());
        }
        let config = Rc::new(load_config());
        match capture_desktop() {
            Ok(source) => begin_selection(app, source, config),
            Err(error) => {
                let window = gtk::ApplicationWindow::builder().application(app).build();
                show_error(&window, &error);
                window.present();
            }
        }
    });
    app.run();
}
