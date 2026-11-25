//! Reads a WireGuard config out of a QR code picture (a screenshot or photo
//! of the code, as configs are often shared).

use anyhow::{bail, Context, Result};
use image::imageops::FilterType;
use image::GrayImage;

/// Camera photos are far bigger than a QR code needs; detection is faster
/// and no less reliable on a downscaled copy.
const WORK_SIZE: u32 = 1600;

pub fn decode(bytes: &[u8]) -> Result<String> {
    let img = image::load_from_memory(bytes).context("this picture can't be opened")?.to_luma8();
    let small = (img.width().max(img.height()) > WORK_SIZE).then(|| {
        let k = WORK_SIZE as f32 / img.width().max(img.height()) as f32;
        image::imageops::resize(&img, (img.width() as f32 * k) as u32, (img.height() as f32 * k) as u32, FilterType::Triangle)
    });
    for candidate in small.iter().chain(std::iter::once(&img)) {
        if let Some(text) = scan(candidate) {
            return Ok(text);
        }
    }
    bail!("no QR code found in this picture")
}

fn scan(img: &GrayImage) -> Option<String> {
    let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(img.width() as usize, img.height() as usize, |x, y| {
        img.get_pixel(x as u32, y as u32).0[0]
    });
    prepared.detect_grids().into_iter().find_map(|g| g.decode().ok().map(|(_, text)| text))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_a_config_from_a_png() {
        let conf = "[Interface]\nPrivateKey = yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=\nAddress = 10.0.0.2/32\n\n\
                    [Peer]\nPublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\nAllowedIPs = 0.0.0.0/0\nEndpoint = 203.0.113.1:51820\n";
        let code = qrcode::QrCode::new(conf).unwrap();
        // big, like a phone screenshot, so the downscaled pass is exercised
        let img = code.render::<image::Luma<u8>>().min_dimensions(2400, 2400).build();
        let mut png = Vec::new();
        image::DynamicImage::ImageLuma8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        assert_eq!(super::decode(&png).unwrap(), conf);
    }

    #[test]
    fn rejects_non_images() {
        assert!(super::decode(b"[Interface]").is_err());
    }
}
