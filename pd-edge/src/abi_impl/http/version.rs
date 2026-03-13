#![cfg_attr(not(feature = "http"), allow(dead_code))]

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum HttpVersionPreference {
    #[default]
    Auto,
    Http1,
    Http2,
    Http3,
}

impl HttpVersionPreference {
    pub(crate) fn parse(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "1" | "1.1" | "http/1.1" => Some(Self::Http1),
            "2" | "h2" | "http/2" => Some(Self::Http2),
            "3" | "h3" | "http/3" => Some(Self::Http3),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Http1 => "1.1",
            Self::Http2 => "2",
            Self::Http3 => "3",
        }
    }
}
