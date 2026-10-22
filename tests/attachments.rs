use base64::{engine::general_purpose::STANDARD, Engine};
use remodex_connect_native::attachments::{self, stage};
use serde_json::json;
fn upload(kind: &str, name: &str, mime: &str, bytes: &[u8]) -> serde_json::Value {
    json!({"type":kind,"name":name,"mimeType":mime,"sizeBytes":bytes.len(),"dataUrl":format!("data:{mime};base64,{}",STANDARD.encode(bytes))})
}
#[test]
fn uploads_are_private_durable_and_use_native_image_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let png = b"\x89PNG\r\n\x1a\nfixture";
    let rows = stage(
        dir.path(),
        json!([
            upload("image", "../../photo.png", "image/png", png),
            upload("file", "note.txt", "text/plain", b"FILE_MARKER")
        ]),
    )
    .unwrap();
    for row in &rows {
        let path = std::path::Path::new(row["path"].as_str().unwrap());
        assert!(path.starts_with(dir.path()));
        assert!(path.is_file());
        assert!(row.get("dataUrl").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    let message = json!({"text":"inspect","attachments":rows});
    let c = attachments::input(&message, true).unwrap();
    assert_eq!(c[1]["type"], "localImage");
    assert!(c[0]["text"].as_str().unwrap().contains("note.txt"));
    let a = attachments::input(&message, false).unwrap();
    assert_eq!(
        STANDARD
            .decode(a[1]["source"]["data"].as_str().unwrap())
            .unwrap(),
        png
    );
    assert!(!attachments::display_message(&message).contains("base64"));
    attachments::discard(&rows);
    assert!(!std::path::Path::new(rows[0]["path"].as_str().unwrap()).exists());
}
#[test]
fn invalid_uploads_roll_back_files_and_enforce_limits() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        upload("image", "fake.png", "image/png", b"not png"),
        json!({"type":"file","dataUrl":"data:text/plain;base64,???"}),
        json!({"type":"file","dataUrl":"data:text/plain;base64,eA==","sizeBytes":2}),
        json!({"type":"file","path":"/etc/passwd"}),
    ] {
        assert!(stage(
            dir.path(),
            json!([upload("file", "first.txt", "text/plain", b"valid"), bad])
        )
        .is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    assert!(stage(
        dir.path(),
        json!(vec![upload("file", "x", "text/plain", b"x"); 9])
    )
    .is_err());
    assert!(stage(
        dir.path(),
        json!([upload(
            "file",
            "huge",
            "text/plain",
            &vec![b'x'; attachments::MAX_FILE + 1]
        )])
    )
    .is_err());
}

#[test]
fn thumbnails_reject_invalid_and_oversized_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.png");
    std::fs::write(&path, b"\x89PNG\r\n\x1a\ninvalid").unwrap();
    assert!(remodex_connect_native::attachments::thumbnail(&path).is_err());
    // Wide but low byte-count image proves dimensions are checked before decode.
    image::DynamicImage::new_rgb8(17000, 1).save(&path).unwrap();
    assert!(remodex_connect_native::attachments::thumbnail(&path).is_err());
}
