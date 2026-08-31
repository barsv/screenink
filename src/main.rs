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
    let status = Command::new("gnome-screenshot")
        .args(["-f", path.to_string_lossy().as_ref()])
        .status()
        .map_err(|error| format!("Не удалось запустить gnome-screenshot: {error}"))?;

    if status.success() && path.is_file() {
        Ok(path)
    } else {
        Err("gnome-screenshot не смог создать изображение экрана".to_owned())
    }
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

fn begin_selection(app: &gtk::Application, source_path: PathBuf, config: Rc<Config>) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("ScreenInk — выделите область")
        .decorated(false)
        .build();
    window.fullscreen();

    let screenshot_file = gio::File::for_path(&source_path);
    let texture = match gdk::Texture::from_file(&screenshot_file) {
        Ok(texture) => texture,
        Err(error) => {
            show_error(
                &window,
                &format!("Не удалось показать снимок для выделения: {error}"),
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
        let x = start_x.min(end.0);
        let y = start_y.min(end.1);
        drop(selection);

        let allocated_width = canvas.allocated_width() as f64;
        let allocated_height = canvas.allocated_height() as f64;
        let source = match image::open(&end_source) {
            Ok(source) => source,
            Err(error) => {
                show_error(&window, &format!("Не удалось открыть снимок: {error}"));
                return;
            }
        };
        let scale_x = source.width() as f64 / allocated_width;
        let scale_y = source.height() as f64 / allocated_height;
        let crop = source.crop_imm(
            (x * scale_x).max(0.0) as u32,
            (y * scale_y).max(0.0) as u32,
            (width * scale_x) as u32,
            (height * scale_y) as u32,
        );
        window.close();
        // GTK получает масштаб непосредственно от активного монитора GNOME.
        // При дробном масштабе 175% в X11 это обычно бэкендный масштаб 2.
        let preview_scale = (canvas.scale_factor() as f64).max(1.0);
        begin_editor(
            &app,
            crop,
            end_config.clone(),
            preview_scale,
            preview_scale,
            (x * scale_x).round() as i32,
            (y * scale_y).round() as i32,
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
    // gnome-screenshot возвращает физические пиксели, а GTK принимает размеры в
    // логических. На HiDPI-дисплее нельзя показывать PNG в исходном размере.
    let preview_width = ((image_width as f64 / source_scale_x).round() as i32).max(1);
    let preview_height = ((image_height as f64 / source_scale_y).round() as i32).max(1);
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("ScreenInk — рисуйте красным, Enter: сохранить и скопировать")
        .default_width(preview_width.min(1200))
        .default_height(preview_height.min(800))
        .build();
    let canvas = gtk::DrawingArea::builder()
        .content_width(preview_width)
        .content_height(preview_height)
        .build();
    let strokes = Rc::new(RefCell::new(Vec::<Vec<(f64, f64)>>::new()));
    let preview_strokes = strokes.clone();
    canvas.set_draw_func(move |_, context, _, _| {
        draw_strokes(context, &preview_strokes.borrow());
    });

    let mut preview_png = Vec::new();
    // Не уменьшаем файл заранее: GtkPicture масштабирует оригинальный снимок при
    // отрисовке и сохраняет заметно более чёткие шрифты.
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
    picture.set_size_request(preview_width, preview_height);
    picture.set_can_shrink(true);

    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    let begin_strokes = strokes.clone();
    let begin_canvas = canvas.downgrade();
    drag.connect_drag_begin(move |_, x, y| {
        begin_strokes.borrow_mut().push(vec![(x, y)]);
        if let Some(canvas) = begin_canvas.upgrade() {
            canvas.queue_draw();
        }
    });
    let update_strokes = strokes.clone();
    let update_canvas = canvas.downgrade();
    drag.connect_drag_update(move |gesture, dx, dy| {
        if let Some((origin_x, origin_y)) = gesture.start_point() {
            if let Some(stroke) = update_strokes.borrow_mut().last_mut() {
                stroke.push((origin_x + dx, origin_y + dy));
            }
        }
        if let Some(canvas) = update_canvas.upgrade() {
            canvas.queue_draw();
        }
    });
    canvas.add_controller(drag);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&canvas);
    let scroll = gtk::ScrolledWindow::builder().child(&overlay).build();
    window.set_child(Some(&scroll));
    let keys = gtk::EventControllerKey::new();
    let key_window = window.downgrade();
    let key_strokes = strokes.clone();
    let key_image = image.clone();
    let key_config = config.clone();
    let key_canvas = canvas.downgrade();
    keys.connect_key_pressed(move |_, key, keycode, modifiers| {
        let Some(window) = key_window.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if modifiers.contains(gdk::ModifierType::CONTROL_MASK) && keycode == X11_KEYCODE_Z {
            key_strokes.borrow_mut().pop();
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
                match save_and_copy(
                    &key_image,
                    &key_strokes.borrow(),
                    &key_config.output_dir,
                    source_scale_x,
                    source_scale_y,
                ) {
                    Ok(path) => {
                        eprintln!("Снимок сохранён: {}", path.display());
                        window.close();
                    }
                    Err(error) => show_error(&window, &error),
                }
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

fn draw_strokes(context: &gtk::cairo::Context, strokes: &[Vec<(f64, f64)>]) {
    context.set_source_rgb(0.9, 0.02, 0.02);
    context.set_line_width(PEN_WIDTH);
    context.set_line_cap(gtk::cairo::LineCap::Round);
    context.set_line_join(gtk::cairo::LineJoin::Round);
    for stroke in strokes {
        if let Some((x, y)) = stroke.first() {
            context.move_to(*x, *y);
            for (x, y) in &stroke[1..] {
                context.line_to(*x, *y);
            }
            context.stroke().expect("draw red stroke");
        }
    }
}

fn paint_circle(image: &mut RgbaImage, center_x: i32, center_y: i32, radius: i32) {
    for y in (center_y - radius)..=(center_y + radius) {
        for x in (center_x - radius)..=(center_x + radius) {
            if x >= 0
                && y >= 0
                && (x as u32) < image.width()
                && (y as u32) < image.height()
                && (x - center_x).pow(2) + (y - center_y).pow(2) <= radius.pow(2)
            {
                image.put_pixel(x as u32, y as u32, Rgba([230, 5, 5, 255]));
            }
        }
    }
}

fn draw_line(image: &mut RgbaImage, start: (f64, f64), end: (f64, f64), pen_width: f64) {
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
        );
    }
}

fn render_annotations(
    source: &RgbaImage,
    strokes: &[Vec<(f64, f64)>],
    source_scale_x: f64,
    source_scale_y: f64,
) -> RgbaImage {
    let mut result = source.clone();
    let pen_width = PEN_WIDTH * ((source_scale_x + source_scale_y) / 2.0);
    for stroke in strokes {
        let scaled_stroke: Vec<(f64, f64)> = stroke
            .iter()
            .map(|(x, y)| (x * source_scale_x, y * source_scale_y))
            .collect();
        for segment in scaled_stroke.windows(2) {
            draw_line(&mut result, segment[0], segment[1], pen_width);
        }
        if scaled_stroke.len() == 1 {
            paint_circle(
                &mut result,
                scaled_stroke[0].0.round() as i32,
                scaled_stroke[0].1.round() as i32,
                (pen_width / 2.0).ceil() as i32,
            );
        }
    }
    result
}

fn save_and_copy(
    source: &RgbaImage,
    strokes: &[Vec<(f64, f64)>],
    output_dir: &Path,
    source_scale_x: f64,
    source_scale_y: f64,
) -> Result<PathBuf, String> {
    fs::create_dir_all(output_dir).map_err(|error| {
        format!(
            "Не удалось создать каталог {}: {error}",
            output_dir.display()
        )
    })?;
    let path = output_dir.join(format!(
        "screenink-{}.png",
        Local::now().format("%Y-%m-%d_%H-%M-%S")
    ));
    render_annotations(source, strokes, source_scale_x, source_scale_y)
        .save(&path)
        .map_err(|error| format!("Не удалось сохранить PNG: {error}"))?;
    let file = gio::File::for_path(&path);
    let texture = gdk::Texture::from_file(&file)
        .map_err(|error| format!("Не удалось подготовить буфер обмена: {error}"))?;
    gdk::Display::default()
        .ok_or_else(|| "Нет доступа к графическому дисплею".to_owned())?
        .clipboard()
        .set_texture(&texture);
    Ok(path)
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
        // На X11 буфер обмена принадлежит процессу-источнику. Удерживаем только
        // основной D-Bus-экземпляр. Удалённые запуски от Print передают ему
        // активацию и завершаются, не оставаясь фоновыми процессами.
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
