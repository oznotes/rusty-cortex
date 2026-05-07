use thiserror::Error;

#[derive(Debug, Error)]
pub enum FlashError {
    #[error("No device found")]
    NoDevice,

    #[error("Device disconnected during operation")]
    DeviceDisconnected,

    #[error("USB error: {0}")]
    Usb(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl PartialEq for FlashError {
    fn eq(&self, other: &Self) -> bool {
        // Compare by Display output — sufficient for tests and error reporting.
        self.to_string() == other.to_string()
    }
}

impl serde::Serialize for FlashError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
