//! Native macOS clipboard snapshots; no helper processes or history database.
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::{NSArray, NSString, NSURL, ns_string};

pub enum Content {
    Text(String),
    Files(Vec<std::path::PathBuf>),
    Image(Image),
    Unsupported,
}

pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
pub struct Image {
    pub name: &'static str,
    pub bytes: Vec<u8>,
}

fn read_image(board: &NSPasteboard) -> Content {
    // Prefer a phone-friendly encoded representation. Never fall through to
    // accompanying text when an image is present but cannot be read.
    for (uti, name) in [
        ("public.png", "Clipboard.png"),
        ("public.jpeg", "Clipboard.jpg"),
        ("com.compuserve.gif", "Clipboard.gif"),
        ("public.tiff", "Clipboard.tiff"),
    ] {
        if let Some(data) = board.dataForType(&NSString::from_str(uti)) {
            if (1..=MAX_IMAGE_BYTES).contains(&data.length()) {
                return Content::Image(Image {
                    name,
                    bytes: data.to_vec(),
                });
            }
        }
    }
    Content::Unsupported
}

fn read_from(board: &NSPasteboard) -> (isize, Content) {
    let count = board.changeCount();
    // Finder offers both file URLs and a plain-text filename. Check file
    // representations first, including promised files and legacy applications.
    #[allow(deprecated)]
    let file_types = NSArray::from_slice(&[
        unsafe { objc2_app_kit::NSPasteboardTypeFileURL },
        unsafe { objc2_app_kit::NSFilenamesPboardType },
        unsafe { objc2_app_kit::NSFilesPromisePboardType },
        ns_string!("com.apple.pasteboard.promised-file-url"),
        ns_string!("com.apple.pasteboard.promised-file-content-type"),
    ]);
    // Check every item before considering text. Resolve only concrete local
    // file URLs below; never materialize promised files or read file contents.
    let file_utis = NSArray::from_slice(&[
        unsafe { objc2_app_kit::NSPasteboardTypeFileURL },
        ns_string!("com.apple.pasteboard.promised-file-url"),
        ns_string!("com.apple.pasteboard.promised-file-content-type"),
    ]);
    let files = board.availableTypeFromArray(&file_types).is_some()
        || board.canReadItemWithDataConformingToTypes(&file_utis);
    let content = if files {
        // Read concrete file URLs only; promised files remain unsupported and
        // never fall back to the plain-text filename Finder also provides.
        let mut paths = Vec::new();
        if let Some(items) = board.pasteboardItems() {
            for index in 0..items.count().min(129) {
                let item = items.objectAtIndex(index);
                if let Some(value) =
                    item.stringForType(unsafe { objc2_app_kit::NSPasteboardTypeFileURL })
                {
                    if let Some(url) = NSURL::URLWithString(&value) {
                        if url.isFileURL()
                            && url
                                .host()
                                .is_none_or(|h| h.length() == 0 || h.to_string() == "localhost")
                        {
                            // Finder can provide file:///.file/id=... URLs.
                            // Foundation resolves these inode references;
                            // ordinary URL decoding produces an unusable path.
                            if let Some(path) = url.filePathURL().and_then(|u| u.path()) {
                                paths.push(std::path::PathBuf::from(path.to_string()));
                            }
                        }
                    }
                }
            }
        }
        Content::Files(paths)
    } else if board
        .canReadItemWithDataConformingToTypes(&NSArray::from_slice(&[ns_string!("public.image")]))
    {
        read_image(board)
    } else {
        board
            .stringForType(unsafe { NSPasteboardTypeString })
            .filter(|s| s.length() <= crate::message_center::MAX_INLINE_TEXT)
            .map(|s| Content::Text(s.to_string()))
            .unwrap_or(Content::Unsupported)
    };
    // A new copy between inspecting types and reading text invalidates the
    // snapshot. Retry on the next change-count check instead of leaking a name.
    (
        count,
        if board.changeCount() == count {
            content
        } else {
            Content::Unsupported
        },
    )
}

static ACCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn read_content() -> (isize, Content) {
    let _access = ACCESS.lock().unwrap();
    autoreleasepool(|_| read_from(&NSPasteboard::generalPasteboard()))
}

fn read_update_from(
    board: &NSPasteboard,
    previous: isize,
    initial: bool,
) -> Option<(isize, Content)> {
    if board.changeCount() == previous && !initial {
        return None;
    }
    let snapshot = read_from(board);
    // Reoffer an existing image on reconnect, without replaying old text.
    (snapshot.0 != previous || matches!(&snapshot.1, Content::Image(_))).then_some(snapshot)
}

pub fn read_update(previous: isize, initial: bool) -> Option<(isize, Content)> {
    let _access = ACCESS.lock().unwrap();
    autoreleasepool(|_| read_update_from(&NSPasteboard::generalPasteboard(), previous, initial))
}

pub fn read() -> (isize, Option<String>) {
    let (count, content) = read_content();
    (
        count,
        match content {
            Content::Text(text) => Some(text),
            _ => None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::NSData;
    struct TestBoard(objc2::rc::Retained<NSPasteboard>);
    impl Drop for TestBoard {
        fn drop(&mut self) {
            // SAFETY: This test owns a uniquely named temporary pasteboard.
            unsafe {
                let _: () = objc2::msg_send![&*self.0, releaseGlobally];
            }
        }
    }
    #[test]
    fn identical_peer_echo_does_not_trigger_another_clipboard_update() {
        autoreleasepool(|_| {
            let board = TestBoard(NSPasteboard::pasteboardWithUniqueName());
            let count = write_to(&board.0, "synthetic shared text").unwrap();
            assert_eq!(write_to(&board.0, "synthetic shared text").unwrap(), count);
            assert!(read_update_from(&board.0, count, false).is_none());
            let next = write_to(&board.0, "new shared text").unwrap();
            assert_ne!(next, count);
            assert!(
                matches!(read_update_from(&board.0, count, false), Some((_, Content::Text(text))) if text == "new shared text")
            );
        });
    }

    #[test]
    fn copied_image_overrides_text_and_is_reoffered_on_reconnect() {
        autoreleasepool(|_| {
            let mut png = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut png, 1, 1);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&[10, 20, 30, 255])
                    .unwrap();
            }
            let board = TestBoard(NSPasteboard::pasteboardWithUniqueName());
            assert!(
                board
                    .0
                    .setData_forType(Some(&NSData::with_bytes(&png)), ns_string!("public.png"))
            );
            assert!(
                board
                    .0
                    .setString_forType(ns_string!("old accompanying text"), unsafe {
                        NSPasteboardTypeString
                    })
            );
            let previous = board.0.changeCount();
            let (_, Content::Image(image)) = read_from(&board.0) else {
                panic!("Image became text")
            };
            assert_eq!(image.bytes, png);
            assert_eq!(image.name, "Clipboard.png");
            assert!(matches!(
                read_update_from(&board.0, previous, true),
                Some((_, Content::Image(_)))
            ));
            assert!(read_update_from(&board.0, previous, false).is_none());
            assert!(matches!(
                read_update_from(&board.0, previous - 1, false),
                Some((_, Content::Image(_)))
            ));

            board.0.clearContents();
            assert!(
                board
                    .0
                    .setString_forType(ns_string!("ordinary text"), unsafe {
                        NSPasteboardTypeString
                    })
            );
            assert!(read_update_from(&board.0, board.0.changeCount(), true).is_none());
            // A failed image read must never send accompanying text instead.
            assert!(
                board
                    .0
                    .setData_forType(Some(&NSData::with_bytes(&[])), ns_string!("public.png"))
            );
            assert!(matches!(read_from(&board.0).1, Content::Unsupported));
        });
    }
    #[test]
    fn finder_file_reference_is_resolved_after_file_rename() {
        autoreleasepool(|_| {
            let root = std::env::temp_dir().join(format!(
                "clip-url-{:016x}",
                u64::from_ne_bytes(crate::private::random().unwrap())
            ));
            std::fs::create_dir(&root).unwrap();
            let before = root.join("测试 original.bin");
            let after = root.join("moved.bin");
            std::fs::write(&before, b"synthetic file").unwrap();
            let url = NSURL::fileURLWithPath(&NSString::from_str(before.to_str().unwrap()));
            let reference = url.fileReferenceURL().unwrap().absoluteString().unwrap();
            std::fs::rename(&before, &after).unwrap();
            let board = TestBoard(NSPasteboard::pasteboardWithUniqueName());
            assert!(board.0.setString_forType(&reference, unsafe {
                objc2_app_kit::NSPasteboardTypeFileURL
            }));
            let Content::Files(paths) = read_from(&board.0).1 else {
                panic!("File URL became text");
            };
            assert_eq!(paths.len(), 1);
            assert_eq!(
                paths[0].canonicalize().unwrap(),
                after.canonicalize().unwrap()
            );
            std::fs::remove_dir_all(root).unwrap();
        });
    }
    #[test]
    fn native_file_representations_override_filename_text_without_touching_user_clipboard() {
        autoreleasepool(|_| {
            let board = TestBoard(NSPasteboard::pasteboardWithUniqueName());
            #[allow(deprecated)]
            let file_types = [
                unsafe { objc2_app_kit::NSPasteboardTypeFileURL },
                unsafe { objc2_app_kit::NSFilenamesPboardType },
                unsafe { objc2_app_kit::NSFilesPromisePboardType },
                ns_string!("com.apple.pasteboard.promised-file-url"),
            ];
            for (index, kind) in file_types.into_iter().enumerate() {
                board.0.clearContents();
                if matches!(index, 1 | 2) {
                    let values = NSArray::from_slice(&[ns_string!("/synthetic/report.pdf")]);
                    // SAFETY: An array of strings is a valid property list for
                    // the legacy filename/promise representation.
                    assert!(unsafe { board.0.setPropertyList_forType(&values, kind) });
                } else {
                    assert!(
                        board
                            .0
                            .setString_forType(ns_string!("file:///synthetic/report.pdf"), kind)
                    );
                }
                assert!(board.0.setString_forType(ns_string!("report.pdf"), unsafe {
                    NSPasteboardTypeString
                }));
                assert!(matches!(read_from(&board.0).1, Content::Files(_)));
            }
            // A file in a later item also blocks the first item's text fallback.
            {
                use objc2::runtime::ProtocolObject;
                use objc2_app_kit::NSPasteboardWriting;
                use objc2_foundation::NSURL;
                let file =
                    NSURL::URLWithString(ns_string!("file:///synthetic/report.pdf")).unwrap();
                let text: &ProtocolObject<dyn NSPasteboardWriting> =
                    ProtocolObject::from_ref(ns_string!("report.pdf"));
                let file: &ProtocolObject<dyn NSPasteboardWriting> =
                    ProtocolObject::from_ref(&*file);
                board.0.clearContents();
                assert!(board.0.writeObjects(&NSArray::from_slice(&[text, file])));
                assert!(
                    matches!(read_from(&board.0).1, Content::Files(paths) if paths == [std::path::PathBuf::from("/synthetic/report.pdf")])
                );
            }
            // Actual text remains text, even when it resembles a filename or URL.
            for text in [
                "report.pdf",
                "file:///synthetic/report.pdf",
                "/synthetic/report.pdf",
                "正常文本",
            ] {
                board.0.clearContents();
                assert!(
                    board
                        .0
                        .setString_forType(&NSString::from_str(text), unsafe {
                            NSPasteboardTypeString
                        })
                );
                assert!(matches!(read_from(&board.0).1, Content::Text(value) if value == text));
            }
        });
    }
}

pub fn change_count() -> isize {
    let _access = ACCESS.lock().unwrap();
    autoreleasepool(|_| NSPasteboard::generalPasteboard().changeCount())
}

pub fn write(text: &str) -> anyhow::Result<isize> {
    let _access = ACCESS.lock().unwrap();
    autoreleasepool(|_| write_to(&NSPasteboard::generalPasteboard(), text))
}

fn write_to(board: &NSPasteboard, text: &str) -> anyhow::Result<isize> {
    // Multiple peers may echo the same text. Preserve the change count so
    // their acknowledgements do not trigger another broadcast cycle.
    if board
        .stringForType(unsafe { NSPasteboardTypeString })
        .is_some_and(|s| s.to_string() == text)
        && matches!(read_from(board).1, Content::Text(_))
    {
        return Ok(board.changeCount());
    }
    board.clearContents();
    anyhow::ensure!(
        board.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString }),
        "macOS rejected text clipboard write"
    );
    Ok(board.changeCount())
}
