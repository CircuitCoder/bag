use ffmpeg_next as ffmpeg;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::software::scaling::{context::Context as Scaler, flag::Flags};
use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};
use std::io::Cursor;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ThumbnailError {
    #[error("FFmpeg error: {0}")]
    Ffmpeg(#[from] ffmpeg::Error),
    #[error("Image processing error: {0}")]
    Image(#[from] image::ImageError),
    #[error("Panic / canceled: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("General error: {0}")]
    General(String),
}

pub async fn extract_thumbnail_img(img: PathBuf, max_dim: u32) -> Result<Vec<u8>, ThumbnailError> {
    tokio::task::spawn_blocking(move || {
        let img = image::open(img)?;
        let mut cursor = Cursor::new(Vec::new());
        img.resize(max_dim, max_dim, image::imageops::FilterType::Lanczos3)
            .write_to(&mut cursor, ImageFormat::WebP)?;

        Ok(cursor.into_inner())
    })
    .await?
}

/// Extracts a thumbnail from a video file and returns raw encoded bytes.
/// Outputs WebP format
pub async fn extract_thumbnail_video(
    video: PathBuf,
    max_dim: u32,
) -> Result<Vec<u8>, ThumbnailError> {
    tokio::task::spawn_blocking(move || {
        // Initialize FFmpeg (safe to call multiple times, but required at least once)
        ffmpeg::init()?;

        // Open the input file
        let mut ictx = ffmpeg::format::input(&video)?;

        // Find the best video stream
        let stream = ictx
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| ThumbnailError::General("No video stream found".to_string()))?;
        let video_stream_index = stream.index();

        // Initialize the decoder
        let context_decoder =
            ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
        let mut decoder = context_decoder.decoder().video()?;

        // Setup the software scaler to convert YUV (or whatever the source is) to RGB24
        let mut scaler = Scaler::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            Pixel::RGB24,
            decoder.width(),
            decoder.height(),
            Flags::BILINEAR,
        )?;

        let mut decoded = ffmpeg::frame::Video::empty();
        let mut rgb_frame = ffmpeg::frame::Video::empty();

        // Read packets from the file
        for (stream, packet) in ictx.packets() {
            if stream.index() == video_stream_index {
                // Send the packet to the decoder
                decoder.send_packet(&packet)?;

                // Attempt to receive a decoded frame
                if decoder.receive_frame(&mut decoded).is_ok() {
                    // Frame successfully decoded! Scale/convert it to RGB
                    scaler.run(&decoded, &mut rgb_frame)?;

                    let width = rgb_frame.width();
                    let height = rgb_frame.height();
                    let data = rgb_frame.data(0);
                    let stride = rgb_frame.stride(0);

                    // FFmpeg frames might have padding (stride != width * 3).
                    // We must strip the padding to pass raw RGB data to the image crate.
                    let mut raw_pixels = Vec::with_capacity((width * height * 3) as usize);
                    for y in 0..height {
                        let start = (y as usize) * stride;
                        let end = start + (width as usize) * 3;
                        raw_pixels.extend_from_slice(&data[start..end]);
                    }

                    // Convert to an `image` crate buffer
                    let img_buffer = ImageBuffer::<Rgb<u8>, _>::from_raw(width, height, raw_pixels)
                        .ok_or_else(|| {
                            ThumbnailError::General(
                                "Failed to reconstruct image buffer from raw pixels".to_owned(),
                            )
                        })?;
                    let dynamic_img = DynamicImage::ImageRgb8(img_buffer);
                    let resized =
                        dynamic_img.resize(max_dim, max_dim, image::imageops::FilterType::Lanczos3);

                    // Encode into an in-memory byte vector
                    let mut cursor = Cursor::new(Vec::new());
                    resized.write_to(&mut cursor, ImageFormat::WebP)?;

                    return Ok(cursor.into_inner());
                }
            }
        }

        Err(ThumbnailError::General(
            "Reached end of file without successfully decoding a video frame.".to_owned(),
        ))
    })
    .await?
}
