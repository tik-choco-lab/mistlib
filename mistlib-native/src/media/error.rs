use thiserror::Error;

#[derive(Error, Debug)]
pub enum MediaError {
    #[error("Base64 decode error: {0}")]
    Base64Decode(#[from] base64::DecodeError),

    #[error("SDP parse error: {0}")]
    SdpParse(String),
}

pub type Result<T> = std::result::Result<T, MediaError>;
