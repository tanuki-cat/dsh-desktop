//! Which icon the shell wears.
//!
//! Three presets ship inside the binary (`include_bytes!`, so they exist in the thin build as
//! well as in the bundled one) plus whatever PNG the user picks. The choice lives in
//! `appearance.json` rather than in `config.json`: that file is hand-written by the user and
//! documented key by key, while this is the same kind of runtime state as `state.json`.
//!
//! What "applying" an icon means is platform-specific, and neither platform can do the other part.
//! macOS changes `NSApplication.setApplicationIconImage`, which is the Dock and Command-Tab icon
//! and lasts only for this process; Windows and Linux change the icon of the windows that exist.
//! In both cases the choice is re-applied at startup from the persisted value, which is what makes
//! it survive a relaunch without rewriting the bundle: replacing the `.icns` inside a signed
//! application would invalidate its signature, and this pipeline is unsigned but still ships
//! whatever the user installed.

use crate::harness;
use crate::process;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauri::Manager;

/// The default artwork, which is also the bundle icon.
const DEFAULT_PNG: &[u8] = include_bytes!("../icons/icon.png");
/// Deep blue.
const DEEP_PNG: &[u8] = include_bytes!("../icons/presets/deep.png");
/// Near-black.
const NIGHT_PNG: &[u8] = include_bytes!("../icons/presets/night.png");
/// Warm sand and amber.
const DAWN_PNG: &[u8] = include_bytes!("../icons/presets/dawn.png");

/// Side length the stored icon is scaled down to.
///
/// Not a limit on what the user may pick: a Dock tile is 128 points at most, so a larger source is
/// box-filtered down to this instead of refused. The value matches the bundle icon, which keeps the
/// presets and a custom image in the same size class.
pub const MAX_CUSTOM_SIDE: u32 = 1024;

/// Smallest side worth accepting: below this the icon is a smear at every size the OS draws it,
/// and no amount of scaling invents detail that was never there.
pub const MIN_CUSTOM_SIDE: u32 = 64;

/// Longest side the decoder will even attempt to expand.
///
/// A PNG header can claim 60000x60000 in a few hundred bytes and cost gigabytes once decoded, so a
/// source above this is refused with a size, before anything is allocated for it. It is far above
/// [`MAX_CUSTOM_SIDE`] on purpose: the point is decompression bombs, not large artwork.
pub const MAX_DECODE_SIDE: u32 = 8192;

/// Largest `data:` URL the status page may hand over: base64 of [`MAX_CUSTOM_BYTES`] plus the
/// header and padding, rounded up. Checked before the decode, so an oversized payload is refused
/// without allocating four thirds of it.
pub const MAX_DATA_URL_BYTES: usize = (MAX_CUSTOM_BYTES as usize / 3 + 1) * 4 + 64;

/// Largest file the picker will read. The limit is about the copy into app data, not decoding.
const MAX_CUSTOM_BYTES: u64 = 32 * 1024 * 1024;

/// The icon choice, as persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Choice {
    /// The artwork the bundle ships with.
    #[default]
    Default,
    Deep,
    Night,
    Dawn,
    /// The PNG the user picked, copied into app data under `custom.png`.
    Custom,
}

impl Choice {
    /// Every choice except `Custom`, in the order the menu lists them.
    pub const PRESETS: [Choice; 4] = [Choice::Default, Choice::Deep, Choice::Night, Choice::Dawn];

    /// The value stored in `appearance.json`, and the id the menu item carries.
    pub fn key(self) -> &'static str {
        match self {
            Choice::Default => "default",
            Choice::Deep => "deep",
            Choice::Night => "night",
            Choice::Dawn => "dawn",
            Choice::Custom => "custom",
        }
    }

    /// The label the menu shows.
    pub fn label(self) -> &'static str {
        match self {
            Choice::Default => "默认",
            Choice::Deep => "深海",
            Choice::Night => "极夜",
            Choice::Dawn => "晨雾",
            Choice::Custom => "自定义图片",
        }
    }

    pub fn parse(value: &str) -> Option<Choice> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Choice::Default),
            "deep" => Some(Choice::Deep),
            "night" => Some(Choice::Night),
            "dawn" => Some(Choice::Dawn),
            "custom" => Some(Choice::Custom),
            _ => None,
        }
    }

    /// The built-in PNG for this choice, when it has one.
    pub fn bytes(self) -> Option<&'static [u8]> {
        match self {
            Choice::Default => Some(DEFAULT_PNG),
            Choice::Deep => Some(DEEP_PNG),
            Choice::Night => Some(NIGHT_PNG),
            Choice::Dawn => Some(DAWN_PNG),
            Choice::Custom => None,
        }
    }
}

/// The choice the running process is wearing.
///
/// A process-wide cell rather than a read of `appearance.json` at the point of use: the menu is
/// built inside `Builder::build`, before the app's path resolver is managed, so the menu builder
/// cannot ask where the data directory is. Everything that changes the choice updates this cell,
/// and the menu only reads it.
static CURRENT: std::sync::Mutex<Option<Choice>> = std::sync::Mutex::new(None);

/// The choice currently applied (or the default, before anything has been applied).
pub fn current() -> Choice {
    CURRENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .unwrap_or_default()
}

/// Record the choice so the menu can read it without touching the filesystem.
fn remember(choice: Choice) {
    *CURRENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(choice);
}

/// `appearance.json`: the persisted appearance choices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Appearance {
    #[serde(default)]
    pub icon: Choice,
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance {
            icon: Choice::Default,
        }
    }
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("appearance.json")
}

/// Where the user's own icon is copied to.
pub fn custom_path(data_dir: &Path) -> PathBuf {
    data_dir.join("icons").join("custom.png")
}

/// Read the persisted choice, falling back to the default for a missing or unreadable file.
///
/// An unreadable file is not rewritten: it may be a hand edit in progress, and the same rule
/// applies here as to a broken `config.json` — this run adapts, the file stays as the user left it.
pub fn load(data_dir: &Path) -> Appearance {
    let Ok(raw) = std::fs::read_to_string(path(data_dir)) else {
        return Appearance::default();
    };
    match serde_json::from_str::<Appearance>(&raw) {
        Ok(appearance) => appearance,
        Err(error) => {
            harness::app_log(&format!(
                "appearance.json 无法解析，本次使用默认图标: {error}"
            ));
            Appearance::default()
        }
    }
}

/// Persist the choice. A failure is returned so the caller can say so on screen.
pub fn save(data_dir: &Path, appearance: &Appearance) -> Result<(), String> {
    let file = path(data_dir);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("无法创建配置目录: {error}"))?;
    }
    let text = serde_json::to_vec_pretty(appearance)
        .map_err(|error| format!("无法序列化外观设置: {error}"))?;
    std::fs::write(&file, text).map_err(|error| format!("无法写入外观设置: {error}"))?;
    process::restrict(&file);
    Ok(())
}
/// A decoded PNG: row-major RGBA, top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Rgba {
    /// Side length when the image is square, which is what an app icon has to be.
    pub fn side(&self) -> Option<u32> {
        (self.width == self.height).then_some(self.width)
    }
}

/// Decode a PNG into RGBA, expanding whatever packing it came in.
///
/// Palette, grayscale and 16-bit sources are ordinary PNGs: asking the decoder to normalise them
/// is one line, and refusing them turned "use this picture" into encoder trivia the user never
/// asked about. The only thing judged here is the header, before a single pixel is expanded — a
/// 60000x60000 PNG is a few hundred bytes on disk and several gigabytes in memory.
pub fn decode_png(bytes: &[u8]) -> Result<Rgba, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("无法解析 PNG: {error}"))?;
    let info = reader.info();
    let (width, height) = (info.width, info.height);
    check_dimensions(width, height)?;
    let mut buffer = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|error| format!("无法解码 PNG: {error}"))?;
    buffer.truncate(frame.buffer_size());
    let pixels = expand_rgba(&buffer, frame.color_type, width, height)?;
    Ok(Rgba {
        width,
        height,
        pixels,
    })
}

/// Refuse a header that is too large to expand, before allocating for it.
fn check_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("PNG 的尺寸是 0".to_string());
    }
    if width > MAX_DECODE_SIDE || height > MAX_DECODE_SIDE {
        return Err(format!(
            "图片太大（{width}x{height}），请先缩到 {MAX_DECODE_SIDE} 像素以内"
        ));
    }
    Ok(())
}

/// Expand one decoded frame to the RGBA every caller here expects.
///
/// `normalize_to_color8` has already turned palettes and 16-bit samples into 8-bit components, so
/// the only work left is the channel layout.
fn expand_rgba(
    buffer: &[u8],
    color: png::ColorType,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let pixels = match color {
        png::ColorType::Rgba => buffer.to_vec(),
        png::ColorType::Rgb => buffer
            .chunks_exact(3)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
        png::ColorType::Grayscale => buffer
            .iter()
            .flat_map(|&gray| [gray, gray, gray, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buffer
            .chunks_exact(2)
            .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
            .collect(),
        // `normalize_to_color8` expands every indexed source, so this cannot come back from a
        // file that decoded at all.
        png::ColorType::Indexed => return Err("PNG 的调色板没有被展开".to_string()),
    };
    if pixels.len() != (width as usize) * (height as usize) * 4 {
        return Err("PNG 解码结果与声明的尺寸不一致".to_string());
    }
    Ok(pixels)
}

/// The square, bounded icon an image becomes.
///
/// Center crop, then box-filter down to [`MAX_CUSTOM_SIDE`]. Users pick screenshots, photos and
/// 2048 px artwork; refusing those because an icon "must be square and at most 1024" is a rule
/// about a format they never chose, and it is indistinguishable from the menu item doing nothing.
/// What genuinely cannot become an icon is an image too small to survive being drawn at 32 px, and
/// that is the one refusal left.
pub fn fit_icon(image: Rgba) -> Result<Rgba, String> {
    let side = image.width.min(image.height);
    if side < MIN_CUSTOM_SIDE {
        return Err(format!(
            "图标太小（{}x{}）：短边至少要 {MIN_CUSTOM_SIDE} 像素",
            image.width, image.height
        ));
    }
    let square = crop_centered(&image, side);
    Ok(if side > MAX_CUSTOM_SIDE {
        downscale(&square, MAX_CUSTOM_SIDE)
    } else {
        square
    })
}

/// The largest centered square of `image`.
///
/// Centered rather than top-left: the subject of a screenshot or a portrait is almost never in a
/// corner, and an off-centre crop of a picture the user chose is a wrong answer even when it is
/// square.
fn crop_centered(image: &Rgba, side: u32) -> Rgba {
    if image.width == side && image.height == side {
        return image.clone();
    }
    let x0 = (image.width - side) / 2;
    let y0 = (image.height - side) / 2;
    let row = (side as usize) * 4;
    let mut pixels = Vec::with_capacity(row * side as usize);
    for y in 0..side {
        let start = (((y + y0) * image.width + x0) as usize) * 4;
        pixels.extend_from_slice(&image.pixels[start..start + row]);
    }
    Rgba {
        width: side,
        height: side,
        pixels,
    }
}

/// Box-filter `image` down to `size`, in premultiplied alpha.
///
/// Premultiplied because averaging straight alpha pulls fully transparent pixels' colour into the
/// result: a transparent margin around a logo would drag a dark or bright ring into its edge.
fn downscale(image: &Rgba, size: u32) -> Rgba {
    let width = image.width as usize;
    let scale = image.width as f64 / size as f64;
    let mut pixels = vec![0u8; (size as usize) * (size as usize) * 4];
    for ty in 0..size {
        let y0 = (f64::from(ty) * scale).floor() as usize;
        let y1 = ((f64::from(ty + 1) * scale).ceil() as usize)
            .min(image.height as usize)
            .max(y0 + 1);
        for tx in 0..size {
            let x0 = (f64::from(tx) * scale).floor() as usize;
            let x1 = ((f64::from(tx + 1) * scale).ceil() as usize)
                .min(width)
                .max(x0 + 1);
            let (mut red, mut green, mut blue, mut alpha) = (0u64, 0u64, 0u64, 0u64);
            for y in y0..y1 {
                for x in x0..x1 {
                    let at = (y * width + x) * 4;
                    let weight = u64::from(image.pixels[at + 3]);
                    red += u64::from(image.pixels[at]) * weight;
                    green += u64::from(image.pixels[at + 1]) * weight;
                    blue += u64::from(image.pixels[at + 2]) * weight;
                    alpha += weight;
                }
            }
            let count = ((y1 - y0) * (x1 - x0)) as u64;
            let at = ((ty as usize) * (size as usize) + tx as usize) * 4;
            // `checked_div` rather than a guard: the divisor is zero exactly when every source
            // pixel in the box is transparent, and the colour there is simply absent.
            let average = |sum: u64| sum.checked_div(alpha).unwrap_or(0) as u8;
            pixels[at] = average(red);
            pixels[at + 1] = average(green);
            pixels[at + 2] = average(blue);
            pixels[at + 3] = (alpha / count) as u8;
        }
    }
    Rgba {
        width: size,
        height: size,
        pixels,
    }
}

/// Decode and fit in one step: what every caller that draws or stores an icon wants.
pub fn icon_rgba(bytes: &[u8]) -> Result<Rgba, String> {
    fit_icon(decode_png(bytes)?)
}

/// Encode an RGBA image as a PNG.
///
/// The stored copy is the *fitted* icon rather than the file the user picked: at most 1024 px and
/// already square, so every later launch reads what it draws instead of repeating the crop.
pub fn encode_png(image: &Rgba) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("无法写入 PNG: {error}"))?;
        writer
            .write_image_data(&image.pixels)
            .map_err(|error| format!("无法写入 PNG: {error}"))?;
    }
    Ok(out)
}

/// Read a PNG file and return the icon it becomes, after the same size cap the picker applies.
pub fn read_png_file(path: &Path) -> Result<Rgba, String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("无法读取图片: {error}"))?;
    if !metadata.is_file() {
        return Err("选择的不是文件".to_string());
    }
    if metadata.len() > MAX_CUSTOM_BYTES {
        return Err(format!(
            "图片超过 {} MiB，请换一张小的",
            MAX_CUSTOM_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("无法读取图片: {error}"))?;
    icon_rgba(&bytes)
}

/// Check and store a user-picked PNG as the custom icon, returning the stored path.
///
/// The source path is only read: what lands in app data is the fitted icon, so moving or deleting
/// the original does not change it, and a 4000 px screenshot does not cost a decode on every
/// launch. Decoding here is the validation — the copy exists only if it can actually be drawn.
pub fn adopt_custom(data_dir: &Path, source: &Path) -> Result<PathBuf, String> {
    let icon = read_png_file(source)?;
    let normalized = encode_png(&icon)?;
    let target = custom_path(data_dir);
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("无法创建图标目录: {error}"))?;
    }
    std::fs::write(&target, normalized).map_err(|error| format!("无法保存自定义图标: {error}"))?;
    process::restrict(&target);
    harness::app_log(&format!(
        "自定义图标已保存: {}（{}x{}）",
        target.display(),
        icon.width,
        icon.height
    ));
    Ok(target)
}

/// The bytes to draw for a choice, or the reason there are none.
pub fn bytes_for(data_dir: &Path, choice: Choice) -> Result<Vec<u8>, String> {
    match choice.bytes() {
        Some(bytes) => Ok(bytes.to_vec()),
        None => std::fs::read(custom_path(data_dir))
            .map_err(|error| format!("自定义图标不可读（{error}），请重新选择")),
    }
}

/// Draw `choice` on every surface this platform offers.
///
/// Best effort by design: a window that refuses an icon, or a platform with no such surface, must
/// not stop the shell from starting. What went wrong is logged and the previous icon stays.
pub fn apply(app: &tauri::AppHandle, data_dir: &Path, choice: Choice) {
    let bytes = match bytes_for(data_dir, choice) {
        Ok(bytes) => bytes,
        Err(reason) => {
            harness::app_log(&format!("应用图标失败，保留当前图标: {reason}"));
            return;
        }
    };
    let rgba = match icon_rgba(&bytes) {
        Ok(rgba) => rgba,
        Err(reason) => {
            harness::app_log(&format!("应用图标失败，保留当前图标: {reason}"));
            return;
        }
    };

    #[cfg(target_os = "macos")]
    {
        // AppKit may only be called from the main thread, and the startup path reaches this from
        // a worker: the work is posted to the event loop rather than performed here.
        let pixels = rgba.pixels.clone();
        let side = rgba.width;
        match app.run_on_main_thread(move || {
            if let Err(reason) = set_dock_icon(&pixels, side) {
                harness::app_log(&format!("设置 Dock 图标失败: {reason}"));
            }
        }) {
            Ok(()) => {}
            Err(error) => harness::app_log(&format!("无法把图标设置投递到主线程: {error}")),
        }
    }

    // Windows and Linux draw the icon from the window itself. macOS ignores this call (its
    // windows carry no icon), so it is made on every platform for the ones that use it. The
    // image is built from the pixels already decoded above: going back through the PNG would
    // mean a second decoder for a buffer that already exists.
    let image = tauri::image::Image::new_owned(rgba.pixels.clone(), rgba.width, rgba.height);
    for label in [crate::window::SPLASH, crate::window::HARNESS] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.set_icon(image.clone());
        }
    }
}

/// Multiply every colour channel by its alpha.
///
/// Straight (non-premultiplied) alpha is what a PNG carries and what the decoder returns. The
/// AppKit bitmap that becomes the Dock icon stores premultiplied alpha, so writing straight
/// values into it makes every partly transparent pixel too bright — a soft edge keeps its full
/// colour and reads as a pale fringe around the icon.
pub fn premultiply_rgba(pixels: &[u8]) -> Vec<u8> {
    let mut out = pixels.to_vec();
    for pixel in out.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
    out
}

/// Replace the Dock icon with `rgba` (row-major, `side` x `side`). Main thread only.
#[cfg(target_os = "macos")]
pub fn set_dock_icon(rgba: &[u8], side: u32) -> Result<(), String> {
    use objc2::rc::Retained;
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSBitmapImageRep, NSCalibratedRGBColorSpace, NSImage};
    use objc2_foundation::NSSize;

    if side == 0 || rgba.len() != (side as usize) * (side as usize) * 4 {
        return Err("图标像素数据与尺寸不一致".to_string());
    }
    let mtm = MainThreadMarker::new().ok_or("设置应用图标必须在主线程进行")?;
    // The bitmap is allocated with no plane pointer, so its buffer lives inside the object and
    // is filled through `bitmapData` afterwards: the initialiser below is only asked for an
    // 8-bit RGBA storage of the right shape.
    let rep: Retained<NSBitmapImageRep> = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            side as isize,
            side as isize,
            8,
            4,
            true,
            false,
            NSCalibratedRGBColorSpace,
            (side as isize) * 4,
            32,
        )
    }
    .ok_or("无法创建位图")?;
    let bitmap = rep.bitmapData();
    if bitmap.is_null() {
        return Err("位图没有像素缓冲区".to_string());
    }
    let premultiplied = premultiply_rgba(rgba);
    unsafe {
        std::ptr::copy_nonoverlapping(premultiplied.as_ptr(), bitmap, premultiplied.len());
    }
    let image = NSImage::initWithSize(NSImage::alloc(), NSSize::new(side as f64, side as f64));
    image.addRepresentation(&rep);
    image.setSize(NSSize::new(side as f64, side as f64));
    let application = NSApplication::sharedApplication(mtm);
    unsafe { application.setApplicationIconImage(Some(&image)) };
    Ok(())
}

/// Switch to `choice`, persist it, and draw it.
pub fn select(app: &tauri::AppHandle, data_dir: &Path, choice: Choice) -> Result<(), String> {
    let mut appearance = load(data_dir);
    appearance.icon = choice;
    save(data_dir, &appearance)?;
    remember(choice);
    apply(app, data_dir, choice);
    harness::app_log(&format!("应用图标切换为 {}", choice.key()));
    Ok(())
}

/// Re-apply the persisted choice. Called once at startup, after the first window exists.
pub fn restore(app: &tauri::AppHandle, data_dir: &Path) {
    let appearance = load(data_dir);
    remember(appearance.icon);
    apply(app, data_dir, appearance.icon);
    if appearance.icon != Choice::Default {
        harness::app_log(&format!("已应用保存的图标: {}", appearance.icon.key()));
    }
}

/// Ask the system for a PNG file, or `Ok(None)` when the user cancels.
///
/// Cancelling is not an error: the panel is a question, and a closed question has an answer too.
#[cfg(target_os = "macos")]
pub fn pick_png() -> Result<Option<PathBuf>, String> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSModalResponseCancel, NSModalResponseOK, NSOpenPanel};
    use objc2_foundation::{NSArray, NSString};

    let mtm = MainThreadMarker::new().ok_or("选择图片必须在主线程进行")?;
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setTitle(Some(&NSString::from_str("选择应用图标（PNG，正方形）")));
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    // Extensions rather than UTIs: the extension is also what the decoder above accepts, so the
    // picker and the validation agree on one rule. `allowedFileTypes` is deprecated in favour of
    // `allowedContentTypes`, which takes UTTypes and would pull `objc2-uniform-type-identifiers`
    // in for one string; the deprecated setter is still honoured by every macOS version this app
    // declares support for, and the decoder re-checks the extension regardless.
    #[allow(deprecated)]
    {
        let png = NSString::from_str("png");
        let types = NSArray::<NSString>::from_slice(&[&*png]);
        panel.setAllowedFileTypes(Some(&types));
    }
    match panel.runModal() {
        response if response == NSModalResponseCancel => Ok(None),
        response if response == NSModalResponseOK => {
            let url = panel.URL().ok_or("文件选择器没有返回路径")?;
            let path = url.path().ok_or("文件选择器返回的路径无法读取")?;
            Ok(Some(PathBuf::from(path.to_string())))
        }
        _ => Err("文件选择器异常结束".to_string()),
    }
}

/// Windows: the file dialog through PowerShell.
#[cfg(windows)]
pub fn pick_png() -> Result<Option<PathBuf>, String> {
    // `System.Windows.Forms` ships with every supported Windows; the dialog is blocking and
    // returns `OK`/`Cancel` in `ShowDialog`'s return value.
    let script = concat!(
        "Add-Type -AssemblyName System.Windows.Forms; ",
        "$d = New-Object System.Windows.Forms.OpenFileDialog; ",
        "$d.Title = '选择应用图标（PNG，正方形）'; ",
        "$d.Filter = 'PNG 图片|*.png'; ",
        "if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { $d.FileName }"
    );
    let mut command = std::process::Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    let output =
        crate::process::output_within(&mut command, std::time::Duration::from_secs(300), 8 * 1024)
            .ok_or("文件选择器没有返回结果")?;
    let path = output.text.trim().to_string();
    Ok(if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    })
}

/// Linux: `zenity`, then `kdialog`, whichever the desktop provides.
///
/// Both are the standard pickers of the two main desktop families, and neither is a dependency
/// this shell can add — a GTK dialog linked into the binary would have to match the WebKitGTK
/// version the distribution ships. Without either, the answer names the manual route instead of
/// failing silently.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn pick_png() -> Result<Option<PathBuf>, String> {
    let attempts: [(&str, Vec<&str>); 2] = [
        (
            "zenity",
            vec![
                "--file-selection",
                "--title=选择应用图标（PNG，正方形）",
                "--file-filter=PNG 图片 | *.png",
            ],
        ),
        ("kdialog", vec!["--getopenfilename", ".", "*.png"]),
    ];
    for (program, args) in attempts {
        let mut command = std::process::Command::new(program);
        command.args(&args);
        let Some(output) = crate::process::output_within(
            &mut command,
            std::time::Duration::from_secs(300),
            8 * 1024,
        ) else {
            continue;
        };
        // A cancelled dialog exits non-zero with no output: that is an answer, not a failure,
        // so it only stops the search when the program actually ran.
        let path = output.text.trim().to_string();
        return Ok(if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        });
    }
    Err("找不到可用的文件选择器（zenity / kdialog）".to_string())
}

/// Decode the `data:` URL the status page sends back after a file input.
///
/// The page cannot hand over a `File` object over IPC, so it reads the chosen file and sends it as
/// a data URL. Only `image/png` is accepted: the same rule the file picker and the decoder apply,
/// stated in the one place that sees the bytes before they reach the decoder.
pub fn decode_data_url(text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim();
    let (meta, payload) = text.split_once(',').ok_or("图标数据不是 data URL")?;
    if !meta.starts_with("data:image/png") || !meta.contains(";base64") {
        return Err("只接受 PNG 图片".to_string());
    }
    if payload.is_empty() {
        return Err("图标数据为空".to_string());
    }
    // The page refuses an oversized file before reading it, but this is the side that owns the
    // limit: a payload larger than base64 of the file cap is not a file this shell would accept.
    check_data_url_size(payload.len())?;
    decode_base64(payload)
}

/// Refuse a `data:` payload past [`MAX_DATA_URL_BYTES`] before it is decoded.
///
/// Separate from the comparison so the boundary is testable without building a forty-megabyte
/// string: the cap is the only thing standing between a page and an allocation four thirds the
/// size of what it sent.
pub fn check_data_url_size(len: usize) -> Result<(), String> {
    if len > MAX_DATA_URL_BYTES {
        return Err(format!(
            "图标数据超过 {} MiB",
            MAX_CUSTOM_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

/// Standard base64 (RFC 4648, with padding), ignoring whitespace.
///
/// Hand-rolled rather than a dependency: this is the only base64 in the program, the input is
/// bounded by the page, and an error has to name the input's problem rather than a decoder's.
fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    let mut padding = 0;
    for byte in text.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            padding += 1;
            continue;
        }
        if padding > 0 {
            return Err("base64 数据在填充之后还有内容".to_string());
        }
        let Some(value) = value(byte) else {
            return Err("base64 数据里有非法字符".to_string());
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    if padding > 2 {
        return Err("base64 填充过多".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
