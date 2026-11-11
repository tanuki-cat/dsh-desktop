//! Tests for the icon choice.
//!
//! The presets are the real files the binary embeds, so a broken regeneration fails here rather
//! than at the first launch of a build.

use super::*;
use crate::test_dir;

/// The fixtures are real PNGs built through the encoder, because the decoder enforces a minimum
/// size that hand-written byte arrays would have to be padded to.
fn square_png(side: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, side, side);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let mut data = Vec::with_capacity((side as usize) * (side as usize) * 4);
        for _ in 0..(side as usize) * (side as usize) {
            data.extend_from_slice(&rgba);
        }
        writer.write_image_data(&data).unwrap();
    }
    out
}

/// A PNG built from a per-pixel colour function, so a test can say which part of an image a crop
/// or a scale kept.
fn patterned_png(width: u32, height: u32, color: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&color(x, y));
        }
    }
    encoded_png(
        width,
        height,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &pixels,
    )
}

/// A PNG in any colour type and depth the encoder accepts, for the decoder-expansion cases.
fn encoded_png(
    width: u32,
    height: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(data).unwrap();
    }
    out
}

/// The presets ship inside the binary, so this is the check that they were generated at all and
/// that each decodes to the shape the platform APIs require.
#[test]
fn every_preset_is_a_square_png_the_decoder_accepts() {
    for choice in Choice::PRESETS {
        let bytes = choice.bytes().expect("a preset always has built-in bytes");
        let image = decode_png(bytes).unwrap_or_else(|error| panic!("{}: {error}", choice.key()));
        assert_eq!(
            image.side(),
            Some(image.width),
            "{} is not square",
            choice.key()
        );
        assert!(
            image.width >= 128,
            "{} is too small to look like an icon: {}",
            choice.key(),
            image.width
        );
        assert_eq!(
            image.pixels.len(),
            (image.width as usize) * (image.width as usize) * 4,
            "{} does not decode to RGBA",
            choice.key()
        );
    }
}

/// The presets have to differ, or the menu offers entries that all look the same.
///
/// Compared through the encoded bytes rather than the decoded pixels: the default artwork is
/// 1024x1024 and the generated presets are 512x512 (they are embedded, so size is binary size),
/// so the two buffers are not comparable element by element.
#[test]
fn the_presets_are_distinct_images() {
    let default = Choice::Default.bytes().unwrap();
    let mut seen: Vec<&[u8]> = vec![default];
    let mut widths = Vec::new();
    for choice in [Choice::Deep, Choice::Night, Choice::Dawn] {
        let bytes = choice.bytes().unwrap();
        assert!(
            !seen.contains(&bytes),
            "{} duplicates another preset",
            choice.key()
        );
        let image = decode_png(bytes).unwrap();
        widths.push(image.width);
        seen.push(bytes);
    }
    assert!(
        widths.windows(2).all(|pair| pair[0] == pair[1]),
        "the generated presets keep one size: {widths:?}"
    );
}

/// A non-square image is center-cropped rather than refused.
///
/// Screenshots and portraits are what people pick. "An icon must be square" is a rule about a
/// format they never chose, and enforcing it made the menu item look broken: the first three
/// real attempts at this feature were a 2886x1338 and a 1920x1080 screenshot, both refused with
/// nothing on screen but a log line.
#[test]
fn a_non_square_image_is_cropped_to_the_center() {
    // 180x100: red on the left half, blue on the right.
    let png = patterned_png(180, 100, |x, _| {
        if x < 90 {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        }
    });
    let icon = icon_rgba(&png).unwrap();
    assert_eq!((icon.width, icon.height), (100, 100));
    let color = |x: u32, y: u32| {
        let at = ((y * icon.width + x) * 4) as usize;
        [
            icon.pixels[at],
            icon.pixels[at + 1],
            icon.pixels[at + 2],
            icon.pixels[at + 3],
        ]
    };
    // The crop starts at x = (180 - 100) / 2 = 40, so the source's split at 90 lands at 50.
    assert_eq!(color(0, 0), [255, 0, 0, 255]);
    assert_eq!(color(49, 50), [255, 0, 0, 255]);
    assert_eq!(color(50, 50), [0, 0, 255, 255]);
    assert_eq!(color(99, 99), [0, 0, 255, 255]);
}

/// A source larger than the stored size is scaled down instead of refused.
///
/// 1254 px artwork is an ordinary icon; the Dock never draws more than 512 physical pixels of one
/// tile, and the old ceiling of 1024 rejected the file with only a log line to show for it.
#[test]
fn an_oversized_image_is_scaled_down_to_the_limit() {
    let png = patterned_png(1280, 1280, |x, y| {
        if (x < 640) == (y < 640) {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        }
    });
    let icon = icon_rgba(&png).unwrap();
    assert_eq!(
        (icon.width, icon.height),
        (MAX_CUSTOM_SIDE, MAX_CUSTOM_SIDE)
    );
    assert_eq!(
        icon.pixels.len(),
        (MAX_CUSTOM_SIDE * MAX_CUSTOM_SIDE * 4) as usize
    );
    // The four quadrants keep their colours through the box filter, so the scale is not a blur
    // into one average.
    let color = |x: u32, y: u32| {
        let at = ((y * icon.width + x) * 4) as usize;
        [icon.pixels[at], icon.pixels[at + 1], icon.pixels[at + 2]]
    };
    assert_eq!(color(10, 10), [255, 0, 0]);
    assert_eq!(color(10, MAX_CUSTOM_SIDE - 10), [0, 0, 255]);
    assert_eq!(color(MAX_CUSTOM_SIDE - 10, 10), [0, 0, 255]);
    assert_eq!(
        color(MAX_CUSTOM_SIDE - 10, MAX_CUSTOM_SIDE - 10),
        [255, 0, 0]
    );
}

/// The one refusal left: an image with no detail to scale is a smear at every size the OS draws.
#[test]
fn an_image_too_small_to_draw_is_refused() {
    let error = icon_rgba(&square_png(MIN_CUSTOM_SIDE - 1, [0, 0, 0, 255])).unwrap_err();
    assert!(error.contains("太小"), "{error}");
    assert!(icon_rgba(&square_png(MIN_CUSTOM_SIDE, [1, 2, 3, 255])).is_ok());
}

/// The header is judged before a single pixel is expanded: a huge PNG is a few hundred bytes on
/// disk and gigabytes in memory.
#[test]
fn an_absurd_header_is_refused_before_it_is_expanded() {
    let error = check_dimensions(60_000, 60_000).unwrap_err();
    assert!(error.contains("太大"), "{error}");
    assert!(check_dimensions(0, 10).is_err());
    assert!(check_dimensions(MAX_DECODE_SIDE, MAX_DECODE_SIDE).is_ok());
}

/// Palette, grayscale and 16-bit PNGs are ordinary images and are expanded by the decoder.
///
/// Asking the user to "另存为 8 位 RGB/RGBA" turned a common, perfectly drawable file into an
/// error message about encoder settings.
#[test]
fn grayscale_and_sixteen_bit_sources_are_expanded() {
    let gray = encoded_png(
        64,
        64,
        png::ColorType::Grayscale,
        png::BitDepth::Eight,
        &vec![128u8; 64 * 64],
    );
    assert_eq!(icon_rgba(&gray).unwrap().pixels[..4], [128, 128, 128, 255]);

    let mut wide = Vec::new();
    for _ in 0..64 * 64 {
        wide.extend_from_slice(&u16::MAX.to_be_bytes());
    }
    let sixteen = encoded_png(
        64,
        64,
        png::ColorType::Grayscale,
        png::BitDepth::Sixteen,
        &wide,
    );
    assert_eq!(
        icon_rgba(&sixteen).unwrap().pixels[..4],
        [255, 255, 255, 255]
    );
}

/// Something that is not a PNG at all must fail with a message, not a panic.
#[test]
fn garbage_is_not_a_png() {
    assert!(decode_png(b"not an image").is_err());
    assert!(decode_png(&[]).is_err());
}

/// A copied icon is the stored copy from then on: deleting the source must not change what the
/// shell draws.
/// What is stored is the fitted icon, not the file the user picked.
///
/// The reported failure was a 1254 px square being refused by the 1024 ceiling. It is now scaled
/// once, at adoption, so neither the launch path nor the stored file carries the original size.
#[test]
fn the_stored_copy_is_the_fitted_icon() {
    let dir = test_dir("icon-store-fitted");
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("wide.png");
    std::fs::write(&source, patterned_png(300, 120, |_, _| [10, 20, 30, 255])).unwrap();
    let stored = adopt_custom(&dir, &source).unwrap();
    let written = icon_rgba(&std::fs::read(&stored).unwrap()).unwrap();
    assert_eq!((written.width, written.height), (120, 120));
    assert_eq!(written.pixels[..4], [10, 20, 30, 255]);
    // The user's own file is only read: it stays exactly where it was.
    assert!(source.is_file());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_custom_icon_is_copied_rather_than_referenced() {
    let dir = test_dir("icon-custom-copy");
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("picked.png");
    std::fs::write(&source, square_png(128, [10, 20, 30, 255])).unwrap();
    let stored = adopt_custom(&dir, &source).unwrap();
    assert_eq!(stored, custom_path(&dir));
    assert!(stored.is_file());
    std::fs::remove_file(&source).unwrap();
    let bytes = bytes_for(&dir, Choice::Custom).unwrap();
    assert!(
        !bytes.is_empty(),
        "the stored copy is what the icon path reads"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An image the decoder refuses must not be copied: the store would otherwise hold something the
/// drawing path has to reject later, far from where the choice was made.
#[test]
fn an_unusable_custom_image_is_not_stored() {
    let dir = test_dir("icon-custom-invalid");
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("bad.png");
    std::fs::write(&source, b"definitely not a png").unwrap();
    assert!(adopt_custom(&dir, &source).is_err());
    assert!(!custom_path(&dir).exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The persisted choice round-trips, and a missing file means the default rather than an error.
#[test]
fn the_choice_round_trips_through_appearance_json() {
    let dir = test_dir("icon-appearance");
    assert_eq!(load(&dir).icon, Choice::Default);
    for choice in [Choice::Night, Choice::Custom, Choice::Default] {
        save(&dir, &Appearance { icon: choice }).unwrap();
        assert_eq!(load(&dir).icon, choice);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A broken file is not overwritten: this run adapts, the file stays as the user left it.
#[test]
fn an_unreadable_appearance_file_falls_back_without_rewriting() {
    let dir = test_dir("icon-appearance-broken");
    std::fs::create_dir_all(&dir).unwrap();
    let file = path(&dir);
    std::fs::write(&file, b"{ not json").unwrap();
    assert_eq!(load(&dir).icon, Choice::Default);
    assert_eq!(std::fs::read(&file).unwrap(), b"{ not json");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Choice keys are the on-disk values and the menu id suffixes, so every one has to parse back
/// to itself.
#[test]
fn choice_keys_round_trip() {
    for choice in Choice::PRESETS.into_iter().chain([Choice::Custom]) {
        assert_eq!(Choice::parse(choice.key()), Some(choice));
        assert_eq!(Choice::parse(&choice.key().to_uppercase()), Some(choice));
        assert!(!choice.label().is_empty());
    }
    assert_eq!(Choice::parse("sparkle"), None);
}

/// The page sends a data URL; only PNG is accepted, and whitespace inside the body (some
/// browsers wrap it) is tolerated.
#[test]
fn a_png_data_url_decodes_and_everything_else_is_refused() {
    let png = square_png(64, [255, 0, 0, 255]);
    let encoded = encode_base64(&png);
    assert_eq!(
        decode_data_url(&format!("data:image/png;base64,{encoded}")).unwrap(),
        png
    );
    assert!(decode_data_url(&format!("data:image/png;base64,{encoded}\n")).is_ok());
    assert!(decode_data_url("data:image/jpeg;base64,AAAA").is_err());
    assert!(decode_data_url("data:image/png,notbase64").is_err());
    assert!(decode_data_url("").is_err());
}

/// The decoded icon is straight (non-premultiplied) RGBA, and the Dock bitmap wants premultiplied:
/// without the conversion a semi-transparent pixel keeps its full colour and reads as a pale
/// fringe around the artwork.
#[test]
fn the_dock_bitmap_gets_premultiplied_alpha() {
    // Opaque pixels are left exactly as they are.
    assert_eq!(
        premultiply_rgba(&[10, 20, 30, 255, 255, 255, 255, 255]),
        vec![10, 20, 30, 255, 255, 255, 255, 255]
    );
    // Half alpha halves the colour; the alpha channel itself is never touched.
    assert_eq!(premultiply_rgba(&[200, 100, 0, 128]), vec![100, 50, 0, 128]);
    // Fully transparent carries no colour, whatever the source said.
    assert_eq!(premultiply_rgba(&[200, 100, 0, 0]), vec![0, 0, 0, 0]);
    // 255 stays exact rather than off by one from the rounding term.
    assert_eq!(
        premultiply_rgba(&[255, 255, 255, 255]),
        vec![255, 255, 255, 255]
    );
    // Nothing to do, and no panic, for an empty buffer.
    assert!(premultiply_rgba(&[]).is_empty());
}

/// The data URL is refused before it is decoded: the page caps the file it reads, and this is the
/// side that owns the limit if a page ever sends more.
#[test]
fn an_oversized_data_url_is_refused_before_decoding() {
    // The boundary itself, without building the forty megabytes it describes.
    assert!(check_data_url_size(MAX_DATA_URL_BYTES).is_ok());
    let error = check_data_url_size(MAX_DATA_URL_BYTES + 1).unwrap_err();
    assert!(error.contains("MiB"), "{error}");
    // And `decode_data_url` applies exactly that check to what the page sent.
    let oversized = format!(
        "data:image/png;base64,{}",
        "A".repeat(MAX_DATA_URL_BYTES + 8)
    );
    assert!(decode_data_url(&oversized).is_err());
    // A normal payload is not touched by it.
    assert!(decode_data_url("data:image/png;base64,AAAA").is_ok());
}

/// Base64 with padding, which is the one shape `readAsDataURL` produces.
#[test]
fn base64_handles_every_padding_length() {
    assert_eq!(decode_base64("").unwrap(), Vec::<u8>::new());
    assert_eq!(decode_base64("YQ==").unwrap(), b"a".to_vec());
    assert_eq!(decode_base64("YWI=").unwrap(), b"ab".to_vec());
    assert_eq!(decode_base64("YWJj").unwrap(), b"abc".to_vec());
    assert!(decode_base64("YW=j").is_err());
    assert!(decode_base64("!!!!").is_err());
}

/// A test-side encoder, so the decode test covers the padding cases without a dependency added
/// only for tests.
fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
