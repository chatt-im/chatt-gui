use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) const MAX_QUEUED_FILES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueuedFile {
    pub id: u64,
    pub path: PathBuf,
    pub file_name: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FileInspection {
    pub accepted: Vec<InspectedFile>,
    pub rejected: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InspectedFile {
    path: PathBuf,
    file_name: String,
}

#[derive(Debug)]
pub(crate) struct FileQueue {
    next_id: u64,
    files: Vec<QueuedFile>,
}

impl Default for FileQueue {
    fn default() -> Self {
        Self {
            next_id: 1,
            files: Vec::new(),
        }
    }
}

impl FileQueue {
    pub fn files(&self) -> &[QueuedFile] {
        &self.files
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn extend(&mut self, files: Vec<InspectedFile>) {
        for file in files {
            let id = self.next_id.max(1);
            self.next_id = id.wrapping_add(1).max(1);
            self.files.push(QueuedFile {
                id,
                path: file.path,
                file_name: file.file_name,
            });
        }
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.files.len();
        self.files.retain(|file| file.id != id);
        self.files.len() != before
    }

    pub fn take_all(&mut self) -> Vec<QueuedFile> {
        std::mem::take(&mut self.files)
    }

    pub fn restore(&mut self, files: impl IntoIterator<Item = QueuedFile>) {
        self.files.extend(files);
        self.files.sort_by_key(|file| file.id);
    }
}

pub(crate) fn inspect_files(
    mut paths: Vec<PathBuf>,
    max_upload_bytes: u64,
    available_slots: usize,
) -> FileInspection {
    let mut result = FileInspection::default();
    let available_slots = available_slots.min(MAX_QUEUED_FILES);
    let mut omitted = paths.len().saturating_sub(MAX_QUEUED_FILES);
    paths.truncate(MAX_QUEUED_FILES);
    for path in paths {
        if result.accepted.len() == available_slots {
            omitted += 1;
            continue;
        }
        match inspect_file(&path, max_upload_bytes) {
            Ok(file_name) => result.accepted.push(InspectedFile { path, file_name }),
            Err(error) => result.rejected.push(error),
        }
    }
    if omitted != 0 {
        result.rejected.push(format!(
            "{omitted} {} not queued; at most {MAX_QUEUED_FILES} files can be queued",
            if omitted == 1 {
                "file was"
            } else {
                "files were"
            }
        ));
    }
    result
}

fn inspect_file(path: &Path, max_upload_bytes: u64) -> Result<String, String> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "upload".into());
    let metadata =
        fs::metadata(path).map_err(|error| format!("{file_name} could not be read · {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{file_name} is not a regular file"));
    }
    if metadata.len() > max_upload_bytes {
        return Err(format!(
            "{file_name} is {} bytes; the upload limit is {max_upload_bytes} bytes",
            metadata.len()
        ));
    }
    Ok(file_name)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn queues_valid_files_in_order_and_keeps_duplicate_paths_independent() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.txt");
        let second = directory.path().join("second.txt");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();
        let mut queue = FileQueue::default();

        let result = inspect_files(vec![first.clone(), second, first], 1024, MAX_QUEUED_FILES);
        let accepted = result.accepted.len();
        queue.extend(result.accepted);

        assert_eq!(accepted, 3);
        assert!(result.rejected.is_empty());
        assert_eq!(
            queue
                .files()
                .iter()
                .map(|file| file.file_name.as_str())
                .collect::<Vec<_>>(),
            vec!["first.txt", "second.txt", "first.txt"]
        );
        assert_ne!(queue.files()[0].id, queue.files()[2].id);
    }

    #[test]
    fn rejects_invalid_entries_without_dropping_valid_siblings() {
        let directory = tempfile::tempdir().unwrap();
        let valid = directory.path().join("valid.txt");
        let oversized = directory.path().join("oversized.txt");
        fs::write(&valid, b"ok").unwrap();
        fs::write(&oversized, b"too large").unwrap();
        let missing = directory.path().join("missing.txt");
        let mut queue = FileQueue::default();

        let result = inspect_files(
            vec![
                directory.path().to_path_buf(),
                valid.clone(),
                oversized,
                missing,
            ],
            2,
            MAX_QUEUED_FILES,
        );
        let accepted = result.accepted.len();
        queue.extend(result.accepted);

        assert_eq!(accepted, 1);
        assert_eq!(result.rejected.len(), 3);
        assert_eq!(queue.files()[0].path, valid);
    }

    #[test]
    fn removes_one_duplicate_and_restores_files_in_original_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("same.txt");
        fs::write(&path, b"same").unwrap();
        let mut queue = FileQueue::default();
        let inspected = inspect_files(
            vec![path.clone(), path.clone(), path],
            1024,
            MAX_QUEUED_FILES,
        );
        queue.extend(inspected.accepted);
        let removed_id = queue.files()[1].id;

        assert!(queue.remove(removed_id));
        assert_eq!(queue.len(), 2);
        let remaining = queue.take_all();
        queue.restore([remaining[1].clone(), remaining[0].clone()]);

        assert_eq!(
            queue.files().iter().map(|file| file.id).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn bounds_file_inspection_to_the_available_queue_slots() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.txt");
        let second = directory.path().join("second.txt");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();

        let result = inspect_files(vec![first, second], 1024, 1);

        assert_eq!(result.accepted.len(), 1);
        assert_eq!(result.rejected.len(), 1);
        assert!(result.rejected[0].contains("at most 128 files"));
    }
}
