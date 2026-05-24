use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct RotationState {
    path: PathBuf,
    max_bytes: u64,
    max_files: usize,
    bytes_written: u64,
}

impl RotationState {
    pub(crate) fn new(path: PathBuf, max_bytes: u64, max_files: usize, bytes_written: u64) -> Self {
        Self {
            path,
            max_bytes: max_bytes.max(1),
            max_files,
            bytes_written,
        }
    }

    pub(crate) fn active_path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn path_for_generation(&self, generation: usize) -> PathBuf {
        if generation == 0 {
            self.path.clone()
        } else {
            PathBuf::from(format!("{}.{}", self.path.display(), generation))
        }
    }

    pub(crate) fn max_files(&self) -> usize {
        self.max_files
    }

    pub(crate) fn should_rotate(&self, incoming: usize) -> bool {
        self.max_files != 0 && self.bytes_written + incoming as u64 > self.max_bytes
    }

    pub(crate) fn record_write(&mut self, written: usize) {
        self.bytes_written += written as u64;
    }

    pub(crate) fn reset_after_rotation(&mut self) {
        self.bytes_written = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_state_rotates_only_after_threshold() {
        let mut state = RotationState::new(PathBuf::from("/tmp/service.log"), 8, 2, 0);
        assert!(!state.should_rotate(8));
        state.record_write(8);
        assert!(state.should_rotate(1));
    }

    #[test]
    fn rotation_state_can_disable_rotation_and_clamps_max_bytes() {
        let mut state = RotationState::new(PathBuf::from("/tmp/service.log"), 0, 0, 0);
        state.record_write(100);
        assert!(!state.should_rotate(100));

        let state = RotationState::new(PathBuf::from("/tmp/service.log"), 0, 1, 1);
        assert!(state.should_rotate(1));
    }

    #[test]
    fn generation_paths_append_generation_to_active_path() {
        let state = RotationState::new(PathBuf::from("/tmp/service.log"), 8, 2, 0);
        assert_eq!(
            state.path_for_generation(0),
            PathBuf::from("/tmp/service.log")
        );
        assert_eq!(
            state.path_for_generation(1),
            PathBuf::from("/tmp/service.log.1")
        );
    }
}
