//! Page snapshots published by the capture before the whole document has finished.
use super::*;
use std::fs::File;
use std::io::Read;

#[derive(Deserialize)]
struct Record {
    #[serde(default)]
    page: i64,
    #[serde(default)]
    units: Vec<CapturedUnit>,
    #[serde(default)]
    invalid: bool,
}

pub(crate) struct PageCapture {
    pub dl: DisplayList,
    pub units: Vec<CapturedUnit>,
}

pub(crate) enum PreviewUpdate {
    Page(Box<PageCapture>),
    Invalid,
    Tick,
}

pub(crate) struct PagePreviewReader {
    dir: PathBuf,
    job: String,
    file: Option<File>,
    pending: Vec<u8>,
}

impl PagePreviewReader {
    /// Clear the previous run's manifest before releasing this run. File timestamps cannot
    /// identify a run reliably when the wall clock changes.
    pub fn new(dir: &Path, job: &str) -> Result<Self> {
        let path = dir.join(format!("{job}.rtex-preview.jsonl"));
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(Self {
            dir: dir.into(),
            job: job.into(),
            file: None,
            pending: Vec::new(),
        })
    }

    /// A terminating newline is written after the page file is closed. Keep incomplete
    /// records until the writer finishes; a malformed preview never prevents a full layout.
    pub fn poll(&mut self, callback: &mut dyn FnMut(PreviewUpdate) -> bool) {
        if !callback(PreviewUpdate::Tick) {
            return;
        }
        if self.file.is_none() {
            self.file = File::open(self.dir.join(format!("{}.rtex-preview.jsonl", self.job))).ok();
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(e) = file.read_to_end(&mut self.pending) {
            log::warn!("reading page previews: {e}");
            return;
        }
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let update = (|| -> Result<PreviewUpdate> {
                let record: Record = serde_json::from_slice(&line)?;
                if record.invalid {
                    return Ok(PreviewUpdate::Invalid);
                }
                let path = self.dir.join(format!(
                    "{}.rtex-preview-page{}.json",
                    self.job, record.page
                ));
                let dl: DisplayList = serde_json::from_reader(File::open(path)?)?;
                Ok(PreviewUpdate::Page(Box::new(PageCapture {
                    dl,
                    units: record.units,
                })))
            })();
            match update {
                Ok(update) => {
                    if !callback(update) {
                        break;
                    }
                }
                Err(e) => log::warn!("ignoring page preview: {e:#}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reader_waits_for_a_complete_record_and_reports_invalidation() {
        let dir = std::env::temp_dir().join(format!("rtex-preview-reader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut reader = PagePreviewReader::new(&dir, "main").unwrap();
        let page = DisplayList {
            kind: "page".into(),
            page: Some(1),
            ..Default::default()
        };
        std::fs::write(
            dir.join("main.rtex-preview-page1.json"),
            serde_json::to_vec(&page).unwrap(),
        )
        .unwrap();
        let mut file = File::create(dir.join("main.rtex-preview.jsonl")).unwrap();
        file.write_all(b"{\"page\":1,\"units\":[]}").unwrap();
        file.flush().unwrap();
        let mut pages = 0;
        let mut invalid = false;
        reader.poll(&mut |event| {
            if matches!(event, PreviewUpdate::Page(_)) {
                pages += 1;
            }
            true
        });
        assert_eq!(pages, 0);
        file.write_all(b"\n{\"invalid\":true}\n").unwrap();
        file.flush().unwrap();
        reader.poll(&mut |event| {
            match event {
                PreviewUpdate::Page(_) => pages += 1,
                PreviewUpdate::Invalid => invalid = true,
                PreviewUpdate::Tick => {}
            }
            true
        });
        assert_eq!(pages, 1);
        assert!(invalid);
        drop(file);
        drop(reader);
        let _ = std::fs::remove_dir_all(dir);
    }
}
