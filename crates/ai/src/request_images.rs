//! How the wires write an image, and the note they write instead when the
//! model takes none.

use crate::types::Image;

/// The text a wire writes in place of an image when the model takes none, so
/// an image is never dropped in silence.
pub(crate) const NO_IMAGES: &str = "[image not sent: the model takes no images]";

/// The `data:` URL of `image`, which the OpenAI wires read.
///
/// The base64 text of the image is the text the image was read with; this
/// holds only the media type around it.
pub(crate) fn data_url(image: &Image) -> String {
    let mime = image.media_type.mime();
    let mut url = String::with_capacity("data:;base64,".len() + mime.len() + image.data.len());
    url.push_str("data:");
    url.push_str(mime);
    url.push_str(";base64,");
    url.push_str(&image.data);
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ImageType;

    fn image() -> Image {
        Image {
            media_type: ImageType::Jpeg,
            width: 2,
            height: 3,
            data: "aGk=".into(),
        }
    }

    #[test]
    fn an_image_data_url_names_the_media_type_and_the_base64_text() {
        assert_eq!(data_url(&image()), "data:image/jpeg;base64,aGk=");
        assert_eq!(
            NO_IMAGES, "[image not sent: the model takes no images]",
            "the note the wires share"
        );
    }
}
