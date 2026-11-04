//! Validation of public image data URLs and model-defined image capacity.

use std::io::Cursor;

use anyhow::anyhow;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use dotcore_proto::dotcore::{
    catalog::v1::{ModelImageDetail, ModelImageMediaType},
    inference::v1::{ChatImageContent, ChatImageMediaType, ChatRequest, chat_content_part},
};
use image::{ImageFormat, ImageReader, Limits};

use crate::model::resolved::ResolvedModelImageInputCapacity;

macro_rules! image_error {
    ($variant:ident, $message:expr $(,)?) => {
        ImageValidationError::$variant(anyhow!($message))
    };
}

/// Decodes one strict public image data URL into private typed image data.
pub(crate) fn decode_data_url(
    value: &str,
    detail: &str,
) -> Result<ChatImageContent, ImageValidationError> {
    if detail != "auto" {
        return Err(image_error!(
            InvalidRequest,
            format!("detail {detail:?} must be \"auto\"")
        ));
    }
    let (prefix, encoded) = value.split_once(',').ok_or_else(|| {
        image_error!(
            InvalidRequest,
            "URL has no data URL metadata separator".to_owned()
        )
    })?;
    let media_type = match prefix {
        "data:image/jpeg;base64" => ChatImageMediaType::Jpeg,
        "data:image/png;base64" => ChatImageMediaType::Png,
        _ => {
            return Err(image_error!(
                InvalidRequest,
                format!("URL prefix {prefix:?} is not a supported JPEG or PNG base64 data URL")
            ));
        }
    };
    let encoded_data = STANDARD.decode(encoded)?;
    if encoded.is_empty() {
        return Err(image_error!(
            InvalidData,
            "encoded payload is empty".to_owned(),
        ));
    }
    if STANDARD.encode(&encoded_data) != encoded {
        return Err(image_error!(
            InvalidData,
            "encoded payload is not canonical standard base64".to_owned(),
        ));
    }
    if !matches_image_magic(&encoded_data, media_type) {
        return Err(image_error!(
            InvalidData,
            format!("{media_type:?} MIME type does not match payload magic bytes")
        ));
    }
    Ok(ChatImageContent {
        media_type: media_type as i32,
        encoded_data,
    })
}

/// Enforces the selected deployment snapshot image capacity before adapter invocation.
pub(crate) fn validate_image_input_capacity(
    request: &ChatRequest,
    capacity: Option<&ResolvedModelImageInputCapacity>,
) -> Result<(), ImageValidationError> {
    let images = request
        .messages
        .iter()
        .flat_map(|message| &message.content_parts)
        .filter_map(|part| match &part.content {
            Some(chat_content_part::Content::ImageContent(image)) => Some(image),
            _ => None,
        })
        .collect::<Vec<_>>();
    if images.is_empty() {
        return Ok(());
    }
    let capacity = capacity.ok_or_else(|| {
        image_error!(
            UnsupportedByModel,
            "selected model has no image input capacity".to_owned(),
        )
    })?;
    if images.len() > capacity.maximum_image_count as usize {
        return Err(image_error!(
            CapacityExceeded,
            format!(
                "request has {} images; maximum is {}",
                images.len(),
                capacity.maximum_image_count
            )
        ));
    }
    let fixed_detail = ModelImageDetail::try_from(capacity.fixed_detail)?;
    if fixed_detail != ModelImageDetail::Auto {
        return Err(image_error!(
            InvalidModelConfiguration,
            format!("fixed detail is {fixed_detail:?}; expected Auto")
        ));
    }
    for image in images {
        let media_type = ChatImageMediaType::try_from(image.media_type)?;
        let catalog_media_type = match media_type {
            ChatImageMediaType::Jpeg => ModelImageMediaType::Jpeg,
            ChatImageMediaType::Png => ModelImageMediaType::Png,
            ChatImageMediaType::Unspecified => {
                return Err(image_error!(
                    InvalidData,
                    "image media type is Unspecified".to_owned(),
                ));
            }
        };
        if !capacity
            .supported_media_types
            .contains(&(catalog_media_type as i32))
        {
            return Err(image_error!(
                UnsupportedByModel,
                format!(
                    "media type {catalog_media_type:?} is not in the selected model supported media types"
                )
            ));
        }
        if image.encoded_data.len() as u64 > capacity.maximum_encoded_bytes {
            return Err(image_error!(
                CapacityExceeded,
                format!(
                    "encoded payload has {} bytes; maximum is {}",
                    image.encoded_data.len(),
                    capacity.maximum_encoded_bytes
                )
            ));
        }
        let decoded_bytes = decode_image(&image.encoded_data, media_type, capacity)?;
        if decoded_bytes > capacity.maximum_decoded_bytes {
            return Err(image_error!(
                CapacityExceeded,
                format!(
                    "decoded image has {decoded_bytes} bytes; maximum is {}",
                    capacity.maximum_decoded_bytes
                )
            ));
        }
    }
    Ok(())
}

/// Enforces the fixed image capacity for an image-capable upstream target.
pub(crate) fn validate_upstream_image_input_capacity(
    request: &ChatRequest,
) -> Result<(), ImageValidationError> {
    let capacity = upstream_image_input_capacity();
    validate_image_input_capacity(request, Some(&capacity))
}

/// A public image input that cannot be decoded or admitted by a model.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ImageValidationError {
    #[error("Base64 image decode error: {0}")]
    Base64Decode(#[from] base64::DecodeError),

    #[error("Image decode error: {0}")]
    ImageDecode(#[from] image::ImageError),

    #[error("Image protobuf enum error: {0}")]
    ProtobufEnum(#[from] prost::UnknownEnumValue),

    #[error("Invalid image request: {0}")]
    InvalidRequest(#[source] anyhow::Error),

    #[error("Invalid image data: {0}")]
    InvalidData(#[source] anyhow::Error),

    #[error("Image unsupported by selected model: {0}")]
    UnsupportedByModel(#[source] anyhow::Error),

    #[error("Image exceeds selected model capacity: {0}")]
    CapacityExceeded(#[source] anyhow::Error),

    #[error("Invalid selected model image configuration: {0}")]
    InvalidModelConfiguration(#[source] anyhow::Error),

    #[error("Truncated image data: {0}")]
    TruncatedData(#[source] anyhow::Error),

    #[error("Image dimension arithmetic error: {0}")]
    ArithmeticOverflow(#[source] anyhow::Error),
}

fn upstream_image_input_capacity() -> ResolvedModelImageInputCapacity {
    ResolvedModelImageInputCapacity {
        maximum_image_count: 1,
        supported_media_types: vec![
            ModelImageMediaType::Jpeg as i32,
            ModelImageMediaType::Png as i32,
        ],
        maximum_encoded_bytes: 10 * 1024 * 1024,
        maximum_decoded_bytes: 64 * 1024 * 1024,
        maximum_width: 4096,
        maximum_height: 4096,
        maximum_pixels: 16_777_216,
        fixed_visual_tokens: 1,
        fixed_detail: ModelImageDetail::Auto as i32,
    }
}

fn matches_image_magic(bytes: &[u8], media_type: ChatImageMediaType) -> bool {
    match media_type {
        ChatImageMediaType::Jpeg => bytes.starts_with(b"\xff\xd8"),
        ChatImageMediaType::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        ChatImageMediaType::Unspecified => false,
    }
}

/// Decodes one admitted image and returns its exact decoded byte length.
///
/// The dimensions come from the image header, so an image beyond a declared dimension or pixel
/// budget is rejected before its decoded buffer is allocated, and a decode stays inside the
/// declared decoded-byte budget.
fn decode_image(
    bytes: &[u8],
    media_type: ChatImageMediaType,
    capacity: &ResolvedModelImageInputCapacity,
) -> Result<u64, ImageValidationError> {
    if !has_terminal_marker(bytes, media_type) {
        return Err(image_error!(
            TruncatedData,
            format!("{media_type:?} payload has no terminal marker")
        ));
    }
    let format = match media_type {
        ChatImageMediaType::Jpeg => ImageFormat::Jpeg,
        ChatImageMediaType::Png => ImageFormat::Png,
        ChatImageMediaType::Unspecified => {
            return Err(image_error!(
                InvalidData,
                "image media type is Unspecified".to_owned(),
            ));
        }
    };
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format).into_dimensions()?;
    if u64::from(width) > u64::from(capacity.maximum_width)
        || u64::from(height) > u64::from(capacity.maximum_height)
    {
        return Err(image_error!(
            CapacityExceeded,
            format!(
                "image is {width}x{height}; maxima are {}x{}",
                capacity.maximum_width, capacity.maximum_height
            )
        ));
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| {
            image_error!(
                ArithmeticOverflow,
                format!("image dimensions {width}x{height} overflow the pixel count")
            )
        })?;
    if pixels > capacity.maximum_pixels {
        return Err(image_error!(
            CapacityExceeded,
            format!(
                "image has {pixels} pixels; maximum is {}",
                capacity.maximum_pixels
            )
        ));
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(capacity.maximum_width);
    limits.max_image_height = Some(capacity.maximum_height);
    limits.max_alloc = Some(capacity.maximum_decoded_bytes);
    reader.limits(limits);
    let image = reader.decode().map_err(|error| match error {
        image::ImageError::Limits(_) => image_error!(
            CapacityExceeded,
            format!(
                "image {width}x{height} decodes beyond the {}-byte limit",
                capacity.maximum_decoded_bytes
            )
        ),
        error => ImageValidationError::ImageDecode(error),
    })?;
    Ok(image.as_bytes().len() as u64)
}

fn has_terminal_marker(bytes: &[u8], media_type: ChatImageMediaType) -> bool {
    match media_type {
        ChatImageMediaType::Jpeg => bytes.ends_with(b"\xff\xd9"),
        ChatImageMediaType::Png => bytes.ends_with(b"\x00\x00\x00\x00IEND\xaeB`\x82"),
        ChatImageMediaType::Unspecified => false,
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use dotcore_proto::dotcore::inference::v1::{ChatContentPart, ChatMessage};
    use image::{DynamicImage, Rgb, RgbImage};

    use super::*;

    #[test]
    fn accepts_complete_jpeg_and_png_images() {
        let jpeg = data_url(ChatImageMediaType::Jpeg, image(1, 1, ImageFormat::Jpeg));
        assert_eq!(
            decode_data_url(&jpeg, "auto").expect("JPEG").media_type,
            ChatImageMediaType::Jpeg as i32
        );
        let png = data_url(ChatImageMediaType::Png, image(1, 1, ImageFormat::Png));
        assert_eq!(
            decode_data_url(&png, "auto").expect("PNG").media_type,
            ChatImageMediaType::Png as i32
        );
    }

    #[test]
    fn malformed_base64_preserves_its_typed_decode_source() {
        let error = decode_data_url("data:image/png;base64,not base64", "auto")
            .expect_err("invalid base64");

        let ImageValidationError::Base64Decode(source) = &error else {
            panic!("expected Base64 decode error, got {error:?}");
        };
        assert_eq!(
            error.to_string(),
            format!("Base64 image decode error: {source}")
        );
        assert!(
            error
                .source()
                .is_some_and(|value| value.downcast_ref::<base64::DecodeError>().is_some())
        );
    }

    #[test]
    fn policy_failures_include_exact_category_and_context() {
        let detail = decode_data_url("data:image/png;base64,AA==", "low")
            .expect_err("unsupported image detail");
        assert!(matches!(detail, ImageValidationError::InvalidRequest(_)));
        assert_eq!(
            detail.to_string(),
            "Invalid image request: detail \"low\" must be \"auto\""
        );
        assert!(detail.source().is_some());

        let mismatch =
            decode_data_url("data:image/png;base64,AA==", "auto").expect_err("MIME mismatch");
        assert!(matches!(mismatch, ImageValidationError::InvalidData(_)));
        assert_eq!(
            mismatch.to_string(),
            "Invalid image data: Png MIME type does not match payload magic bytes"
        );
        assert!(mismatch.source().is_some());
    }

    #[test]
    fn invalid_protobuf_media_type_preserves_its_typed_source() {
        let error = validate_image_input_capacity(
            &request(vec![image_part_value(99, vec![0])]),
            Some(&capacity()),
        )
        .expect_err("unknown image media type");

        let ImageValidationError::ProtobufEnum(source) = &error else {
            panic!("expected protobuf enum error, got {error:?}");
        };
        assert_eq!(
            error.to_string(),
            "Image protobuf enum error: unknown enumeration value 99"
        );
        assert_eq!(source.to_string(), "unknown enumeration value 99");
        assert!(
            error
                .source()
                .is_some_and(|value| value.downcast_ref::<prost::UnknownEnumValue>().is_some())
        );
    }

    #[test]
    fn rejects_truncated_and_structurally_invalid_jpeg_and_png_images() {
        for (media_type, format) in [
            (ChatImageMediaType::Jpeg, ImageFormat::Jpeg),
            (ChatImageMediaType::Png, ImageFormat::Png),
        ] {
            let valid = image(2, 2, format);
            let truncated = valid[..valid.len() - 1].to_vec();
            let error = validate_image_input_capacity(
                &request(vec![image_part(media_type, truncated)]),
                Some(&capacity()),
            )
            .expect_err("truncated image");
            assert!(matches!(error, ImageValidationError::TruncatedData(_)));
            assert_eq!(
                error.to_string(),
                format!("Truncated image data: {media_type:?} payload has no terminal marker")
            );
            assert!(error.source().is_some());

            let mut invalid = valid;
            match media_type {
                ChatImageMediaType::Jpeg => invalid = vec![0xff, 0xd8, 0, 0, 0xff, 0xd9],
                ChatImageMediaType::Png => invalid[29] ^= 0xff,
                ChatImageMediaType::Unspecified => unreachable!(),
            }
            let error = validate_image_input_capacity(
                &request(vec![image_part(media_type, invalid)]),
                Some(&capacity()),
            )
            .expect_err("structurally invalid image");
            let ImageValidationError::ImageDecode(source) = &error else {
                panic!("expected image decode error, got {error:?}");
            };
            assert_eq!(error.to_string(), format!("Image decode error: {source}"));
            assert!(
                error
                    .source()
                    .is_some_and(|value| value.downcast_ref::<image::ImageError>().is_some())
            );
        }
    }

    #[test]
    fn model_support_configuration_and_capacity_are_distinct() {
        let part = image_part(ChatImageMediaType::Png, image(1, 1, ImageFormat::Png));

        let unsupported = validate_image_input_capacity(&request(vec![part.clone()]), None)
            .expect_err("Model without image support");
        assert!(matches!(
            unsupported,
            ImageValidationError::UnsupportedByModel(_)
        ));
        assert_eq!(
            unsupported.to_string(),
            "Image unsupported by selected model: selected model has no image input capacity"
        );

        let mut invalid_configuration = capacity();
        invalid_configuration.fixed_detail = ModelImageDetail::Unspecified as i32;
        let invalid_configuration = validate_image_input_capacity(
            &request(vec![part.clone()]),
            Some(&invalid_configuration),
        )
        .expect_err("invalid model image configuration");
        assert!(matches!(
            invalid_configuration,
            ImageValidationError::InvalidModelConfiguration(_)
        ));
        assert_eq!(
            invalid_configuration.to_string(),
            "Invalid selected model image configuration: fixed detail is Unspecified; expected Auto"
        );

        let mut limited = capacity();
        limited.maximum_image_count = 0;
        let capacity_error = validate_image_input_capacity(&request(vec![part]), Some(&limited))
            .expect_err("image count capacity");
        assert!(matches!(
            capacity_error,
            ImageValidationError::CapacityExceeded(_)
        ));
        assert_eq!(
            capacity_error.to_string(),
            "Image exceeds selected model capacity: request has 1 images; maximum is 0"
        );
    }

    #[test]
    fn upstream_image_policy_has_fixed_bounds_and_reuses_capacity_admission() {
        let capacity = upstream_image_input_capacity();
        assert_eq!(capacity.maximum_image_count, 1);
        assert_eq!(
            capacity.supported_media_types,
            vec![
                ModelImageMediaType::Jpeg as i32,
                ModelImageMediaType::Png as i32,
            ]
        );
        assert_eq!(capacity.maximum_encoded_bytes, 10 * 1024 * 1024);
        assert_eq!(capacity.maximum_width, 4096);
        assert_eq!(capacity.maximum_height, 4096);
        assert_eq!(capacity.maximum_pixels, 16_777_216);
        assert_eq!(capacity.maximum_decoded_bytes, 64 * 1024 * 1024);

        let encoded = image(1, 1, ImageFormat::Png);
        let count_error = validate_upstream_image_input_capacity(&request(vec![
            image_part(ChatImageMediaType::Png, encoded.clone()),
            image_part(ChatImageMediaType::Png, encoded),
        ]))
        .expect_err("more than one upstream image");
        assert!(matches!(
            count_error,
            ImageValidationError::CapacityExceeded(_)
        ));

        let encoded_error = validate_upstream_image_input_capacity(&request(vec![image_part(
            ChatImageMediaType::Png,
            vec![0; 10 * 1024 * 1024 + 1],
        )]))
        .expect_err("oversized upstream image");
        assert!(matches!(
            encoded_error,
            ImageValidationError::CapacityExceeded(_)
        ));
    }

    #[test]
    fn enforces_each_declared_image_capacity_boundary() {
        let encoded = image(1, 1, ImageFormat::Png);
        let part = image_part(ChatImageMediaType::Png, encoded.clone());
        let mut limits = capacity();

        limits.maximum_image_count = 1;
        assert!(
            validate_image_input_capacity(
                &request(vec![part.clone(), part.clone()]),
                Some(&limits)
            )
            .is_err()
        );

        limits.maximum_encoded_bytes = encoded.len() as u64 - 1;
        assert!(
            validate_image_input_capacity(&request(vec![part.clone()]), Some(&limits)).is_err()
        );

        limits = capacity();
        limits.maximum_decoded_bytes = 2;
        assert!(
            validate_image_input_capacity(&request(vec![part.clone()]), Some(&limits)).is_err()
        );

        limits = capacity();
        limits.maximum_width = 1;
        assert!(
            validate_image_input_capacity(
                &request(vec![image_part(
                    ChatImageMediaType::Png,
                    image(2, 1, ImageFormat::Png)
                )]),
                Some(&limits)
            )
            .is_err()
        );

        limits = capacity();
        limits.maximum_height = 1;
        assert!(
            validate_image_input_capacity(
                &request(vec![image_part(
                    ChatImageMediaType::Png,
                    image(1, 2, ImageFormat::Png)
                )]),
                Some(&limits)
            )
            .is_err()
        );

        limits = capacity();
        limits.maximum_pixels = 3;
        assert!(
            validate_image_input_capacity(
                &request(vec![image_part(
                    ChatImageMediaType::Png,
                    image(2, 2, ImageFormat::Png)
                )]),
                Some(&limits)
            )
            .is_err()
        );
    }

    #[test]
    fn oversized_images_are_rejected_before_their_buffer_is_allocated() {
        let part = image_part(ChatImageMediaType::Png, image(2, 2, ImageFormat::Png));

        let mut pixels = capacity();
        pixels.maximum_pixels = 3;
        let error = validate_image_input_capacity(&request(vec![part.clone()]), Some(&pixels))
            .expect_err("image over the pixel budget");
        assert!(matches!(error, ImageValidationError::CapacityExceeded(_)));
        assert_eq!(
            error.to_string(),
            "Image exceeds selected model capacity: image has 4 pixels; maximum is 3"
        );

        let mut width = capacity();
        width.maximum_width = 1;
        let error = validate_image_input_capacity(&request(vec![part.clone()]), Some(&width))
            .expect_err("image over the width budget");
        assert_eq!(
            error.to_string(),
            "Image exceeds selected model capacity: image is 2x2; maxima are 1x64"
        );

        let mut bytes = capacity();
        bytes.maximum_decoded_bytes = 8;
        let error = validate_image_input_capacity(&request(vec![part]), Some(&bytes))
            .expect_err("image over the decoded byte budget");
        assert!(matches!(error, ImageValidationError::CapacityExceeded(_)));
        assert_eq!(
            error.to_string(),
            "Image exceeds selected model capacity: image 2x2 decodes beyond the 8-byte limit"
        );
    }

    fn capacity() -> ResolvedModelImageInputCapacity {
        ResolvedModelImageInputCapacity {
            maximum_image_count: 1,
            supported_media_types: vec![
                ModelImageMediaType::Png as i32,
                ModelImageMediaType::Jpeg as i32,
            ],
            maximum_encoded_bytes: u64::MAX,
            maximum_decoded_bytes: 4096,
            maximum_width: 64,
            maximum_height: 64,
            maximum_pixels: 4096,
            fixed_visual_tokens: 1,
            fixed_detail: ModelImageDetail::Auto as i32,
        }
    }

    fn request(content_parts: Vec<ChatContentPart>) -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage {
                content_parts,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn image_part(media_type: ChatImageMediaType, encoded_data: Vec<u8>) -> ChatContentPart {
        image_part_value(media_type as i32, encoded_data)
    }

    fn image_part_value(media_type: i32, encoded_data: Vec<u8>) -> ChatContentPart {
        ChatContentPart {
            content: Some(chat_content_part::Content::ImageContent(ChatImageContent {
                media_type,
                encoded_data,
            })),
        }
    }

    fn image(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, Rgb([0, 0, 0])));
        let mut encoded = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut encoded), format)
            .expect("test image");
        encoded
    }

    fn data_url(media_type: ChatImageMediaType, encoded: Vec<u8>) -> String {
        let media_type = match media_type {
            ChatImageMediaType::Jpeg => "image/jpeg",
            ChatImageMediaType::Png => "image/png",
            ChatImageMediaType::Unspecified => unreachable!(),
        };
        format!("data:{media_type};base64,{}", STANDARD.encode(encoded))
    }
}
