use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct RotationState {
    path: PathBuf,
    max_bytes: u64,
    max_files: usize,
    bytes_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RotationPlan {
    active_path: PathBuf,
    steps: Vec<RotationStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RotationStep {
    Remove { path: PathBuf },
    Rename { from: PathBuf, to: PathBuf },
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

    pub(crate) fn path_for_generation(&self, generation: usize) -> PathBuf {
        if generation == 0 {
            self.path.clone()
        } else {
            PathBuf::from(format!("{}.{}", self.path.display(), generation))
        }
    }

    pub(crate) fn rotation_plan(&self) -> RotationPlan {
        let mut steps = Vec::new();
        for generation in (1..=self.max_files).rev() {
            let path = self.path_for_generation(generation);
            if generation == self.max_files {
                steps.push(RotationStep::Remove { path });
            } else {
                steps.push(RotationStep::Rename {
                    from: path,
                    to: self.path_for_generation(generation + 1),
                });
            }
        }
        steps.push(RotationStep::Rename {
            from: self.path_for_generation(0),
            to: self.path_for_generation(1),
        });

        RotationPlan {
            active_path: self.path.clone(),
            steps,
        }
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

impl RotationPlan {
    pub(crate) fn active_path(&self) -> &Path {
        &self.active_path
    }

    pub(crate) fn steps(&self) -> &[RotationStep] {
        &self.steps
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

    #[test]
    fn rotation_plan_removes_oldest_then_renames_generations_and_active_file() {
        let state = RotationState::new(PathBuf::from("/tmp/service.log"), 8, 3, 9);
        assert_eq!(
            state.rotation_plan(),
            RotationPlan {
                active_path: PathBuf::from("/tmp/service.log"),
                steps: vec![
                    RotationStep::Remove {
                        path: PathBuf::from("/tmp/service.log.3")
                    },
                    RotationStep::Rename {
                        from: PathBuf::from("/tmp/service.log.2"),
                        to: PathBuf::from("/tmp/service.log.3")
                    },
                    RotationStep::Rename {
                        from: PathBuf::from("/tmp/service.log.1"),
                        to: PathBuf::from("/tmp/service.log.2")
                    },
                    RotationStep::Rename {
                        from: PathBuf::from("/tmp/service.log"),
                        to: PathBuf::from("/tmp/service.log.1")
                    }
                ],
            }
        );
    }
}
