use super::{Engine, SpecTransform};
use crate::pb::*;
use anyhow::Result;
use bytes::Bytes;
use image::{DynamicImage, ImageBuffer, ImageFormat};
use photon_rs::{
    PhotonImage, effects, filters, multiple, native::open_image_from_bytes, transform,
};
use std::convert::TryFrom;
use std::io::Cursor;
use std::sync::OnceLock;
use tracing::warn;

static WATERMARK: OnceLock<Option<PhotonImage>> = OnceLock::new();

pub struct Photon(PhotonImage);

impl TryFrom<Bytes> for Photon {
    type Error = anyhow::Error;

    fn try_from(bytes: Bytes) -> Result<Self, Self::Error> {
        let image = open_image_from_bytes(&bytes)?;
        Ok(Photon(image))
    }
}

impl Engine for Photon {
    fn apply(&mut self, spec: &[Spec]) {
        for s in spec {
            match s.data {
                Some(spec::Data::Resize(ref v)) => self.transform(v),
                Some(spec::Data::Crop(ref v)) => self.transform(v),
                Some(spec::Data::Contrast(ref v)) => self.transform(v),
                Some(spec::Data::Filter(ref v)) => self.transform(v),
                Some(spec::Data::Fliph(ref v)) => self.transform(v),
                Some(spec::Data::Flipv(ref v)) => self.transform(v),
                Some(spec::Data::Watermark(ref v)) => self.transform(v),
                None => {}
            }
        }
    }

    fn generate(self, format: ImageFormat) -> Vec<u8> {
        image_to_buf(self.0, format)
    }
}

impl SpecTransform<&Resize> for Photon {
    fn transform(&mut self, op: &Resize) {
        let resize_type = resize::ResizeType::try_from(op.rtype)
            .unwrap_or(resize::ResizeType::Normal);
        let filter = resize::SampleFilter::try_from(op.filter)
            .unwrap_or(resize::SampleFilter::Nearest)
            .into();

        let image = match resize_type {
            resize::ResizeType::Normal => transform::resize(&self.0, op.width, op.height, filter),
            resize::ResizeType::SeamCarve => transform::seam_carve(&self.0, op.width, op.height),
        };
        self.0 = image;
    }
}

impl SpecTransform<&Crop> for Photon {
    fn transform(&mut self, op: &Crop) {
        let img = transform::crop(&self.0, op.x1, op.y1, op.x2, op.y2);
        self.0 = img;
    }
}
impl SpecTransform<&Contrast> for Photon {
    fn transform(&mut self, op: &Contrast) {
        effects::adjust_contrast(&mut self.0, op.contrast);
    }
}
impl SpecTransform<&Flipv> for Photon {
    fn transform(&mut self, _op: &Flipv) {
        transform::flipv(&mut self.0)
    }
}
impl SpecTransform<&Fliph> for Photon {
    fn transform(&mut self, _op: &Fliph) {
        transform::fliph(&mut self.0)
    }
}
impl SpecTransform<&Filter> for Photon {
    fn transform(&mut self, op: &Filter) {
        match filter::Filter::try_from(op.filter) {
            Ok(filter::Filter::Unspecified) => {}
            Ok(f) => {
                if let Some(name) = f.to_str() {
                    filters::filter(&mut self.0, name);
                }
            }
            Err(_) => {}
        }
    }
}

impl SpecTransform<&Watermark> for Photon {
    fn transform(&mut self, op: &Watermark) {
        if let Some(watermark) = watermark() {
            multiple::watermark(&mut self.0, watermark, i64::from(op.x), i64::from(op.y));
        }
    }
}

fn watermark() -> Option<&'static PhotonImage> {
    WATERMARK
        .get_or_init(|| {
            let data = include_bytes!("../../rust-logo.png");
            match open_image_from_bytes(data) {
                Ok(image) => Some(transform::resize(
                    &image,
                    64,
                    64,
                    transform::SamplingFilter::Nearest,
                )),
                Err(err) => {
                    warn!("failed to load watermark image, watermark transform will be skipped: {err}");
                    None
                }
            }
        })
        .as_ref()
}

fn image_to_buf(image: PhotonImage, format: ImageFormat) -> Vec<u8> {
    let raw_pixels = image.get_raw_pixels();
    let width = image.get_width();
    let height = image.get_height();
    let img_buffer = ImageBuffer::from_vec(width, height, raw_pixels).unwrap();
    let dynamic_image = DynamicImage::ImageRgba8(img_buffer);
    let mut cursor = Cursor::new(Vec::new());
    dynamic_image.write_to(&mut cursor, format).unwrap();
    cursor.into_inner()
}
