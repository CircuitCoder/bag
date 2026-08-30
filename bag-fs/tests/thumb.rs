use std::io::Cursor;

use bag_fs::thumb::extract_thumbnail_img;
use image::{DynamicImage, ImageFormat};

#[tokio::test]
async fn extracts_image_thumbnail_from_buffered_reader() {
    let mut source = Cursor::new(Vec::new());
    DynamicImage::new_rgb8(4, 2)
        .write_to(&mut source, ImageFormat::Png)
        .unwrap();
    source.set_position(0);

    let thumbnail = extract_thumbnail_img(source, 2).await.unwrap();
    assert_eq!(image::guess_format(&thumbnail).unwrap(), ImageFormat::WebP);
}
