//! Tests to ensure the functionality of the parser.
mod basic;
mod edge_cases;
mod formatting;
mod models;
mod places;

use std::{fmt, fs, path::PathBuf};

use rbx_dom_weak::{DomViewer, PropertyPool};

/// Runs several tests over the provided file.
pub fn test_suite(path: PathBuf) -> Result<(), Error> {
    // It would be difficult to run into a situation where this could panic
    let test_name = path
        .parent()
        .unwrap()
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap();

    let contents = fs::read(&path).map_err(|e| Error::new(e, "read"))?;

    let mut decoded = crate::from_reader_default(contents.as_slice())
        .map_err(|e| Error::new(e, "deserialize"))?;
    insta::assert_yaml_snapshot!(
        format!("{test_name}__decoded"),
        DomViewer::new().view_children(&decoded)
    );

    let mut encoded = Vec::new();
    crate::to_writer_default(&mut encoded, &decoded, decoded.root().children())
        .map_err(|e| Error::new(e, "serialize"))?;

    let mut pool = PropertyPool::default();
    decoded.share_properties(&mut pool);
    let mut shared_encoded = Vec::new();
    crate::to_writer_default(&mut shared_encoded, &decoded, decoded.root().children())
        .map_err(|e| Error::new(e, "serialize shared properties"))?;
    assert_eq!(shared_encoded, encoded);

    let encoded_text =
        std::str::from_utf8(&encoded).map_err(|e| Error::new(e, "display encoded"))?;
    insta::assert_snapshot!(format!("{test_name}__encoded"), encoded_text);

    let roundtrip =
        crate::from_reader_default(encoded.as_slice()).map_err(|e| Error::new(e, "roundtrip"))?;
    insta::assert_yaml_snapshot!(
        format!("{test_name}__roundtrip"),
        DomViewer::new().view_children(&roundtrip)
    );

    Ok(())
}

pub struct Error {
    inner: Box<dyn std::error::Error>,
    op: String,
}

// This isn't something that is normally a good idea, but panicking in errors
// tends to be very unhelpful because it doesn't explain the error.
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "could not {} file because {}",
            self.op, self.inner
        ))
    }
}

impl Error {
    pub fn new<E>(err: E, op: &str) -> Self
    where
        E: std::error::Error + 'static,
    {
        Self {
            inner: Box::from(err),
            op: op.to_string(),
        }
    }
}
