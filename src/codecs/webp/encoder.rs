//! Encoding of WebP images.
///
/// Uses the simple encoding API from the [libwebp] library.
///
/// [libwebp]: https://developers.google.com/speed/webp/docs/api#simple_encoding_api
use std::io::Write;

use libwebp::{Encoder, PixelLayout, WebPMemory};

use crate::error::{
    EncodingError, ParameterError, ParameterErrorKind, UnsupportedError, UnsupportedErrorKind,
};
use crate::flat::SampleLayout;
use crate::{ColorType, ImageEncoder, ImageError, ImageFormat, ImageResult};

/// WebP Encoder.
pub struct WebPEncoder<W> {
    inner: W,
    quality: WebPQuality,

    chunk_buffer: Vec<u8>,
    buffer: u64,
    nbits: u8,
}

/// WebP encoder quality.
#[derive(Debug, Copy, Clone)]
pub struct WebPQuality(Quality);

#[derive(Debug, Copy, Clone)]
enum Quality {
    Lossless,
    Lossy(u8),
}

impl WebPQuality {
    /// Minimum lossy quality value (0).
    pub const MIN: u8 = 0;
    /// Maximum lossy quality value (100).
    pub const MAX: u8 = 100;
    /// Default lossy quality (80), providing a balance of quality and file size.
    pub const DEFAULT: u8 = 80;

    /// Lossless encoding.
    pub fn lossless() -> Self {
        Self(Quality::Lossless)
    }

    /// Lossy encoding. 0 = low quality, small size; 100 = high quality, large size.
    ///
    /// Values are clamped from 0 to 100.
    pub fn lossy(quality: u8) -> Self {
        Self(Quality::Lossy(quality.clamp(Self::MIN, Self::MAX)))
    }
}

impl Default for WebPQuality {
    fn default() -> Self {
        Self::lossy(WebPQuality::DEFAULT)
    }
}

impl<W: Write> WebPEncoder<W> {
    /// Create a new encoder that writes its output to `w`.
    ///
    /// Defaults to lossy encoding, see [`WebPQuality::DEFAULT`].
    pub fn new(w: W) -> Self {
        WebPEncoder::new_with_quality(w, WebPQuality::default())
    }

    /// Create a new encoder with the specified quality, that writes its output to `w`.
    pub fn new_with_quality(w: W, quality: WebPQuality) -> Self {
        Self {
            inner: w,
            quality,
            chunk_buffer: Vec::new(),
            buffer: 0,
            nbits: 0,
        }
    }

    fn write_bits(&mut self, bits: u64, nbits: u8) -> io::Result<()> {
        debug_assert!(nbits <= 64);

        self.buffer |= bits << self.nbits;
        self.nbits += nbits;

        if self.nbits >= 64 {
            self.chunk_buffer.write_all(&self.buffer.to_le_bytes())?;
            self.nbits -= 64;
            self.buffer = bits.checked_shr((nbits - self.nbits) as u32).unwrap_or(0);
        }
        debug_assert!(self.nbits < 64);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.nbits % 8 != 0 {
            self.write_bits(0, 8 - self.nbits % 8)?;
        }
        if self.nbits > 0 {
            self.chunk_buffer
                .write_all(&self.buffer.to_le_bytes()[..self.nbits as usize / 8])
                .unwrap();
            self.buffer = 0;
            self.nbits = 0;
        }
        Ok(())
    }

    fn write_single_entry_huffman_tree(&mut self, symbol: u8) -> io::Result<()> {
        self.write_bits(1, 2)?;
        if symbol <= 1 {
            self.write_bits(0, 1)?;
            self.write_bits(symbol as u64, 1)?;
        } else {
            self.write_bits(1, 1)?;
            self.write_bits(symbol as u64, 8)?;
        }
        Ok(())
    }

    fn write_flat_huffman_tree(&mut self) -> io::Result<()> {
        self.write_bits(0, 1)?; // normal huffman tree
        self.write_bits(8, 4)?; // num_code_lengths - 4

        // code_length_code_lengths = [0, 0, 0, 0, 0, 0, 0, 0, 1]
        for _ in 0..11 {
            self.write_bits(0, 3)?;
        }
        self.write_bits(1, 3)?;

        // max_symbol = 256
        self.write_bits(1, 1)?;
        self.write_bits(3, 3)?;
        self.write_bits(254, 8)?;

        Ok(())
    }

    fn encode_lossless(mut self, data: &[u8], width: u32, height: u32) -> ImageResult<()> {
        if width == 0
            || width > 16383
            || height == 0
            || height > 16383
            || !SampleLayout::row_major_packed(color.channel_count(), width, height)
                .fits(data.len())
        {
            return Err(ImageError::Parameter(ParameterError::from_kind(
                ParameterErrorKind::DimensionMismatch,
            )));
        }

        let (is_color, is_alpha) = match color_type {
            ColorType::L8 => (false, false),
            ColorType::La8 => (false, true),
            ColorType::Rgb8 => (true, false),
            ColorType::Rgba8 => (true, true),
            _ => {
                return Err(ImageError::Unsupported(
                    UnsupportedError::from_format_and_kind(
                        ImageFormat::WebP.into(),
                        UnsupportedErrorKind::Color(color_type.into()),
                    ),
                ))
            }
        };

        self.write_bits(0x2f, 8)?; // signature
        self.write_bits(width as u64 - 1, 14)?;
        self.write_bits(height as u64 - 1, 14)?;

        self.write_bits(is_alpha as u64, 1)?; // alpha used
        self.write_bits(0x0, 3)?; // version

        // transforms
        if !is_color {
            self.write_bits(0b101, 3)?;
        }
        self.write_bits(0x0, 1)?;

        // color cache
        self.write_bits(0x0, 1)?;

        // meta-huffman codes
        self.write_bits(0x0, 1)?;

        // huffman codes
        self.write_flat_huffman_tree()?;
        if is_color {
            self.write_flat_huffman_tree()?;
            self.write_flat_huffman_tree()?;
        } else {
            self.write_single_entry_huffman_tree(0)?;
            self.write_single_entry_huffman_tree(0)?;
        }
        if is_alpha {
            self.write_flat_huffman_tree()?;
        } else {
            self.write_single_entry_huffman_tree(255)?;
        }
        self.write_single_entry_huffman_tree(0)?;

        // image data
        match color_type {
            ColorType::L8 => {
                for &pixel in buf {
                    self.write_bits(pixel.reverse_bits() as u64, 8)?;
                }
            }
            ColorType::La8 => {
                for pixel in buf.chunks_exact(2) {
                    self.write_bits(pixel[0].reverse_bits() as u64, 8)?;
                    self.write_bits(pixel[1].reverse_bits() as u64, 8)?;
                }
            }
            ColorType::Rgb8 => {
                for pixel in buf.chunks_exact(3) {
                    self.write_bits(pixel[1].reverse_bits() as u64, 8)?;
                    self.write_bits(pixel[0].reverse_bits() as u64, 8)?;
                    self.write_bits(pixel[2].reverse_bits() as u64, 8)?;
                }
            }
            ColorType::Rgba8 => {
                for pixel in buf.chunks_exact(4) {
                    self.write_bits(pixel[1].reverse_bits() as u64, 8)?;
                    self.write_bits(pixel[0].reverse_bits() as u64, 8)?;
                    self.write_bits(pixel[2].reverse_bits() as u64, 8)?;
                    self.write_bits(pixel[3].reverse_bits() as u64, 8)?;
                }
            }
            _ => unreachable!(),
        }

        self.flush()?;
        if self.chunk_buffer.len() % 2 == 1 {
            self.chunk_buffer.push(0);
        }

        self.writer.write_all(b"RIFF")?;
        self.writer
            .write_all(&(self.chunk_buffer.len() as u32 + 12).to_le_bytes())?;
        self.writer.write_all(b"WEBP")?;
        self.writer.write_all(b"VP8L")?;
        self.writer
            .write_all(&(self.chunk_buffer.len() as u32).to_le_bytes())?;
        self.writer.write_all(&self.chunk_buffer)?;

        Ok(())
    }

    /// Encode image data with the indicated color type.
    ///
    /// The encoder requires image data be Rgb8 or Rgba8.
    pub fn encode(
        mut self,
        data: &[u8],
        width: u32,
        height: u32,
        color: ColorType,
    ) -> ImageResult<()> {
        if let Quality::Lossless = self.quality {
            return self.encode_lossless(data, width, height);
        }

        // TODO: convert color types internally?
        let layout = match color {
            ColorType::Rgb8 => PixelLayout::Rgb,
            ColorType::Rgba8 => PixelLayout::Rgba,
            _ => {
                return Err(ImageError::Unsupported(
                    UnsupportedError::from_format_and_kind(
                        ImageFormat::WebP.into(),
                        UnsupportedErrorKind::Color(color.into()),
                    ),
                ))
            }
        };

        // Validate dimensions upfront to avoid panics.
        if width == 0
            || height == 0
            || !SampleLayout::row_major_packed(color.channel_count(), width, height)
                .fits(data.len())
        {
            return Err(ImageError::Parameter(ParameterError::from_kind(
                ParameterErrorKind::DimensionMismatch,
            )));
        }

        // Call the native libwebp library to encode the image.
        let encoder = Encoder::new(data, layout, width, height);
        let encoded: WebPMemory = match self.quality.0 {
            Quality::Lossless => encoder.encode_lossless(),
            Quality::Lossy(quality) => encoder.encode(quality as f32),
        };

        // The simple encoding API in libwebp does not return errors.
        if encoded.is_empty() {
            return Err(ImageError::Encoding(EncodingError::new(
                ImageFormat::WebP.into(),
                "encoding failed, output empty",
            )));
        }

        self.inner.write_all(&encoded)?;
        Ok(())
    }
}

impl<W: Write> ImageEncoder for WebPEncoder<W> {
    fn write_image(
        self,
        buf: &[u8],
        width: u32,
        height: u32,
        color_type: ColorType,
    ) -> ImageResult<()> {
        self.encode(buf, width, height, color_type)
    }
}

#[cfg(test)]
mod tests {
    use crate::codecs::webp::{WebPEncoder, WebPQuality};
    use crate::{ColorType, ImageEncoder};

    #[test]
    fn write_webp() {
        let img = crate::open("/home/jonathan/git/image/tests/images/tiff/testsuite/rgb-3c-16b.tiff").unwrap().to_rgba8();

        let mut output = Vec::new();
        super::WebpEncoder::new(&mut output)
            .write_image(&img.inner_pixels(), img.width(), img.height(), crate::ColorType::Rgba8)
            .unwrap();

        crate::load_from_memory_with_format(&output, crate::ImageFormat::WebP).unwrap();

        std::fs::write("test.webp", output).unwrap();
    }

    #[test]
    fn webp_lossless_deterministic() {
        // 1x1 8-bit image buffer containing a single red pixel.
        let rgb: &[u8] = &[255, 0, 0];
        let rgba: &[u8] = &[255, 0, 0, 128];
        for (color, img, expected) in [
            (
                ColorType::Rgb8,
                rgb,
                [
                    82, 73, 70, 70, 28, 0, 0, 0, 87, 69, 66, 80, 86, 80, 56, 76, 15, 0, 0, 0, 47,
                    0, 0, 0, 0, 7, 16, 253, 143, 254, 7, 34, 162, 255, 1, 0,
                ],
            ),
            (
                ColorType::Rgba8,
                rgba,
                [
                    82, 73, 70, 70, 28, 0, 0, 0, 87, 69, 66, 80, 86, 80, 56, 76, 15, 0, 0, 0, 47,
                    0, 0, 0, 16, 7, 16, 253, 143, 2, 6, 34, 162, 255, 1, 0,
                ],
            ),
        ] {
            // Encode it into a memory buffer.
            let mut encoded_img = Vec::new();
            {
                let encoder =
                    WebPEncoder::new_with_quality(&mut encoded_img, WebPQuality::lossless());
                encoder
                    .write_image(&img, 1, 1, color)
                    .expect("image encoding failed");
            }

            // WebP encoding should be deterministic.
            assert_eq!(encoded_img, expected);
        }
    }

    #[derive(Debug, Clone)]
    struct MockImage {
        width: u32,
        height: u32,
        color: ColorType,
        data: Vec<u8>,
    }

    impl quickcheck::Arbitrary for MockImage {
        fn arbitrary(g: &mut quickcheck::Gen) -> Self {
            // Limit to small, non-empty images <= 512x512.
            let width = u32::arbitrary(g) % 512 + 1;
            let height = u32::arbitrary(g) % 512 + 1;
            let (color, stride) = if bool::arbitrary(g) {
                (ColorType::Rgb8, 3)
            } else {
                (ColorType::Rgba8, 4)
            };
            let size = width * height * stride;
            let data: Vec<u8> = (0..size).map(|_| u8::arbitrary(g)).collect();
            MockImage {
                width,
                height,
                color,
                data,
            }
        }
    }

    quickcheck! {
        fn fuzz_webp_valid_image(image: MockImage, quality: u8) -> bool {
            // Check valid images do not panic.
            let mut buffer = Vec::<u8>::new();
            for webp_quality in [WebPQuality::lossless(), WebPQuality::lossy(quality)] {
                buffer.clear();
                let encoder = WebPEncoder::new_with_quality(&mut buffer, webp_quality);
                if !encoder
                    .write_image(&image.data, image.width, image.height, image.color)
                    .is_ok() {
                    return false;
                }
            }
            true
        }

        fn fuzz_webp_no_panic(data: Vec<u8>, width: u8, height: u8, quality: u8) -> bool {
            // Check random (usually invalid) parameters do not panic.
            let mut buffer = Vec::<u8>::new();
            for color in [ColorType::Rgb8, ColorType::Rgba8] {
                for webp_quality in [WebPQuality::lossless(), WebPQuality::lossy(quality)] {
                    buffer.clear();
                    let encoder = WebPEncoder::new_with_quality(&mut buffer, webp_quality);
                    // Ignore errors.
                    let _ = encoder
                        .write_image(&data, width as u32, height as u32, color);
                }
            }
            true
        }
    }
}
