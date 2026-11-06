// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `Content-Type` an upload is sent with.

/// The media type for a file called `name`, from its extension (in any case);
/// `application/octet-stream` when the extension is missing or unknown.
pub fn content_type_for(name: &str) -> String {
    mime_guess::from_path(name)
        .first_or_octet_stream()
        .essence_str()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types() {
        assert_eq!(content_type_for("index.html"), "text/html");
        assert_eq!(content_type_for("a.webp"), "image/webp");
        assert_eq!(content_type_for("noext"), "application/octet-stream");
    }

    #[test]
    fn common_site_files_and_odd_names() {
        assert_eq!(content_type_for("style.css"), "text/css");
        assert_eq!(content_type_for("logo.svg"), "image/svg+xml");
        assert_eq!(content_type_for("PHOTO.JPG"), "image/jpeg");
        assert_eq!(content_type_for("font.woff2"), "font/woff2");
        assert_eq!(content_type_for(".htaccess"), "application/octet-stream");
        assert_eq!(
            content_type_for("archive.unknownext"),
            "application/octet-stream"
        );
        assert_eq!(content_type_for(""), "application/octet-stream");
    }
}
