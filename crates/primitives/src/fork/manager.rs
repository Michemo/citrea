use std::collections::VecDeque;

use sov_rollup_interface::spec::SpecId;
#[cfg(feature = "native")]
use tracing::info;

use super::ForkMigration;

/// Defines the interface which a fork manager needs to implement.
pub trait Fork {
    /// Returns the currently active fork.
    fn active_fork(&self) -> SpecId;

    /// Register a new L2 block with fork manager
    fn register_block(&mut self, height: u64) -> anyhow::Result<()>;
}

pub type SpecActivationBlockHeight = u64;

pub struct ForkManager {
    active_spec: SpecId,
    specs: VecDeque<(SpecId, SpecActivationBlockHeight)>,
    migration_handlers: Vec<Box<dyn ForkMigration + Sync + Send>>,
}

impl ForkManager {
    pub fn new(
        current_l2_height: u64,
        active_spec: SpecId,
        mut specs: Vec<(SpecId, SpecActivationBlockHeight)>,
    ) -> Self {
        // Filter out specs which have already been activated.
        specs.retain(|(spec, block)| *spec != active_spec && *block > current_l2_height);
        // Make sure the list of specs is sorted by the block number at which they activate.
        specs.sort_by_key(|(_, block_number)| *block_number);
        Self {
            specs: specs.into(),
            active_spec,
            migration_handlers: vec![],
        }
    }

    pub fn register_handler(&mut self, handler: Box<dyn ForkMigration + Sync + Send>) {
        self.migration_handlers.push(handler);
    }
}

impl Fork for ForkManager {
    fn active_fork(&self) -> SpecId {
        self.active_spec
    }

    fn register_block(&mut self, height: u64) -> anyhow::Result<()> {
        if let Some((new_spec, activation_block_height)) = self.specs.front() {
            if height == *activation_block_height {
                #[cfg(feature = "native")]
                info!("Activating fork {:?} at height: {}", *new_spec, height);

                self.active_spec = *new_spec;
                for handler in self.migration_handlers.iter() {
                    handler.spec_activated(self.active_spec)?;
                }
                self.specs.pop_front();
            }
        }
        Ok(())
    }
}

/// Simple search for the fork to which a specific block number blongs.
/// This assumes that the list of forks is sorted by block number in ascending fashion.
pub fn fork_from_block_number(forks: &[(SpecId, u64)], block_number: u64) -> SpecId {
    let mut fork = forks[0].0;
    if forks.len() == 1 {
        return fork;
    }
    for (spec_id, activation_block) in &forks[1..] {
        if block_number >= *activation_block {
            fork = *spec_id;
        }
    }
    fork
}
