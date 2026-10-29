use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// Everything except the RFC 3986 unreserved characters (`A-Z a-z 0-9 - . _ ~`).
/// Non-ASCII bytes are always encoded by `percent_encoding`, in uppercase hex.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Why a string could not be used as (part of) a [`RemotePath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path segment must not be empty")]
    EmptySegment,
    #[error("path segment {0:?} must not contain '/'")]
    ContainsSeparator(String),
    #[error("path segment {0:?} is not allowed")]
    DotSegment(String),
}

/// A normalized, root-relative path on the remote storage: no leading or
/// trailing slash, no empty segments, no `.` or `..` segments. The root is the
/// empty path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemotePath(String);

impl RemotePath {
    pub fn root() -> Self {
        Self(String::new())
    }

    /// Splits on `/`, drops empty segments (so leading, trailing and doubled
    /// slashes are normalized away) and rejects `.` and `..`.
    pub fn parse(input: &str) -> Result<Self, PathError> {
        let mut segments = Vec::new();
        for segment in input.split('/').filter(|s| !s.is_empty()) {
            check_segment(segment)?;
            segments.push(segment);
        }
        Ok(Self(segments.join("/")))
    }

    /// Appends exactly one segment; `name` must not contain `/`.
    pub fn join(&self, name: &str) -> Result<Self, PathError> {
        check_segment(name)?;
        if self.is_root() {
            Ok(Self(name.to_owned()))
        } else {
            Ok(Self(format!("{}/{name}", self.0)))
        }
    }

    /// The path without its last segment; `None` for the root.
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        match self.0.rsplit_once('/') {
            Some((parent, _)) => Some(Self(parent.to_owned())),
            None => Some(Self::root()),
        }
    }

    /// The last segment; `None` for the root.
    pub fn name(&self) -> Option<&str> {
        if self.is_root() {
            None
        } else {
            Some(self.0.rsplit('/').next().unwrap_or(&self.0))
        }
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|s| !s.is_empty())
    }

    /// The path as it appears in a URL: every segment percent-encoded and
    /// joined with `/`, plus a trailing `/` when requested. The root is always
    /// the empty string.
    pub fn to_url_path(&self, trailing_slash: bool) -> String {
        let mut url = self
            .segments()
            .map(encode_segment)
            .collect::<Vec<_>>()
            .join("/");
        if trailing_slash && !url.is_empty() {
            url.push('/');
        }
        url
    }
}

/// Percent-encodes one path segment, keeping only RFC 3986 unreserved
/// characters (`A-Z a-z 0-9 - . _ ~`) and encoding every other byte of its
/// UTF-8 form in uppercase hex.
pub fn encode_segment(segment: &str) -> String {
    utf8_percent_encode(segment, SEGMENT).to_string()
}

fn check_segment(segment: &str) -> Result<(), PathError> {
    if segment.is_empty() {
        Err(PathError::EmptySegment)
    } else if segment.contains('/') {
        Err(PathError::ContainsSeparator(segment.to_owned()))
    } else if segment == "." || segment == ".." {
        Err(PathError::DotSegment(segment.to_owned()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normalizes_slashes() {
        assert_eq!(
            RemotePath::parse("/wp-content//uploads/").unwrap().as_str(),
            "wp-content/uploads"
        );
    }

    #[test]
    fn parse_rejects_dot_segments() {
        assert!(RemotePath::parse("a/../b").is_err());
        assert!(RemotePath::parse("./a").is_err());
    }

    #[test]
    fn root_properties() {
        let r = RemotePath::root();
        assert!(r.is_root());
        assert_eq!(r.parent(), None);
        assert_eq!(r.to_url_path(true), "");
    }

    #[test]
    fn join_parent_name() {
        let p = RemotePath::root()
            .join("wp-content")
            .unwrap()
            .join("uploads")
            .unwrap();
        assert_eq!(p.name(), Some("uploads"));
        assert_eq!(p.parent().unwrap().as_str(), "wp-content");
        assert!(RemotePath::root().join("a/b").is_err());
        assert!(RemotePath::root().join("..").is_err());
    }

    #[test]
    fn url_encoding_per_segment() {
        let p = RemotePath::parse("a/x y#?%+é").unwrap();
        assert_eq!(p.to_url_path(false), "a/x%20y%23%3F%25%2B%C3%A9");
        assert_eq!(p.to_url_path(true), "a/x%20y%23%3F%25%2B%C3%A9/");
    }

    #[test]
    fn unreserved_chars_untouched() {
        assert_eq!(
            RemotePath::parse("1-ge_bidi.120km~x")
                .unwrap()
                .to_url_path(false),
            "1-ge_bidi.120km~x"
        );
    }

    #[test]
    fn parent_of_single_segment_is_root() {
        let p = RemotePath::parse("uploads").unwrap();
        assert_eq!(p.parent(), Some(RemotePath::root()));
    }

    #[test]
    fn join_rejects_empty_name() {
        assert_eq!(RemotePath::root().join(""), Err(PathError::EmptySegment));
    }
}
